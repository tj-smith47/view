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
//! string.
//!
//! Literals alone would pin only the commands written here, so a second
//! walk pins the population of send sites instead: every call expression
//! inside a `send`/`submitted` argument is read off -- the argument taken
//! to the paren that closes it however many lines that takes, counting no
//! paren a string, a raw string in either spelling, or a char literal
//! carries, and a path-qualified call read as its last segment -- and one
//! whose builder this crate does not define is a command built elsewhere
//! -- no literal for the first walk to grade -- so it fails unless a row
//! declares it by that builder's name. A builder defined here needs no
//! row: its own literal stands on a line the first walk already reads.
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
    /// Why the command is exempt: the measured action itself, or a view
    /// command that emits no message for a config to route.
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
        grounds: "setup, exempt on message-freeness rather than on being the \
                  action: it opens the panel the AI rows sample keys against, \
                  through the same message-free `rpcnotify` command",
    },
    TypedCommand {
        file: "notices.rs",
        command: "View",
        built_by: "",
        grounds: "view's own history command, reaching the editor as an \
                  `rpcnotify` that emits no message for any config to route; \
                  the takedown waits on view's history surface, which no \
                  ex-command prefix reaches either way",
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

/// The hash count of the raw string opening at `at`, or `None` where no raw
/// string opens there.
fn raw_string_hashes(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes.get(at) != Some(&b'r') {
        return None;
    }
    let mut hashes = 0usize;
    while bytes.get(at + 1 + hashes) == Some(&b'#') {
        hashes += 1;
    }
    (bytes.get(at + 1 + hashes) == Some(&b'"')).then_some(hashes)
}

/// Whether the raw string opened with `hashes` hashes closes at `at`.
fn raw_string_closes(bytes: &[u8], at: usize, hashes: usize) -> bool {
    bytes.get(at) == Some(&b'"') && (0..hashes).all(|k| bytes.get(at + 1 + k) == Some(&b'#'))
}

/// The argument text of a call whose opening paren is at `open`, to the
/// paren that closes it however many lines that takes, counting no paren
/// that stands inside a string, a raw string or a char literal.
///
/// Reading to the end of the line instead would drop the argument of a
/// send site rustfmt wrapped, which is a command reaching the session
/// with nothing read off it. A literal paren is the same miss in both
/// directions: a `)` inside a string closes the argument a paren early
/// and drops every call written after it, and a `(` runs it to the end
/// of the file and reads calls no send site made.
fn argument(text: &str, open: usize) -> &str {
    let rest = &text[open + 1..];
    let bytes = rest.as_bytes();
    let mut depth = 1usize;
    let mut at = 0usize;
    while at < rest.len() {
        // a raw string reads its own backslashes and hashes: the escape step
        // below swallows the closing quote of `r"a\"`, and the hash form
        // carries quotes of its own that close nothing
        if let Some(hashes) = raw_string_hashes(bytes, at) {
            at += hashes + 2;
            while at < rest.len() && !raw_string_closes(bytes, at, hashes) {
                at += 1;
            }
            at += hashes + 1;
            continue;
        }
        match bytes[at] {
            b'"' => {
                at += 1;
                while at < rest.len() && bytes[at] != b'"' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
            }
            // a char literal closes within three bytes of its opener; a
            // quote that closes no further along is a lifetime, which
            // opens nothing to skip past
            b'\'' => {
                for width in [2usize, 3] {
                    if bytes.get(at + width) == Some(&b'\'') {
                        at += width;
                        break;
                    }
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[..at];
                }
            }
            _ => {}
        }
        at += 1;
    }
    rest
}

/// The functions `text` calls, ignoring method calls and macros.
///
/// The character in front of the name is what parts them: `.` opens a
/// method call and a macro's `!` leaves no name character in front of the
/// paren at all. A path-qualified call keeps its last segment, which is
/// the function's own name: an imported builder is the shape this walk
/// exists for, and skipping it read `hang::wedge_command(...)` as
/// something the session was never handed.
fn call_names(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for (end, _) in text.match_indices('(') {
        let mut start = end;
        while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            start -= 1;
        }
        if start == end {
            continue;
        }
        if start > 0 && matches!(bytes[start - 1], b'.' | b'!') {
            continue;
        }
        found.push(text[start..end].to_string());
    }
    found
}

