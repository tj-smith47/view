//! Finding 4: `EngineHandle` must be usable from multiple threads at once
//! (issuing requests) while another thread owns the notification receiver.
//! This drives that shape against a real nvim: several threads share one
//! cloned handle and all their requests must complete with correct results.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use view_engine::process::{Engine, EngineConfig};

#[test]
fn concurrent_requesters_all_complete_correctly() {
    let engine = Engine::spawn(EngineConfig::default()).unwrap();

    let threads: Vec<_> = (0..8i64)
        .map(|i| {
            let h = engine.handle.clone();
            std::thread::spawn(move || {
                let expr = format!("{i} * 2");
                let result = h
                    .request("nvim_eval", vec![rmpv::Value::from(expr)])
                    .unwrap();
                (i, result.as_i64())
            })
        })
        .collect();

    for handle in threads {
        let (i, got) = handle.join().unwrap();
        assert_eq!(got, Some(i * 2), "request for input {i} got {got:?}");
    }
}
