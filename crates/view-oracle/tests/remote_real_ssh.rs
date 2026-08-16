//! The opt-in half of the remote coverage: a real OpenSSH client, a real
//! connection, and a real remote shell.
//!
//! `#[ignore]`d, and it stays that way. A CI host cannot be assumed to have
//! a reachable `sshd`, network egress, or credentials, and a test that
//! silently passed by finding none of them would be worth less than no test.
//! The gated coverage runs against the committed stand-in client
//! (`view_oracle::remote`); this is what the stand-in cannot substitute for.
//!
//! # Running it
//!
//! ```sh
//! # any destination ssh's own syntax accepts: a ~/.ssh/config alias, or
//! # user@host. The connection must already work non-interactively --
//! # `ssh -o BatchMode=yes $VIEW_REMOTE_TEST_HOST true` must succeed --
//! # because view runs the client in batch mode and an embedded editor has
//! # no way to answer a prompt.
//! export VIEW_REMOTE_TEST_HOST=a-dev-box
//! # optional: the editor on the far side, when it is not on the PATH a
//! # non-interactive ssh session gets. A login shell's PATH is not the
//! # PATH `ssh host nvim` resolves against, and on macOS in particular the
//! # Homebrew prefix is missing from the latter.
//! export VIEW_REMOTE_TEST_NVIM=/opt/homebrew/bin/nvim
//! cargo test -p view-oracle --test remote_real_ssh -- --ignored --nocapture
//! ```
//!
//! A loopback destination (`localhost`, with this host's own `sshd`) proves
//! the mechanism and is the cheapest thing to point this at. It is not the
//! strongest form of the claim: the shell, the login environment and the
//! filesystem on the far side are this machine's, so a remote host whose
//! shell this tree does not control is what actually tests the assumption
//! that none of them were needed.
//!
//! # What this proves that the stand-in cannot
//!
//! - Authentication happens, and `BatchMode=yes` means a refused one
//!   reports rather than prompts.
//! - `~/.ssh/config` resolution: the destination is handed to the client
//!   unparsed, so an alias resolves exactly as it does on a command line.
//! - The remote user's real login environment, and a value carrying a space
//!   or a single quote surviving a shell that is not this host's.
//! - The working directory a far-side command actually starts in, which the
//!   CLI's own no-`cd` design rests on.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use view_engine::process::{Engine, EngineConfig, RemoteSpec};

/// The destination, from the environment. Every test here reports its own
/// skip by name when it is unset: a silent pass is the failure mode this
/// whole file is arranged against.
fn target() -> Option<String> {
    match std::env::var("VIEW_REMOTE_TEST_HOST") {
        Ok(host) if !host.is_empty() => Some(host),
        _ => {
            eprintln!(
                "skipped: VIEW_REMOTE_TEST_HOST is unset, so there is no real \
                 target to connect to (see this file's own module docs)"
            );
            None
        }
    }
}

/// A spec for the configured destination, with the far-side editor named
/// when the environment names one and connection multiplexing refused.
///
/// A destination whose `~/.ssh/config` sets `ControlMaster`/`ControlPersist`
/// lets a connection ride a socket some earlier command opened, at which
/// point none of these legs performs a key exchange and the leg that is
/// specifically about a *cold* connection -- the batch-mode rejection, which
/// is where a prompt would appear -- never sees one. Refusing the multiplex
/// costs a second per leg and is what makes the timings here per-connection
/// costs rather than per-socket-reuse costs.
fn spec(host: &str) -> RemoteSpec {
    let spec = RemoteSpec::new(host)
        .with_ssh_opt("ControlMaster=no")
        .with_ssh_opt("ControlPath=none");
    match std::env::var("VIEW_REMOTE_TEST_NVIM") {
        Ok(bin) if !bin.is_empty() => spec.with_remote_nvim_bin(bin),
        _ => spec,
    }
}

/// A config that runs the far-side editor without the remote user's own
/// configuration, and gives a real connection room to establish itself:
/// the local handshake budget bounds a process start, and this one bounds a
/// process start plus a key exchange.
fn cfg(host: &str) -> EngineConfig {
    EngineConfig::isolated()
        .with_remote(spec(host))
        .with_handshake_timeout(Duration::from_secs(30))
}

