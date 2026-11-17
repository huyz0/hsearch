//! M07.02: the frame header, the shared budget and the receive state machine, without any socket.
//! Every refusal here happens before a body byte is stored.
use hsearch_peer::budget::{Budget, BudgetLimits};
use hsearch_peer::inbound::{Config, Event, Fatal, Inbound, Side};
use hsearch_peer::wire::{HEADER_LEN, Header, Kind, Limits, Status, WireError};
use hsearch_security::policy::PeerRpc;

fn cfg(side: Side) -> Config {
    Config {
        side,
        limits: Limits::default(),
        header_ms: 100,
        body_base_ms: 100,
        min_rate: 1000,
        idle_ms: 1000,
        max_pending: 2,
    }
}

fn budget(frames: u64, bytes: u64) -> Budget {
    Budget::new(BudgetLimits {
        connections: 2,
        handshaking: 2,
        frames,
        bytes,
    })
}

fn frame(h: Header, body: &[u8]) -> Vec<u8> {
    let mut out = h.encode().to_vec();
    out.extend_from_slice(body);
    out
}

fn req(stream: u32, rpc: PeerRpc, body: &[u8]) -> Vec<u8> {
    frame(Header::request(stream, rpc, 1000, body.len() as u32), body)
}

// ---- header ----------------------------------------------------------------------------------

#[test]
fn every_rpc_and_kind_round_trips() {
    let limits = Limits::default();
    for (i, rpc) in PeerRpc::ALL.into_iter().enumerate() {
        let h = Header::request(2 * i as u32 + 1, rpc, 5000, 10);
        assert_eq!(Header::parse(&h.encode(), &limits, true), Ok(h));
    }
    for status in 0..8 {
        let s = Status::from_code(status).unwrap();
        assert_eq!(s.code(), status);
        let h = Header::response(4, s, 3);
        assert_eq!(Header::parse(&h.encode(), &limits, false), Ok(h));
    }
    assert!(Status::from_code(8).is_none());
    let c = Header::cancel(7);
    assert_eq!(Header::parse(&c.encode(), &limits, true), Ok(c));
}

#[test]
fn a_bad_header_names_the_field_that_is_wrong() {
    let limits = Limits::default();
    let good = Header::request(1, PeerRpc::Heartbeat, 100, 4).encode();
    let with = |at: usize, v: u8| {
        let mut b = good;
        b[at] = v;
        b
    };
    let parse = |b: [u8; HEADER_LEN]| Header::parse(&b, &limits, true);
    assert_eq!(parse(with(0, b'X')), Err(WireError::Magic));
    assert_eq!(parse(with(2, 2)), Err(WireError::Version));
    assert_eq!(parse(with(3, 9)), Err(WireError::Kind));
    assert_eq!(parse(with(4, 0)), Err(WireError::Rpc));
    assert_eq!(parse(with(4, 200)), Err(WireError::Rpc));
    assert_eq!(parse(with(5, 1)), Err(WireError::Status));
    assert_eq!(parse(with(6, 1)), Err(WireError::Reserved));
    assert_eq!(parse(with(7, 1)), Err(WireError::Reserved));
    // Stream 0 and an even stream from a caller.
    let mut b = good;
    b[8..12].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(parse(b), Err(WireError::Stream));
    b[8..12].copy_from_slice(&2u32.to_be_bytes());
    assert_eq!(parse(b), Err(WireError::Stream));
    // No deadline, and one past the cap.
    b = good;
    b[12..16].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(parse(b), Err(WireError::Deadline));
    b[12..16].copy_from_slice(&(limits.max_deadline_ms + 1).to_be_bytes());
    assert_eq!(parse(b), Err(WireError::Deadline));
    // A response arriving at a server, a request arriving at a client.
    let resp = Header::response(1, Status::Ok, 0).encode();
    assert_eq!(Header::parse(&resp, &limits, true), Err(WireError::Kind));
    assert_eq!(Header::parse(&good, &limits, false), Err(WireError::Kind));
    // A cancel with a body or a deadline, a response with a deadline.
    let mut c = Header::cancel(1).encode();
    c[16..20].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(Header::parse(&c, &limits, true), Err(WireError::Body));
    let mut c = Header::cancel(1).encode();
    c[12..16].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(Header::parse(&c, &limits, true), Err(WireError::Deadline));
    let mut r = Header::response(2, Status::Ok, 0).encode();
    r[12..16].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(Header::parse(&r, &limits, false), Err(WireError::Deadline));
    let mut r = Header::response(2, Status::Ok, 0).encode();
    r[4] = 1;
    assert_eq!(Header::parse(&r, &limits, false), Err(WireError::Rpc));
    let mut r = Header::response(2, Status::Ok, 0).encode();
    r[5] = 99;
    assert_eq!(Header::parse(&r, &limits, false), Err(WireError::Status));
}

