//! Test double for the nvim binary: accepts the `--embed` the Engine always
//! passes (and any other argument) without erroring, never touches stdin or
//! stdout, and blocks until killed -- the shape `Engine::spawn`'s
//! handshake-timeout path exists to survive.
//!
//! A compiled binary rather than the shell script this replaced: Windows
//! `CreateProcess` cannot exec a `#!` script at all, which gated the only
//! test of that path off the platform whose process lifetimes differ most
//! from the unix ones the assertion was written against.
//!
//! Writes its own pid to the file named by `VIEW_ENGINE_HANG_MARKER` before
//! blocking. That is what lets a test prove the fixture genuinely started
//! and was running during the handshake window -- an assertion that it is
//! gone afterwards proves nothing about reaping if it never ran at all.

fn main() {
    if let Ok(marker) = std::env::var("VIEW_ENGINE_HANG_MARKER") {
        let _ = std::fs::write(marker, std::process::id().to_string());
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
