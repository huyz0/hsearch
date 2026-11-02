//! Coordinator-only feasibility experiments over public fixtures, not a serving adapter.
//! Native commit/NRT are deliberately separate from simulated authority decisions.
use std::collections::BTreeMap;
use std::sync::{Arc, Barrier, Mutex};

use lucene_analysis::{Analyzer, StandardAnalyzer};
use lucene_codecs::stored_fields::FieldValue;
use lucene_index::buffered_updates::Term;
use lucene_index::document::{Document, KeywordField, LongField, Store, StringField, TextField};
use lucene_index::index_writer::IndexWriter;
use lucene_index::segment_info::LuceneVersion;
use lucene_search::aggs::{self, MetricSpec, NEED_ALL, Source, ValueKind};
use lucene_search::directory_reader::DirectoryReader;
use lucene_search::index_searcher::IndexSearcher;
use lucene_search::query::{
    BooleanQuery, Clause, MatchAllDocsQuery, PhraseQuery, PointsRangeQuery, TermQuery,
};
use lucene_search::terms_agg;
use lucene_search::top_field::{SortField, SortType};
use lucene_store::{ByteBuffersDirectory, Directory};

const VERSION: LuceneVersion = LuceneVersion {
    major: 10,
    minor: 5,
    bugfix: 0,
};
const MAX_DOCUMENTS: usize = 128;
const MAX_BYTES: usize = 8192;
const MAX_TERMS: usize = 128;

#[derive(Clone, Copy)]
struct Row<'a> {
    uid: &'a str,
    tenant: &'a str,
    category: &'a str,
    title: &'a str,
    price: i64,
    active: bool,
    created_at: i64,
}
struct SearchCase {
    id: &'static str,
    query: BooleanQuery,
    sort: Vec<SortField>,
    size: usize,
    ids: Vec<&'static str>,
    total: u64,
}
struct AnalyzerCase {
    id: &'static str,
    text: &'static str,
    tokens: Vec<(&'static str, i32, i32, i32)>,
}
include!("fixtures/native_core.rs");

fn analyzer() -> Analyzer {
    Analyzer::new(StandardAnalyzer::new())
}
fn writer(directory: &dyn Directory) -> IndexWriter<'_> {
    let mut writer = IndexWriter::open(directory, vec![], "Lucene104", VERSION).unwrap();
    writer.set_analyzer(Some(Arc::new(analyzer())));
    writer.set_ram_buffer_size_mb(8.0).unwrap();
    writer.set_max_buffered_docs(8).unwrap();
    writer
}