/// Every function a file's `send`/`submitted` arguments call, above its
/// test module.
///
/// This is the population the literal walk cannot see: a command built by
/// a call carries no literal at the send site, so the send sites are read
/// instead of the strings. A builder is listed once however many sites
/// hand it a command -- the question each one asks is the same one.
fn command_builders(source: &str) -> Vec<String> {
    let mut body = String::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            break;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    let mut found = Vec::new();
    for call in ["send(", "submitted("] {
        for (at, _) in body.match_indices(call) {
            if at > 0 {
                let prev = body.as_bytes()[at - 1];
                if prev.is_ascii_alphanumeric() || prev == b'_' {
                    continue;
                }
            }
            for builder in call_names(argument(&body, at + call.len() - 1)) {
                if !found.contains(&builder) {
                    found.push(builder);
                }
            }
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
         `silent`, or declare it here -- as the measured action, or as a view \
         command that emits no message -- with grounds.",
        undeclared.join("\n  ")
    );
}

// What the send-site walk reads: a builder defined beside the send site,
// one imported from elsewhere, one reached through its module path, one
// whose send site rustfmt wrapped, five whose literals carry a paren, and
// the three call shapes that build no command. The two spellings in the
// middle are the ones a line-at-a-time walk that skipped a path-qualified
// name read as nothing at all, and the literal parens are the two
// directions a paren counted inside a literal misreads the argument in,
// one for each shape a literal comes in: a string, a raw string in either
// spelling, and a char. The lifetime stands in front of them because a
// quote read as an opener swallows the parens between it and the next one.
#[test]
fn the_walk_reads_every_function_a_send_site_hands_a_command() {
    let source = concat!(
        "    session.send(flood_command().as_bytes())?;\n",
        "    session.send(submitted(&wedge_command(BOUND)).as_bytes())?;\n",
        "    session.send(submitted(&hang::wedge_release(BOUND)).as_bytes())?;\n",
        "    session.send(\n",
        "        history_command(SAMPLE).as_bytes(),\n",
        "    )?;\n",
        "    session.send(format!(\":silent e {file}\\r\").as_bytes())?;\n",
        "    session.send(&WALK_STEP.repeat(DEFAULT_CAPACITY))?;\n",
        "    session.send(OPEN_COMMAND)?;\n",
        "    session.send(submitted(&paren_command(\")\")).repeat(count_of(SAMPLE)).as_bytes())?;\n",
        "    session.send(open_command(\"(\").as_bytes())?;\n",
        "    session.send(lifetime_command(SAMPLE as &'a str).as_bytes())?;\n",
        "    session.send(submitted(&char_command(')')).repeat(char_count(SAMPLE)).as_bytes())?;\n",
        "    session.send(char_open('(').as_bytes())?;\n",
        "    session.send(submitted(&raw_command(r\"a\\\")).repeat(raw_count(SAMPLE)).as_bytes())?;\n",
        "    session.send(submitted(&hash_command(r#\"a \")\"#)).repeat(hash_count(SAMPLE)).as_bytes())?;\n",
        "    let bound = wedge_bound(SAMPLE);\n",
        "    #[cfg(test)]\n",
        "    session.send(only_in_tests())?;\n",
    );
    assert_eq!(
        command_builders(source),
        vec![
            "flood_command".to_string(),
            "submitted".to_string(),
            "wedge_command".to_string(),
            "wedge_release".to_string(),
            "history_command".to_string(),
            "paren_command".to_string(),
            "count_of".to_string(),
            "open_command".to_string(),
            "lifetime_command".to_string(),
            "char_command".to_string(),
            "char_count".to_string(),
            "char_open".to_string(),
            "raw_command".to_string(),
            "raw_count".to_string(),
            "hash_command".to_string(),
            "hash_count".to_string(),
        ],
        "a builder is read wherever a send site hands it a command -- through a \
         module path, across the line the argument wrapped onto, and past a paren \
         a literal carries -- and a method call, a macro or a constant builds none"
    );
}

#[test]
fn every_command_built_outside_this_crate_is_declared_by_its_builder() {
    let sources = driver_sources(&source_dir());
    let mut undeclared = Vec::new();
    for (file, source) in &sources {
        for builder in command_builders(source) {
            let written_here = sources
                .iter()
                .any(|(_, body)| body.contains(&format!("fn {builder}(")));
            if written_here {
                continue;
            }
            let declared = UNSILENCED
                .iter()
                .any(|entry| entry.file == *file && entry.built_by == builder);
            if !declared {
                undeclared.push(format!("{file}: {builder}()"));
            }
        }
    }
    assert!(
        undeclared.is_empty(),
        "a command built outside this crate reaches the session with no literal \
         for the silence walk to grade:\n  {}\nDeclare it here with the function \
         that builds it in `built_by` and its grounds, or build it beside the send \
         site so its literal is read.",
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
