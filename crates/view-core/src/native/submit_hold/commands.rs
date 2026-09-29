//! Which commands a `:` line runs, read the way nvim splits it at `|`.

/// Whether one of the `|`-separated commands on `line` is `View` or an
/// abbreviation nvim would run as it. A command that reads the `|` as its
/// argument ends the reading.
pub(super) fn names_view(line: &str) -> bool {
    let mut rest = line;
    loop {
        let command = rest.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        let word = command_word(command);
        if word.starts_with('V') && "View".starts_with(word) {
            return true;
        }
        if takes_bar(command) {
            return false;
        }
        let Some(bar) = next_bar(command) else {
            return false;
        };
        rest = &command[bar + 1..];
    }
}

/// The name a command starts with.
fn command_word(command: &str) -> &str {
    command
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default()
}

/// The commands `:help :bar` lists as reading a `|` as their argument,
/// with the shortest abbreviation nvim accepts for each. `:read !`,
/// `:write !` and `:[range]!` are read by [`takes_bar`], and the script
/// commands and `:tabdo` join the list because the engine's command
/// table gives them no `|` handling either.
const TAKES_BAR: [(&str, usize); 34] = [
    ("argdo", 5),
    ("autocmd", 2),
    ("bufdo", 4),
    ("cdo", 3),
    ("cfdo", 3),
    ("command", 3),
    ("debug", 3),
    ("eval", 2),
    ("folddoopen", 5),
    ("folddoclosed", 7),
    ("function", 2),
    ("global", 1),
    ("help", 1),
    ("helpgrep", 5),
    ("ldo", 2),
    ("lfdo", 3),
    ("lhelpgrep", 2),
    ("make", 3),
    ("normal", 4),
    ("perlfile", 5),
    ("pyfile", 3),
    ("python", 2),
    ("registers", 3),
    ("sign", 3),
    ("terminal", 2),
    ("vglobal", 1),
    ("windo", 4),
    ("tabdo", 4),
    ("lua", 3),
    ("luado", 4),
    ("luafile", 4),
    ("python3", 7),
    ("py3", 3),
    ("pyx", 3),
];

/// Whether `command` reads the rest of its line, `|` included, as its
/// argument. A user command reads it unless it was defined with `-bar`,
/// which view does not know.
fn takes_bar(command: &str) -> bool {
    let command = skip_range(command);
    if command.starts_with('!') {
        return true;
    }
    let word = command_word(command);
    let after = &command[word.len()..];
    let abbreviates =
        |full: &str, shortest: usize| word.len() >= shortest && full.starts_with(word);
    let filter = after.trim_start().starts_with('!');
    word.starts_with(|c: char| c.is_ascii_uppercase())
        || abbreviates("read", 1) && filter
        // `:w!` forces the write, and only `:w !` filters
        || abbreviates("write", 1) && filter && after.starts_with(char::is_whitespace)
        || TAKES_BAR
            .iter()
            .any(|&(full, shortest)| abbreviates(full, shortest))
}

/// `command` past the line range written before its name.
fn skip_range(command: &str) -> &str {
    let mut rest = command;
    loop {
        rest = rest.trim_start_matches(|c: char| {
            c.is_ascii_digit() || c.is_whitespace() || ".,;$%+-:".contains(c)
        });
        let mut chars = rest.chars();
        rest = match chars.next() {
            // a mark's name can be any character, `|` included
            Some('\'') => {
                chars.next();
                chars.as_str()
            }
            Some(delimiter @ ('/' | '?')) => {
                let pattern = chars.as_str();
                let mut escaped = false;
                let end = pattern.find(|c: char| {
                    let ends = !escaped && c == delimiter;
                    escaped = !escaped && c == '\\';
                    ends
                });
                end.map_or("", |end| &pattern[end + 1..])
            }
            Some('\\') if rest[1..].starts_with(['/', '?', '&']) => &rest[2..],
            _ => return rest,
        };
    }
}

/// Where the `|` that ends `command` stands. A backslash before one makes
/// it part of the argument.
fn next_bar(command: &str) -> Option<usize> {
    let mut escaped = false;
    command.char_indices().find_map(|(at, c)| {
        let ends = !escaped && c == '|';
        escaped = !escaped && c == '\\';
        ends.then_some(at)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_word_is_read_with_its_abbreviations() {
        for line in ["View ai open", "Vie ai", "  :View", "View!", "V"] {
            assert!(names_view(line), "{line:?}");
        }
        for line in ["", "vim", "Vex", "Views", "set ft=View", "edit View"] {
            assert!(!names_view(line), "{line:?}");
        }
    }

    /// Every `|`-separated command is read, up to one that takes the rest
    /// of the line as its argument.
    #[test]
    fn a_view_command_after_a_bar_is_read() {
        for line in [
            "w|View ai open",
            "w | :View",
            "%s/a/b/|View",
            "echo 1|w|View",
            "w!|View",
            "set ft=x\\|y|View",
        ] {
            assert!(names_view(line), "{line:?}");
        }
        for line in [
            "normal! ihello|View",
            "norm x|View",
            "g/x/d|View",
            "%g/x/d|View",
            "v/x/d|View",
            "!ls|View",
            "%!sort|View",
            "r !ls|View",
            "r!ls|View",
            "w !cat|View",
            "Foo x|View",
            "windo e|View",
            "lua print(1)|View",
            "echo 'a\\|View'",
            "'<,'>normal x|View",
        ] {
            assert!(!names_view(line), "{line:?}");
        }
    }
}
