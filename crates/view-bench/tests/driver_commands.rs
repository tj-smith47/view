//! A source-text pin on the `:` commands a bench driver types at the
//! session it measures.
//!
//! Only the measured action is allowed to paint. Nvim announces a delete
//! of more lines than `'report'`, and a config that routes messages into a
//! float paints that announcement over the cells the next sample types
//! into -- where the driver can do nothing but refuse the cell, so a row
//! records on the plugin-free fixtures and never on the login-shaped one.
//! Every command that is a driver's own setup therefore carries `silent`,
//! whose messages no config can route because there are none. `silent` and
//! never `silent!`: an error still reaches the screen, which is what a
//! setup step that failed is worth.
//!
//! The walk reads each source down to its `#[cfg(test)]` boundary and
//! finds the literal the command is written in, whether it is typed
//! through an `<Esc>` escape, built by `format!` or declared as a byte
//! string. Its stated limit: a command a driver *imports* is not a literal
//! here, so such a member is declared with the function that builds it
//! and pinned by that name instead.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// One typed command that does not carry `silent`.
struct TypedCommand {
    /// Source file under `src/`, as the walk names it.
    file: &'static str,
    /// The command exactly as the walk extracts it, empty for a member
    /// built outside this crate.
    command: &'static str,
    /// The function that builds the command, for a member the literal
    /// walk cannot see; empty for one written here.
    built_by: &'static str,
    /// Why it is the measured action rather than setup.
    grounds: &'static str,
}

const UNSILENCED: &[TypedCommand] = &[
    TypedCommand {
        file: "picker.rs",
        command: "View picker files",
        built_by: "",
        grounds: "the picker open is the row's own subject, and view's command \
                  reaches the editor as an `rpcnotify` that emits no message for \
                  any config to route",
    },
    TypedCommand {
        file: "ai_session.rs",
        command: "View ai open",
        built_by: "",
        grounds: "the panel the AI rows sample keys against, opened by the same \
                  message-free `rpcnotify` command",
    },
    TypedCommand {
        file: "notices.rs",
        command: "View",
        built_by: "",
        grounds: "the takedown asks for view's own message history and waits for it \
                  on screen, so a prefix that suppressed what it waits for would \
                  turn every takedown into a timeout",
    },
    TypedCommand {
        file: "supervision.rs",
        command: "",
        built_by: "wedge_command",
        grounds: "the wedge is the measured action itself: the clock starts on the \
                  line before it is typed and stops on the banner it provokes",
    },
];

fn source_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `*.rs` under `src/`, including its subdirectories.
fn driver_sources(dir: &Path) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("the source directory must be readable") {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            found.extend(driver_sources(&path));
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs") {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("a source file name")
                .to_string();
            let body = std::fs::read_to_string(&path).expect("a readable source file");
            found.push((name, body));
        }
    }
    found
}

/// The `:` commands a file's literals carry above its test module.
///
/// A colon opens one when it stands immediately after a literal's own
/// quote or after the `<Esc>` a driver types in front of it, and is
/// followed by the first character of an ex word: that is what parts a
/// command from a separator (`": buffer content"`), a path element
/// (`push(":")`) and a Rust path (`"::"`).
fn typed_commands(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            break;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        for (at, _) in line.match_indices(':') {
            let opens = line[..at].ends_with('"') || line[..at].ends_with("\\x1b");
            let rest = &line[at + 1..];
            let is_command = rest
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '%');
            if !opens || !is_command {
                continue;
            }
            let end = rest.find(['"', '\\', '{']).unwrap_or(rest.len());
            found.push(rest[..end].trim_end().to_string());
        }
    }
    found
}

// What the walk reads and what it does not, on a source it holds whole:
// the three spellings a typed command is written in, the three colons that
// open no command, a comment quoting one, and the test boundary.
#[test]
fn the_walk_reads_a_typed_command_in_every_spelling_a_driver_writes_it() {
    let source = concat!(
        "    session.send(b\"\\x1b:silent %d _\\r\")?;\n",
        "    session.send(submitted(\":preserve\").as_bytes())?;\n",
        "    const OPEN_COMMAND: &[u8] = b\":View picker files\";\n",
        "    session.send(format!(\":silent e {file}\\r\").as_bytes())?;\n",
        "    path.push(\":\");\n",
        "    head.strip_suffix(\"::\");\n",
        "    text.split_once(\": buffer content\");\n",
        "    // a comment quoting \":qa!\"\n",
        "    #[cfg(test)]\n",
        "    assert_eq!(history_command(), \":View notifications history\\r\");\n",
    );
    assert_eq!(
        typed_commands(source),
        vec![
            "silent %d _".to_string(),
            "preserve".to_string(),
            "View picker files".to_string(),
            "silent e".to_string(),
        ],
        "a command is read wherever it is written and a colon that opens none is \
         not one"
    );
}

#[test]
fn every_typed_command_is_silenced_or_declared_the_measured_action() {
    let mut undeclared = Vec::new();
    for (file, source) in driver_sources(&source_dir()) {
        for command in typed_commands(&source) {
            if command.starts_with("silent ") {
                continue;
            }
            let declared = UNSILENCED
                .iter()
                .any(|entry| entry.file == file && entry.command == command);
            if !declared {
                undeclared.push(format!("{file}: :{command}"));
            }
        }
    }
    assert!(
        undeclared.is_empty(),
        "a driver's own setup paints nothing, because the message it would print is \
         routed into a float by the configs these rows are recorded on and stands \
         over the cells the next sample types into:\n  {}\nPrefix the command with \
         `silent`, or declare it here as the measured action with grounds.",
        undeclared.join("\n  ")
    );
}

#[test]
fn no_declared_command_outlives_the_send_site_it_describes() {
    let sources = driver_sources(&source_dir());
    for entry in UNSILENCED {
        let found = sources.iter().any(|(file, source)| {
            *file == entry.file
                && if entry.built_by.is_empty() {
                    typed_commands(source).iter().any(|c| c == entry.command)
                } else {
                    source.contains(entry.built_by)
                }
        });
        assert!(
            found,
            "{}'s exemption is declared here and its send site is gone; a command \
             that moved is measuring something else now, so its grounds ({}) are \
             what needs revisiting rather than this entry alone",
            entry.file, entry.grounds
        );
    }
}
