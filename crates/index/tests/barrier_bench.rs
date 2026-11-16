//! Not a gate: prints the cost of pinning a reader and of publishing one. Run with
//! `cargo test --release -p hsearch-index --test barrier_bench -- --ignored --nocapture`.
use hsearch_index::barrier::{Readers, Replay};
use std::sync::Arc;
use std::time::Instant;

#[test]
#[ignore]
fn pin_and_publish_cost() {
    let readers = Arc::new(Readers::<u64>::new(64));
    readers.commit(u64::MAX / 2).unwrap();
    readers.publish(0, Replay::new(1).cut().unwrap()).unwrap();
    let n = 2_000_000u32;
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(readers.pin());
    }
    println!("pin, 1 thread: {:?} per call", t.elapsed() / n);
    for threads in [4usize, 16] {
        let t = Instant::now();
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                let r = Arc::clone(&readers);
                std::thread::spawn(move || {
                    for _ in 0..500_000 {
                        std::hint::black_box(r.pin());
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        println!(
            "pin, {threads} threads: {:?} per call (wall / total calls)",
            t.elapsed() / (500_000 * threads as u32)
        );
    }
    let m = 200_000u64;
    let t = Instant::now();
    for i in 0..m {
        readers
            .publish(i, Replay::new(2 + i).cut().unwrap())
            .unwrap();
    }
    println!("publish: {:?} per call", t.elapsed() / m as u32);
}
