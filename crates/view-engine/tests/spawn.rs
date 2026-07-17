#![allow(clippy::unwrap_used, clippy::expect_used)]
use view_engine::process::{Engine, EngineConfig};

#[test]
fn spawns_and_handshakes_with_real_nvim() {
    let mut engine = Engine::spawn(EngineConfig::default()).unwrap();
    assert!(engine.api_info.channel_id >= 1);
    // floor from the spec: engine must be at least 0.11
    assert!(
        (engine.api_info.version_major, engine.api_info.version_minor) >= (0, 11),
        "nvim >= 0.11 required, found {}.{}",
        engine.api_info.version_major,
        engine.api_info.version_minor
    );
    let echoed = engine
        .handle
        .request("nvim_eval", vec![rmpv::Value::from("21 * 2")])
        .unwrap();
    assert_eq!(echoed.as_u64(), Some(42));
    engine.child.kill().unwrap();
}
