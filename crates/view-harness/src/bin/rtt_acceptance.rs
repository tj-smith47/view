//! `rtt-acceptance`: the RTT-injection proof, driven through the tap
//! channel and the stub-ssh delay relay -- both unix-only mechanisms, so
//! the driver lives in a gated child module and this binary is a shim
//! that reports the platform rather than failing to build on it (the same
//! shape `bench`'s own unix-only row modules use).
//!
//! ```text
//! cargo run --release -p view-harness --bin rtt-acceptance -- \
//!     --taps-view-bin target/taps/release/view --nvim-bin nvim
//! ```

#[cfg(unix)]
#[path = "rtt_acceptance/run.rs"]
mod run;

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    run::main()
}

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "rtt-acceptance measures through the tap channel and the stub-ssh delay relay, \
         neither of which exists off unix; nothing to run here"
    );
}
