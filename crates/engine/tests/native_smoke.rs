//! First native embedding control; not a hsearch authority or format acceptance test.
use lucene_analysis::{Analyzer, StandardAnalyzer};
use lucene_index::document::{Document, Store, StringField, TextField};
use lucene_index::index_writer::IndexWriter;
use lucene_index::segment_info::LuceneVersion;
use lucene_search::directory_reader::DirectoryReader;
use lucene_store::ByteBuffersDirectory;

#[test]
fn pinned_writer_reader_public_fields() {
    let directory = ByteBuffersDirectory::new();
    let mut writer = IndexWriter::open(
        &directory,
        vec![],
        "Lucene104",
        LuceneVersion {
            major: 10,
            minor: 5,
            bugfix: 0,
        },
    )
    .unwrap();
    writer.set_analyzer(Some(std::sync::Arc::new(Analyzer::new(
        StandardAnalyzer::new(),
    ))));
    let mut document = Document::new();
    document.add(StringField::new("uid", "001", Store::Yes));
    document.add(TextField::new("title", "Quick brown fox", Store::Yes));
    writer.add_fields_document(&document).unwrap();
    writer.commit().unwrap();
    let reader = DirectoryReader::open(&directory).unwrap();
    assert_eq!(reader.num_docs(), 1);
    assert_eq!(reader.segment_readers().len(), 1);
    writer.close().unwrap();
    assert_eq!(reader.num_docs(), 1);
}