/// The values that break a design passing them as separate arguments, or
/// escaping only what it judges to need escaping -- asserted against a
/// shell this tree does not own, on a host it does not configure.
///
/// The stand-in proves the mechanism is right. This proves the mechanism is
/// right about a real remote shell, which is a different claim: the join
/// happens inside the real client, over the wire, and is re-parsed by
/// whatever `sh` the remote account resolves.
#[test]
#[ignore = "needs VIEW_REMOTE_TEST_HOST and a reachable ssh target"]
fn adversarial_values_reach_a_real_remote_editors_environment_byte_for_byte() {
    const SPACED: &str = "/home/a user/config/init.lua";
    const QUOTED: &str = "it's a value";
    let Some(host) = target() else {
        return;
    };

    let engine = Engine::spawn(
        cfg(&host)
            .with_env("VIEW_REMOTE_SPACED", SPACED)
            .with_env("VIEW_REMOTE_QUOTED", QUOTED),
    )
    .expect("the configured remote target must handshake");

    for (name, expected) in [
        ("VIEW_REMOTE_SPACED", SPACED),
        ("VIEW_REMOTE_QUOTED", QUOTED),
    ] {
        let seen = engine
            .handle
            .eval_str(&format!("getenv('{name}')"))
            .unwrap();
        assert_eq!(
            seen, expected,
            "{name} did not survive the real remote shell's re-parse: a value \
             word-split, truncated at a quote, or expanded as syntax is a \
             value the remote editor never received"
        );
    }
}

/// The working directory a far-side editor actually starts in.
///
/// The CLI passes a relative `--remote host:path` through unresolved, on the
/// stated assumption that a non-interactive ssh session starts at the remote
/// account's own home -- the same convention `scp host:path` rests on. That
/// assumption is about a real `sshd`, so the stand-in (which runs the
/// command in this process's own working directory) cannot check it and this
/// is the only place it can be checked at all.
#[test]
#[ignore = "needs VIEW_REMOTE_TEST_HOST and a reachable ssh target"]
fn a_real_remote_editor_starts_in_the_remote_accounts_own_home() {
    let Some(host) = target() else {
        return;
    };
    let engine = Engine::spawn(cfg(&host)).expect("the configured remote target must handshake");

    let cwd = engine.handle.eval_str("getcwd()").unwrap();
    let home = engine.handle.eval_str("getenv('HOME')").unwrap();
    assert!(
        !home.is_empty(),
        "the far side reported no HOME at all, so there is nothing to hold \
         the working directory against"
    );
    assert_eq!(
        cwd, home,
        "the remote editor started in {cwd:?} rather than the remote \
         account's own home {home:?}; a relative path the CLI forwards \
         unresolved would open somewhere other than where the user meant"
    );
}

/// Batch mode against a real server, which is the only place the guarantee
/// actually holds or fails: an embedded client has no way to render an
/// authentication prompt and no keyboard to answer one, so a rejected
/// connection must report, promptly, rather than sit on a question.
///
/// The rejection is arranged by offering the server no authentication
/// method at all, which every OpenSSH server refuses. It rides
/// `--ssh-opt`'s own pass-through (`RemoteSpec::with_ssh_opt`), so the
/// option really does reach the client the spawn runs.
#[test]
#[ignore = "needs VIEW_REMOTE_TEST_HOST and a reachable ssh target"]
fn a_real_batch_mode_rejection_reports_instead_of_prompting() {
    let Some(host) = target() else {
        return;
    };
    let handshake = Duration::from_secs(30);
    let refused = EngineConfig::isolated()
        .with_remote(spec(&host).with_ssh_opt("PreferredAuthentications=none"))
        .with_handshake_timeout(handshake);

    let started = Instant::now();
    let outcome = match Engine::spawn(refused) {
        Ok(_) => String::from("the spawn was accepted"),
        // the variant, not its rendering: a timeout here means the refusal
        // was waited out rather than reported, which is the prompt this
        // disproves and is invisible in a message
        Err(view_engine::EngineError::Timeout { method, timeout }) => {
            format!("the spawn waited out {timeout:?} for {method}")
        }
        Err(err) => err.to_string(),
    };
    let elapsed = started.elapsed();
    assert!(
        !outcome.starts_with("the spawn was accepted") && !outcome.starts_with("the spawn waited"),
        "a connection the server refuses must fail the spawn, and fail by \
         report rather than by timeout: {outcome}"
    );
    // bounded well below the handshake budget on purpose: a bound of the
    // budget itself would pass a client that sat on a prompt until just
    // short of it, which is the failure this test exists to catch
    let prompt = Duration::from_secs(10);
    assert!(
        elapsed < prompt && prompt < handshake,
        "the refusal took {elapsed:?}: a batch-mode client must be told no \
         and say so, not be waited out"
    );
}