#[test]
fn a_length_is_capped_by_the_rpc_not_by_one_shared_maximum() {
    let limits = Limits::default();
    let heartbeat = Limits::max_request(PeerRpc::Heartbeat);
    assert!(Limits::max_request(PeerRpc::ShardWrite) > 1 << 20);
    let ok = Header::request(1, PeerRpc::Heartbeat, 10, heartbeat);
    assert!(Header::parse(&ok.encode(), &limits, true).is_ok());
    let big = Header::request(1, PeerRpc::Heartbeat, 10, heartbeat + 1);
    assert_eq!(
        Header::parse(&big.encode(), &limits, true),
        Err(WireError::TooLong)
    );
    let huge = Header::response(2, Status::Ok, limits.max_response + 1);
    assert_eq!(
        Header::parse(&huge.encode(), &limits, false),
        Err(WireError::TooLong)
    );
    assert_eq!(Header::cancel(1).kind, Kind::Cancel);
}

// ---- budget ----------------------------------------------------------------------------------

#[test]
fn the_budget_is_all_or_nothing_and_gives_back_on_drop() {
    let b = budget(2, 100);
    let a = b.frame(60).unwrap();
    assert!(b.frame(60).is_none(), "bytes cap");
    assert_eq!(
        b.report().frames,
        1,
        "a refused frame leaves nothing behind"
    );
    let c = b.frame(40).unwrap();
    assert!(b.frame(0).is_none(), "frame cap");
    drop(a);
    assert_eq!((b.report().frames, b.report().bytes), (1, 40));
    drop(c);
    let r = b.report();
    assert_eq!((r.frames, r.bytes), (0, 0));
    assert_eq!((r.peak_frames, r.peak_bytes), (2, 100));
    assert_eq!(r.refused_frames, 2);
    let c1 = b.connection().unwrap();
    let _c2 = b.connection().unwrap();
    assert!(b.connection().is_none());
    drop(c1);
    assert!(b.connection().is_some());
    assert_eq!(b.report().refused_connections, 1);
    assert_eq!(b.limits().connections, 2);
}

// ---- receive ---------------------------------------------------------------------------------