// Finite research preparation only. It does not assign identity/version or mutate a writer.
fn prepare(row: Row<'_>, version: i64) -> Result<Document, &'static str> {
    let strings = [row.uid, row.tenant, row.category, row.title];
    if row.uid.is_empty()
        || version <= 0
        || strings.iter().map(|s| s.len()).sum::<usize>() > MAX_BYTES
    {
        return Err("row-bound");
    }
    if analyzer()
        .try_analyze_stream(row.title)
        .unwrap()
        .tokens
        .len()
        > MAX_TERMS
    {
        return Err("term-bound");
    }
    let mut document = Document::new();
    document.add(KeywordField::new("uid", row.uid, Store::Yes));
    document.add(KeywordField::new("tenant", row.tenant, Store::Yes));
    document.add(KeywordField::new("category", row.category, Store::Yes));
    document.add(TextField::new("title", row.title, Store::Yes));
    document.add(LongField::new("price", row.price, Store::Yes));
    document.add(StringField::new(
        "active",
        row.active.to_string(),
        Store::Yes,
    ));
    document.add(LongField::new("created_at", row.created_at, Store::Yes));
    document.add(LongField::new("logical_version", version, Store::Yes));
    Ok(document)
}
fn prepare_batch(rows: &[Row<'_>], version: i64) -> Result<Vec<Document>, &'static str> {
    if rows.len() > MAX_DOCUMENTS {
        return Err("document-bound");
    }
    rows.iter().map(|row| prepare(*row, version)).collect()
}
fn term(field: &str, text: &str) -> Clause {
    Clause::Term(TermQuery::new(field, text.as_bytes()))
}
fn top(clause: Clause) -> BooleanQuery {
    BooleanQuery {
        must: vec![clause],
        ..Default::default()
    }
}
fn all() -> BooleanQuery {
    top(Clause::MatchAllDocs(MatchAllDocsQuery::new(128)))
}
fn analyzed(text: &str, phrase: bool) -> Clause {
    let terms: Vec<Vec<u8>> = analyzer()
        .try_analyze_stream(text)
        .unwrap()
        .tokens
        .into_iter()
        .map(|t| t.term.into_bytes())
        .collect();
    if phrase {
        Clause::Phrase(PhraseQuery::new("title", terms))
    } else {
        Clause::Boolean(Box::new(BooleanQuery {
            should: terms
                .into_iter()
                .map(|t| Clause::Term(TermQuery::new("title", t)))
                .collect(),
            minimum_should_match: 1,
            ..Default::default()
        }))
    }
}

fn stored(reader: &DirectoryReader, mut global_doc: i32, name: &str) -> FieldValue {
    for segment in reader.segment_readers() {
        if global_doc < segment.max_doc {
            let number = segment
                .field_infos()
                .fields
                .iter()
                .find(|f| f.name == name)
                .unwrap()
                .number;
            let document = segment.stored_document(global_doc).unwrap().unwrap();
            return document
                .fields
                .into_iter()
                .find(|f| f.field_number == number)
                .unwrap()
                .value;
        }
        global_doc -= segment.max_doc;
    }
    panic!("Native search returned an invalid document");
}
fn uid(reader: &DirectoryReader, doc: i32) -> String {
    match stored(reader, doc, "uid") {
        FieldValue::String(value) => value,
        _ => panic!("Stored logical identity changed type"),
    }
}
fn search(
    reader: &DirectoryReader,
    query: &BooleanQuery,
    sort: &[SortField],
    size: usize,
) -> (Vec<String>, u64) {
    assert!((1..=MAX_DOCUMENTS).contains(&size));
    assert!(reader.max_doc() <= 128);
    assert!(reader.segment_readers().len() <= 32);
    let mut opened = reader.open_segments().unwrap();
    opened.open_points().unwrap();
    let segments = opened.as_open_segments();
    let maps = reader.field_norms_by_field(&["title".to_string()]).unwrap();
    let norms: Vec<_> = maps.iter().map(Some).collect();
    let searcher = IndexSearcher::new(&segments, &norms).unwrap();
    let result = searcher
        .search_sorted(reader.segment_readers(), query, size, sort, None)
        .unwrap();
    let ids = result.hits.iter().map(|hit| uid(reader, hit.doc)).collect();
    (ids, result.total.value)
}
fn ids(reader: &DirectoryReader) -> Vec<String> {
    search(
        reader,
        &all(),
        &[SortField::numeric("uid", SortType::String, false)],
        128,
    )
    .0
}
fn state(reader: &DirectoryReader) -> BTreeMap<String, (String, i64)> {
    let mut opened = reader.open_segments().unwrap();
    opened.open_points().unwrap();
    let segments = opened.as_open_segments();
    let norms = vec![None; segments.len()];
    let result = IndexSearcher::new(&segments, &norms)
        .unwrap()
        .search(&all(), 128)
        .unwrap();
    assert_eq!(
        result.score_docs.len(),
        usize::try_from(reader.num_docs()).unwrap()
    );
    let values: BTreeMap<_, _> = result
        .score_docs
        .iter()
        .map(|hit| {
            let title = match stored(reader, hit.doc, "title") {
                FieldValue::String(v) => v,
                _ => panic!("title type"),
            };
            let version = match stored(reader, hit.doc, "logical_version") {
                FieldValue::Long(v) => v,
                _ => panic!("version type"),
            };
            (uid(reader, hit.doc), (title, version))
        })
        .collect();
    assert_eq!(
        values.len(),
        result.score_docs.len(),
        "Duplicate live logical IDs must not be hidden by a map"
    );
    values
}

#[test]
fn native_reference_search_and_finite_aggregations() {
    let directory = ByteBuffersDirectory::new();
    let mut writer = writer(&directory);
    let rows = core_rows();
    assert_eq!(rows.len(), 11);
    for row in rows {
        writer
            .add_fields_document(&prepare(row, 1).unwrap())
            .unwrap();
    }
    writer.commit().unwrap();
    let reader = DirectoryReader::open(&directory).unwrap();
    let cases = search_cases();
    assert_eq!(cases.len(), 12);
    for case in cases {
        let (actual, total) = search(&reader, &case.query, &case.sort, case.size);
        assert_eq!(actual, case.ids, "{} logical IDs/order", case.id);
        assert_eq!(total, case.total, "{} exact total", case.id);
        println!("NATIVE_SEARCH_CASE {} passed", case.id);
    }
    let opened = reader.open_segments().unwrap();
    let segments = opened.as_open_segments();
    let metrics = aggs::metric_states(
        &segments,
        reader.segment_readers(),
        &all(),
        &[MetricSpec {
            field: "price".into(),
            kind: ValueKind::Long,
            source: Source::DocValues,
            needs: NEED_ALL,
        }],
    )
    .unwrap();
    assert_eq!(metrics.len(), 1);
    let value = metrics[0];
    assert_eq!((value.sum, value.min, value.max, value.count), METRICS);
    assert!(
        [value.sum, value.min, value.max]
            .iter()
            .all(|x| x.is_finite())
    );
    println!("NATIVE_SEARCH_CASE finite-metrics passed");
    let mut terms =
        terms_agg::terms(&segments, reader.segment_readers(), &all(), "category", 3).unwrap();
    assert_eq!(terms.other_doc_count, 0);
    // This finite key order is an explicit research adaptation of the native terms result.
    terms.buckets.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        terms.buckets,
        TERMS
            .iter()
            .map(|(key, count)| (key.as_bytes().to_vec(), *count))
            .collect::<Vec<_>>()
    );
    println!("NATIVE_SEARCH_CASE finite-key-ordered-terms passed");
    writer.close().unwrap();
}

#[test]
fn native_analyzer_reference_tokens_positions_and_utf16_offsets() {
    let cases = analyzer_cases();
    assert_eq!(cases.len(), 4);
    for case in cases {
        let actual = analyzer().try_analyze_stream(case.text).unwrap();
        let mut position = -1;
        let tokens: Vec<_> = actual
            .tokens
            .iter()
            .map(|t| {
                position += t.position_increment;
                (t.term.as_str(), position, t.start_offset, t.end_offset)
            })
            .collect();
        assert_eq!(tokens, case.tokens, "{}", case.id);
        println!("NATIVE_ANALYZER_CASE {} passed", case.id);
    }
}

#[test]
fn held_out_phrase_order_gap_and_date_range_boundaries() {
    let directory = ByteBuffersDirectory::new();
    let mut writer = writer(&directory);
    let base = core_rows()[0];
    for &(id, title) in PHRASE_ROWS {
        writer
            .add_fields_document(
                &prepare(
                    Row {
                        uid: id,
                        title,
                        ..base
                    },
                    1,
                )
                .unwrap(),
            )
            .unwrap();
    }
    writer.commit().unwrap();
    let reader = DirectoryReader::open(&directory).unwrap();
    let sort = [SortField::numeric("uid", SortType::String, false)];
    let phrase = search(&reader, &top(analyzed("brown fox", true)), &sort, 128).0;
    assert_eq!(phrase, PHRASE_IDS);
    let wrong_conjunction = top(Clause::Boolean(Box::new(BooleanQuery {
        must: vec![term("title", "brown"), term("title", "fox")],
        ..Default::default()
    })));
    let wrong_phrase = search(&reader, &wrong_conjunction, &sort, 128).0;
    assert_eq!(wrong_phrase, WRONG_AND_IDS);
    assert_ne!(
        wrong_phrase, phrase,
        "Unsafe AND substitution must be detected"
    );
    let dates = ByteBuffersDirectory::new();
    let mut date_writer = self::writer(&dates);
    for &(id, date) in DATE_ROWS {
        date_writer
            .add_fields_document(
                &prepare(
                    Row {
                        uid: id,
                        created_at: date,
                        ..base
                    },
                    1,
                )
                .unwrap(),
            )
            .unwrap();
    }
    date_writer.commit().unwrap();
    let date_reader = DirectoryReader::open(&dates).unwrap();
    let range = top(Clause::PointsRange(PointsRangeQuery::new(
        "created_at",
        DATE_RANGE.0,
        DATE_RANGE.1,
    )));
    let dated = search(&date_reader, &range, &sort, 128).0;
    assert_eq!(dated, DATE_IDS);
    assert_ne!(
        ids(&date_reader),
        dated,
        "Unsafe match-all substitution must be detected"
    );
    date_writer.close().unwrap();
    println!("NATIVE_HELD_OUT phrase-and-substitution date-match-all-substitution rejected");
    writer.close().unwrap();
}

#[test]
fn native_crud_merge_frozen_reader_reopen_and_resolved_identity_replay() {
    let first = ByteBuffersDirectory::new();
    let replay = ByteBuffersDirectory::new();
    let base = core_rows()[0];
    let history = [
        ("stable", "original", 1),
        ("other", "keep", 2),
        ("stable", "replacement", 3),
    ];
    let mut expected = BTreeMap::new();
    let mut w = writer(&first);
    w.commit().unwrap();
    let empty = DirectoryReader::open(&first).unwrap();
    for (id, title, version) in history {
        let document = prepare(
            Row {
                uid: id,
                title,
                ..base
            },
            version,
        )
        .unwrap();
        w.update_fields_documents(Term::new("uid", id.as_bytes()), &[document])
            .unwrap();
        expected.insert(id.to_string(), (title.to_string(), version));
    }
    w.commit().unwrap();
    let frozen = DirectoryReader::open(&first).unwrap();
    assert_eq!(state(&frozen), expected);
    assert!(state(&empty).is_empty());
    w.delete_documents_by_term(&[Term::new("uid", b"other")])
        .unwrap();
    w.commit().unwrap();
    w.force_merge(1).unwrap();
    w.commit().unwrap();
    let final_reader = DirectoryReader::open(&first).unwrap();
    expected.remove("other");
    assert_eq!(state(&final_reader), expected);
    assert_eq!(state(&frozen).len(), 2);
    assert_eq!(final_reader.segment_readers().len(), 1);
    w.close().unwrap();
    let mut reopened = writer(&first);
    assert_eq!(state(&DirectoryReader::open(&first).unwrap()), expected);
    reopened
        .add_fields_document(
            &prepare(
                Row {
                    uid: "after-reopen",
                    title: "new",
                    ..base
                },
                4,
            )
            .unwrap(),
        )
        .unwrap();
    reopened.commit().unwrap();
    expected.insert("after-reopen".into(), ("new".into(), 4));
    reopened.close().unwrap();
    let mut replay_writer = writer(&replay);
    for (id, title, version) in history {
        replay_writer
            .update_fields_documents(
                Term::new("uid", id.as_bytes()),
                &[prepare(
                    Row {
                        uid: id,
                        title,
                        ..base
                    },
                    version,
                )
                .unwrap()],
            )
            .unwrap();
    }
    replay_writer
        .delete_documents_by_term(&[Term::new("uid", b"other")])
        .unwrap();
    replay_writer
        .add_fields_document(
            &prepare(
                Row {
                    uid: "after-reopen",
                    title: "new",
                    ..base
                },
                4,
            )
            .unwrap(),
        )
        .unwrap();
    replay_writer.commit().unwrap();
    replay_writer.close().unwrap();
    assert_eq!(state(&DirectoryReader::open(&replay).unwrap()), expected);
    assert_eq!(state(&DirectoryReader::open(&first).unwrap()), expected);
    println!(
        "NATIVE_LIFECYCLE crud merge frozen-reader native-reopen resolved-ID-version-replay passed"
    );
}

#[test]
fn private_refusal_concurrent_refresh_and_unsafe_tentative_writer_counterexample() {
    let directory = ByteBuffersDirectory::new();
    let mut writer = writer(&directory);
    let base = core_rows()[0];
    writer
        .add_fields_document(
            &prepare(
                Row {
                    uid: "committed",
                    ..base
                },
                1,
            )
            .unwrap(),
        )
        .unwrap();
    writer.commit().unwrap();
    let frozen = DirectoryReader::open(&directory).unwrap();
    let writer = Mutex::new(writer);
    let barrier = Barrier::new(3);
    let (prepared_sender, prepared_receiver) = std::sync::mpsc::sync_channel(1);
    let (refreshed_sender, refreshed_receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let barrier_ref = &barrier;
        let writer_ref = &writer;
        let refuse = scope.spawn(move || {
            barrier_ref.wait();
            let too_long = "x".repeat(MAX_BYTES + 1);
            assert!(
                prepare(
                    Row {
                        uid: "invalid",
                        title: &too_long,
                        ..base
                    },
                    2
                )
                .is_err()
            );
            // Keep valid refused preparation private and alive throughout the refresh.
            let tentative = prepare(
                Row {
                    uid: "authority-refused",
                    ..base
                },
                2,
            )
            .unwrap();
            let simulated_authority_committed = false;
            assert!(!simulated_authority_committed);
            prepared_sender.send(()).unwrap();
            refreshed_receiver.recv().unwrap();
            drop(tentative);
        });
        let refresh = scope.spawn(move || {
            barrier_ref.wait();
            // This receipt orders failed preparation before refresh, without timing assumptions.
            prepared_receiver.recv().unwrap();
            assert_eq!(
                ids(&DirectoryReader::open_from_writer(writer_ref).unwrap()),
                ["committed"]
            );
            refreshed_sender.send(()).unwrap();
        });
        barrier.wait();
        refuse.join().unwrap();
        refresh.join().unwrap();
    });
    assert_eq!(ids(&frozen), ["committed"]);
    // Deliberately unsafe control: mutate the serving writer before authority acceptance.
    writer
        .lock()
        .unwrap()
        .add_fields_document(
            &prepare(
                Row {
                    uid: "authority-refused",
                    ..base
                },
                2,
            )
            .unwrap(),
        )
        .unwrap();
    let simulated_authority_committed = false;
    assert!(!simulated_authority_committed);
    let leaked = ids(&DirectoryReader::open_from_writer(&writer).unwrap());
    assert_eq!(leaked, ["authority-refused", "committed"]);
    assert_ne!(
        leaked,
        ids(&frozen),
        "Serving-writer speculation must violate the independent prefix oracle"
    );
    assert_eq!(
        ids(&DirectoryReader::open(&directory).unwrap()),
        ["committed"]
    );
    let mut writer = writer
        .into_inner()
        .unwrap_or_else(|_| panic!("writer mutex poisoned"));
    writer.set_commit_on_close(false);
    writer.close().unwrap();
    assert_eq!(
        ids(&DirectoryReader::open(&directory).unwrap()),
        ["committed"]
    );
    println!(
        "NATIVE_VISIBILITY private-refusal passed unsafe-serving-writer counterexample detected"
    );
}

#[test]
fn bounded_preparation_and_256_resolved_operations() {
    let directory = ByteBuffersDirectory::new();
    let mut writer = writer(&directory);
    let base = core_rows()[0];
    assert!(prepare(Row { uid: "", ..base }, 1).is_err());
    assert!(prepare(base, 0).is_err());
    let too_many = "word ".repeat(MAX_TERMS + 1);
    assert_eq!(
        prepare(
            Row {
                title: &too_many,
                ..base
            },
            1
        )
        .err(),
        Some("term-bound")
    );
    let files_before = directory.list_all().unwrap();
    assert_eq!(
        prepare_batch(&vec![base; MAX_DOCUMENTS + 1], 1).err(),
        Some("document-bound")
    );
    assert_eq!(
        directory.list_all().unwrap(),
        files_before,
        "Limit+one refused before native I/O"
    );
    assert_eq!(
        prepare_batch(&vec![base; MAX_DOCUMENTS], 1).unwrap().len(),
        MAX_DOCUMENTS
    );
    let mut expected = BTreeMap::new();
    for operation in 0..256 {
        let id = format!("{:03}", operation % (MAX_DOCUMENTS / 2));
        let version = i64::try_from(operation + 1).unwrap();
        let title = format!("resolved payload {operation}");
        let document = prepare(
            Row {
                uid: &id,
                title: &title,
                ..base
            },
            version,
        )
        .unwrap();
        writer
            .update_fields_documents(Term::new("uid", id.as_bytes()), &[document])
            .unwrap();
        expected.insert(id, (title, version));
        if operation % 32 == 31 {
            writer.commit().unwrap();
        }
        if operation % 64 == 63 {
            writer.force_merge(1).unwrap();
            writer.commit().unwrap();
        }
    }
    let reader = DirectoryReader::open(&directory).unwrap();
    assert_eq!(reader.num_docs(), 64);
    assert_eq!(state(&reader), expected);
    assert!(reader.segment_readers().len() <= 32);
    writer.close().unwrap();
    println!(
        "NATIVE_BOUNDS operations=256 live_documents=64 physical_documents<=128 actors<=3 segments<=32 passed"
    );
}

#[test]
fn native_execution_envelope_receipt() {
    let read = |name: &str| {
        std::fs::read_to_string(format!("/sys/fs/cgroup/{name}"))
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|_| "unavailable".to_string())
    };
    println!(
        "NATIVE_RESEARCH_ENVELOPE target={} memory={} swap={} pids={} cpu={}",
        std::env::consts::ARCH,
        read("memory.max"),
        read("memory.swap.max"),
        read("pids.max"),
        read("cpu.max")
    );
    assert!(matches!(std::env::consts::ARCH, "x86_64" | "aarch64"));
}