fn requests(events: Vec<Event>) -> Vec<(u32, PeerRpc, Vec<u8>)> {
    events
        .into_iter()
        .filter_map(|e| match e {
            Event::Request(r) => Some((r.stream, r.rpc, r.body.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_frame_split_at_every_byte_decodes_the_same() {
    let bytes: Vec<u8> = [
        req(1, PeerRpc::Membership, &[7; 300]),
        req(3, PeerRpc::Heartbeat, &[]),
        frame(Header::cancel(5), &[]),
    ]
    .concat();
    let b = budget(10, 1 << 20);
    let mut whole = Inbound::new(cfg(Side::Server), Some(b.clone()), 0);
    let all = whole.feed(&bytes, 1).unwrap();
    let mut split = Inbound::new(cfg(Side::Server), Some(b), 0);
    let mut got = Vec::new();
    for byte in &bytes {
        got.extend(split.feed(std::slice::from_ref(byte), 1).unwrap());
    }
    assert_eq!(all.len(), 3);
    assert_eq!(got.len(), 3);
    assert_eq!(
        requests(all),
        vec![
            (1, PeerRpc::Membership, vec![7; 300]),
            (3, PeerRpc::Heartbeat, vec![])
        ]
    );
    assert_eq!(requests(got).len(), 2);
}

#[test]
fn a_declared_body_costs_a_permit_and_no_memory_until_bytes_arrive() {
    let b = budget(4, 1 << 24);
    let mut inbound = Inbound::new(cfg(Side::Server), Some(b.clone()), 0);
    let header = Header::request(1, PeerRpc::ShardWrite, 1000, 16 << 20).encode();
    assert!(inbound.feed(&header, 0).unwrap().is_empty());
    assert_eq!(
        inbound.buffered(),
        0,
        "nothing allocated for a declared body"
    );
    assert_eq!(b.report().bytes, 16 << 20, "the permit is held");
    // 1 KiB arrives: the buffer is at most twice what arrived.
    inbound.feed(&[0; 1024], 1).unwrap();
    assert!(inbound.buffered() <= 2048, "{}", inbound.buffered());
    // Dropping the decoder (the connection died) gives the permit back.
    drop(inbound);
    assert_eq!(b.report().bytes, 0);
}

#[test]
fn an_oversized_prefix_ends_the_connection_before_anything_is_reserved() {
    let b = budget(4, 1 << 30);
    let mut inbound = Inbound::new(cfg(Side::Server), Some(b.clone()), 0);
    // Heartbeat claiming 2 GiB.
    let mut header = Header::request(1, PeerRpc::Heartbeat, 1000, 0).encode();
    header[16..20].copy_from_slice(&(2u32 << 30).to_be_bytes());
    assert_eq!(
        inbound.feed(&header, 0).unwrap_err(),
        Fatal::Wire(WireError::TooLong)
    );
    assert_eq!(inbound.buffered(), 0);
    assert_eq!(b.report().peak_bytes, 0, "nothing was ever reserved");
}

#[test]
fn a_body_that_does_not_fit_the_budget_is_skipped_and_the_connection_survives() {
    let b = budget(4, 1000);
    let mut inbound = Inbound::new(cfg(Side::Server), Some(b.clone()), 0);
    let stream: Vec<u8> = [
        req(1, PeerRpc::Membership, &[1; 5000]),
        req(3, PeerRpc::Heartbeat, &[9; 10]),
    ]
    .concat();
    let events = inbound.feed(&stream, 0).unwrap();
    assert!(matches!(
        events[0],
        Event::Refused {
            stream: 1,
            status: Status::Overloaded
        }
    ));
    assert_eq!(requests(events), vec![(3, PeerRpc::Heartbeat, vec![9; 10])]);
    assert_eq!(
        b.report().peak_bytes,
        10,
        "the skipped body was never charged"
    );
    assert_eq!(inbound.pending(), 1);
}

#[test]
fn a_full_queue_sheds_instead_of_growing() {
    let b = budget(100, 1 << 20);
    let mut inbound = Inbound::new(cfg(Side::Server), Some(b), 0);
    let stream: Vec<u8> = (0..4)
        .flat_map(|i| req(2 * i + 1, PeerRpc::Heartbeat, &[]))
        .collect();
    let events = inbound.feed(&stream, 0).unwrap();
    let shed = events
        .iter()
        .filter(|e| matches!(e, Event::Refused { .. }))
        .count();
    assert_eq!((requests(events).len(), shed), (2, 2));
    inbound.finished();
    assert_eq!(inbound.pending(), 1);
    let again = inbound.feed(&req(9, PeerRpc::Heartbeat, &[]), 0).unwrap();
    assert_eq!(requests(again).len(), 1);
    inbound.finished();
    inbound.finished();
    inbound.finished();
    assert_eq!(inbound.pending(), 0);
}

#[test]
fn a_header_trickle_a_stalled_header_and_a_slow_body_are_all_too_slow() {
    let b = budget(4, 1 << 20);
    // Header: 5 bytes then nothing.
    let mut a = Inbound::new(cfg(Side::Server), Some(b.clone()), 0);
    a.feed(&[b'H'; 5], 10).unwrap();
    assert_eq!(a.tick(100), Ok(()));
    assert_eq!(a.tick(111), Err(Fatal::TooSlow));
    // Body: 10,000 bytes at a 1,000 B/s floor and 100 ms base is allowed 10.1 s.
    let mut s = Inbound::new(cfg(Side::Server), Some(b.clone()), 0);
    let h = Header::request(1, PeerRpc::Membership, 1000, 10_000).encode();
    s.feed(&h, 0).unwrap();
    s.feed(&[0; 100], 5000).unwrap();
    assert_eq!(s.tick(10_100), Ok(()));
    assert_eq!(s.tick(10_101), Err(Fatal::TooSlow));
    // A skipped body is held to the same clock.
    let tiny = budget(1, 1);
    let mut k = Inbound::new(cfg(Side::Server), Some(tiny), 0);
    k.feed(&h, 0).unwrap();
    assert_eq!(k.tick(10_101), Err(Fatal::TooSlow));
}

#[test]
fn idle_is_only_when_nothing_is_pending_and_no_frame_has_started() {
    let b = budget(4, 1 << 20);
    let mut i = Inbound::new(cfg(Side::Server), Some(b), 0);
    assert_eq!(i.tick(1000), Ok(()));
    assert_eq!(i.tick(1001), Err(Fatal::Idle));
    i.feed(&req(1, PeerRpc::Heartbeat, &[]), 1500).unwrap();
    // A request is pending: waiting for its answer is not idleness.
    assert_eq!(i.tick(9000), Ok(()));
    i.finished();
    assert_eq!(i.tick(9000), Err(Fatal::Idle));
}

#[test]
fn a_client_decodes_responses_and_refuses_requests() {
    let mut c = Inbound::new(cfg(Side::Client), None, 0);
    let bytes = frame(Header::response(3, Status::Ok, 4), b"data");
    match c.feed(&bytes, 0).unwrap().pop() {
        Some(Event::Response {
            stream: 3,
            status: Status::Ok,
            body,
        }) => assert_eq!(body, b"data"),
        other => panic!("{other:?}"),
    }
    let empty = frame(Header::response(5, Status::WrongRole, 0), &[]);
    assert!(matches!(
        c.feed(&empty, 0).unwrap().pop(),
        Some(Event::Response {
            status: Status::WrongRole,
            ..
        })
    ));
    assert_eq!(
        c.feed(&req(1, PeerRpc::Heartbeat, &[]), 0).unwrap_err(),
        Fatal::Wire(WireError::Kind)
    );
}

#[test]
fn a_refused_frame_claim_leaves_no_trace_in_the_peaks_even_under_contention() {
    // Frames are plentiful, bytes allow exactly one 60-byte body at a time.
    let b = Budget::new(BudgetLimits {
        connections: 1,
        handshaking: 1,
        frames: 1000,
        bytes: 100,
    });
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let b = b.clone();
            std::thread::spawn(move || {
                for _ in 0..5000 {
                    drop(b.frame(60));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let r = b.report();
    assert_eq!((r.peak_frames, r.peak_bytes), (1, 60));
    assert_eq!((r.frames, r.bytes), (0, 0));
    assert!(r.refused_frames > 0);
}

#[test]
fn a_forbidden_rpc_is_refused_at_its_header_with_nothing_charged() {
    use hsearch_security::identity::Role;
    let b = budget(4, 1 << 30);
    let mut inbound = Inbound::new(cfg(Side::Server), Some(b.clone()), 0);
    inbound.set_caller(Role::SearchNode);
    let stream: Vec<u8> = [
        req(1, PeerRpc::ShardWrite, &[1; 4000]),
        req(3, PeerRpc::Heartbeat, &[]),
    ]
    .concat();
    let events = inbound.feed(&stream, 0).unwrap();
    assert!(matches!(
        events[0],
        Event::Refused {
            stream: 1,
            status: Status::WrongRole
        }
    ));
    assert_eq!(requests(events).len(), 1);
    assert_eq!(b.report().peak_bytes, 0);
    assert_eq!(inbound.pending(), 1, "a refusal takes no queue place");
}

#[test]
fn resuming_restarts_the_clock_of_a_frame_in_progress() {
    let b = budget(4, 1 << 20);
    let mut i = Inbound::new(cfg(Side::Server), Some(b), 0);
    i.feed(&[b'H'; 5], 10).unwrap();
    assert_eq!(i.tick(500), Err(Fatal::TooSlow));
    i.resume(500);
    assert_eq!(i.tick(550), Ok(()));
    assert_eq!(i.tick(601), Err(Fatal::TooSlow));
}
