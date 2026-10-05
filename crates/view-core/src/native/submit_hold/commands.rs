//! Which commands a `:` line runs, read the way nvim splits it at `|`.

use unicode_segmentation::UnicodeSegmentation;

/// How many of the `|`-separated commands on `line` are `View` or an
/// abbreviation nvim would run as it. `View` ends at a `|`, and any other
/// command that reads the `|` as its argument ends the reading.
pub(super) fn view_commands(line: &str) -> usize {
    let mut rest = line;
    let mut count = 0;
    loop {
        let command = skip_modifiers(rest);
        let word = command_word(command);
        let view = word.starts_with('V') && "View".starts_with(word);
        count += usize::from(view);
        if !view && takes_bar(command) {
            return count;
        }
        let Some(bar) = unescaped(command, '|') else {
            return count;
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

/// Whether `word` is `full` or an abbreviation nvim runs as it, given the
/// fewest characters (`shortest`) it accepts.
fn abbreviates(word: &str, full: &str, shortest: usize) -> bool {
    word.len() >= shortest && full.starts_with(word)
}

/// The commands `:help :bar` lists as reading a `|` as their argument,
/// with the fewest characters nvim accepts for each. `:read !`,
/// `:write !`, `:[range]!` and user commands are read by [`takes_bar`].
pub const TAKES_BAR: [(&str, usize); 29] = [
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
    ("lsp", 3),
    ("make", 3),
    ("normal", 4),
    ("perlfile", 5),
    ("pyfile", 3),
    ("python", 2),
    ("registers", 3),
    ("sign", 3),
    ("tabdo", 4),
    ("terminal", 2),
    ("vglobal", 1),
    ("windo", 4),
];

/// The script commands the engine's command table gives no `|` handling,
/// which `:help :bar` leaves out, with the fewest characters nvim accepts
/// for each.
pub const SCRIPT_COMMANDS: [(&str, usize); 20] = [
    ("lua", 3),
    ("luado", 4),
    ("luafile", 4),
    ("perl", 2),
    ("perldo", 5),
    ("py3", 3),
    ("py3do", 4),
    ("py3file", 4),
    ("pydo", 3),
    ("python3", 7),
    ("pythonx", 7),
    ("pyx", 3),
    ("pyxdo", 4),
    ("pyxfile", 4),
    ("ruby", 3),
    ("rubydo", 5),
    ("rubyfile", 5),
    ("tcl", 3),
    ("tcldo", 4),
    ("tclfile", 4),
];

/// The command modifiers `:help :command-modifiers` lists that run a user
/// command written after them on the same line, with the fewest characters
/// nvim's modifier parser accepts for each. `:sandbox` is on that list and
/// refuses a user command, so it is left out.
pub const MODIFIERS: [(&str, usize); 22] = [
    ("aboveleft", 3),
    ("belowright", 3),
    ("botright", 2),
    ("browse", 3),
    ("confirm", 4),
    ("hide", 3),
    ("horizontal", 3),
    ("keepalt", 5),
    ("keepjumps", 5),
    ("keepmarks", 3),
    ("keeppatterns", 5),
    ("leftabove", 5),
    ("lockmarks", 3),
    ("noautocmd", 3),
    ("noswapfile", 3),
    ("rightbelow", 6),
    ("silent", 3),
    ("tab", 3),
    ("topleft", 2),
    ("unsilent", 3),
    ("verbose", 4),
    ("vertical", 4),
];

/// `:filter`, with the fewest characters nvim's modifier parser accepts.
/// `:help :command-modifiers` leaves it off because `<mods>` does not carry
/// it, and the parser runs the command written after its pattern.
pub const FILTER: (&str, usize) = ("filter", 4);

/// `command` past its leading colons and the modifiers written before its
/// name, each with the count or `!` it takes, and `:filter` with its
/// pattern.
fn skip_modifiers(command: &str) -> &str {
    let mut rest = command;
    loop {
        rest = rest.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        let counted = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        let word = command_word(counted);
        let after = &counted[word.len()..];
        rest = if abbreviates(word, FILTER.0, FILTER.1) {
            skip_filter_pattern(after)
        } else if MODIFIERS
            .iter()
            .any(|&(full, shortest)| abbreviates(word, full, shortest))
        {
            after.trim_start_matches('!')
        } else {
            return rest;
        };
    }
}

/// `after` past the `!` and the pattern a `:filter` takes. A pattern opened
/// by a character that cannot start a name ends where [`pattern_end`] finds
/// that character again, followed by any of the `g`, `j` and `f` flags
/// `:vimgrep` takes, and any other pattern ends at whitespace.
fn skip_filter_pattern(after: &str) -> &str {
    let after = after.trim_start();
    let pattern = after.strip_prefix('!').unwrap_or(after).trim_start();
    match pattern.chars().next() {
        // nvim tests the first byte against `'isident'`, whose default
        // holds every byte that starts a multibyte character
        Some(delimiter)
            if !(delimiter.is_ascii_alphanumeric()
                || delimiter == '_'
                || !delimiter.is_ascii()) =>
        {
            let body = &pattern[delimiter.len_utf8()..];
            pattern_end(body, delimiter).map_or("", |end| {
                body[end + delimiter.len_utf8()..].trim_start_matches(['g', 'j', 'f'])
            })
        }
        _ => pattern.trim_start_matches(|c: char| !c.is_whitespace()),
    }
}

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
    let filter = after.trim_start().starts_with('!');
    let listed = |table: &[(&str, usize)]| {
        table
            .iter()
            .any(|&(full, shortest)| abbreviates(word, full, shortest))
    };
    word.starts_with(|c: char| c.is_ascii_uppercase())
        || abbreviates(word, "read", 1) && filter
        // `:w!` forces the write, and only `:w !` filters
        || abbreviates(word, "write", 1) && filter && after.starts_with(char::is_whitespace)
        || listed(&TAKES_BAR)
        || listed(&SCRIPT_COMMANDS)
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
                pattern_end(pattern, delimiter).map_or("", |end| &pattern[end + 1..])
            }
            Some('\\') if rest[1..].starts_with(['/', '?', '&']) => &rest[2..],
            _ => return rest,
        };
    }
}

/// Where the first `target` in `text` stands that no backslash makes part
/// of the text.
fn unescaped(text: &str, target: char) -> Option<usize> {
    let mut escaped = false;
    text.char_indices().find_map(|(at, c)| {
        let found = !escaped && c == target;
        escaped = !escaped && c == '\\';
        found.then_some(at)
    })
}

/// Where `delimiter` closes the search pattern `body` opens, found the way
/// nvim's `skip_regexp` finds it: a backslash takes the character after it,
/// and a collection holds the delimiter as one of its characters. `\v` and
/// `\V` switch the pattern to and from magic, where `[` opens a collection
/// and `\[` does without it. `None` when nothing closes the pattern.
fn pattern_end(body: &str, delimiter: char) -> Option<usize> {
    let mut magic = true;
    let mut at = 0;
    while let Some(c) = body[at..].chars().next() {
        if c == delimiter {
            return Some(at);
        }
        let after = &body[at + c.len_utf8()..];
        let step = match (c, after.chars().next()) {
            ('[', _) if magic => {
                at = collection_end(body, at + 1)?;
                1
            }
            // nvim reads this collection from the `[` itself, so a `^`, `]`
            // or `-` after it reads as it would further in
            ('\\', Some('[')) if !magic => {
                at = collection_end(body, at + 1)?;
                1
            }
            ('\\', Some(next)) => {
                // nvim's skip leaves `\m` and `\M` in magic, so `\M[` still
                // opens a collection there
                match next {
                    'v' => magic = true,
                    'V' => magic = false,
                    _ => {}
                }
                at += 1;
                next.len_utf8()
            }
            _ => c.len_utf8(),
        };
        at += step;
    }
    None
}

/// The character classes a collection names as `[:name:]`.
const CHARACTER_CLASSES: [&str; 19] = [
    "alnum",
    "alpha",
    "backspace",
    "blank",
    "cntrl",
    "digit",
    "escape",
    "fname",
    "graph",
    "ident",
    "keyword",
    "lower",
    "print",
    "punct",
    "return",
    "space",
    "tab",
    "upper",
    "xdigit",
];

/// Where the `]` closing the collection that opens before `start` stands in
/// `body`, read the way nvim's `skip_anyof` reads one. `None` when the
/// collection runs to the end of `body`.
fn collection_end(body: &str, start: usize) -> Option<usize> {
    let mut rest = &body[start..];
    rest = rest.strip_prefix('^').unwrap_or(rest);
    rest = rest.strip_prefix([']', '-']).unwrap_or(rest);
    loop {
        let mut chars = rest.chars();
        match chars.next()? {
            ']' => return Some(body.len() - rest.len()),
            '-' if !chars.as_str().starts_with(']') => {
                chars.next();
            }
            '\\' if chars
                .as_str()
                .starts_with(|c: char| "]^-n\\rtebdoxuU".contains(c)) =>
            {
                chars.next();
            }
            '[' => {
                let item = chars.as_str();
                // nvim takes a character with its composing characters, and
                // stops the cluster at an ASCII byte
                let single = |mark: char| {
                    let inner = item.strip_prefix(mark)?;
                    let cluster = inner.graphemes(true).next()?;
                    let first = cluster.chars().next()?.len_utf8();
                    let len = cluster[first..]
                        .find(|c: char| c.is_ascii())
                        .map_or(cluster.len(), |cut| first + cut);
                    inner[len..].strip_prefix(mark)?.strip_prefix(']')
                };
                let class = || {
                    let name = item.strip_prefix(':')?;
                    CHARACTER_CLASSES
                        .iter()
                        .find_map(|class| name.strip_prefix(class)?.strip_prefix(":]"))
                };
                if let Some(past) = class().or_else(|| single('=')).or_else(|| single('.')) {
                    chars = past.chars();
                }
            }
            _ => {}
        }
        rest = chars.as_str();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_word_is_read_with_its_abbreviations() {
        for line in ["View ai open", "Vie ai", "  :View", "View!", "V"] {
            assert!(view_commands(line) > 0, "{line:?}");
        }
        for line in ["", "vim", "Vex", "Views", "set ft=View", "edit View"] {
            assert_eq!(view_commands(line), 0, "{line:?}");
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
            "hide|View",
        ] {
            assert!(view_commands(line) > 0, "{line:?}");
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
            "tabdo e|View",
            "lsp restart|View",
            "lua print(1)|View",
            "py3file x.py|View",
            "rubyd x|View",
            "tcl x|View",
            "silent normal x|View",
            "echo 'a\\|View'",
            "'<,'>normal x|View",
            "/a\\/b/normal x|View",
            "/[/]/normal x|View",
        ] {
            assert_eq!(view_commands(line), 0, "{line:?}");
        }
    }

    /// A modifier runs the command after it, so `View` written behind one
    /// or several is the command the line runs.
    #[test]
    fn a_view_command_behind_a_modifier_is_read() {
        for line in [
            "silent View ai open",
            "silent! View",
            "sil View",
            "vert View",
            "vertical View",
            "tab View",
            "3tab View",
            "2verbose View",
            "topleft View",
            "botright vert View",
            "aboveleft keepalt View",
            "belowright keepjumps keepmarks keeppatterns View",
            "lockmarks noautocmd noswapfile confirm View",
            "hide unsilent View",
            "w|silent View",
            "filter /x/ View",
            "filt! x View",
            "filter /a\\/b/ View",
            "filter /[/]/ View",
            "filter /a[/]b/ View",
            "filter /[[:alpha:]/]/ View",
            "filter /[^]/]/ View",
            "filter /[\\]/]/ View",
            "filter /x/g View",
            "filter /x/j View",
            "filter /x/gjf View",
            "filter /\\V[/ View",
            "filter /\\V\\[/]/ View",
            "filter /\\M[/]/ View",
            "filter /\\V\\[:alpha:]/]/ View",
            "filter /\\V\\[=a=]/]/ View",
            // a decomposed é, `e` and U+0301
            "filter /[[=e\u{301}=]/]/ View",
            "filter /[[.e\u{301}.]/]/ View",
            "bel View",
            "hor View",
        ] {
            assert!(view_commands(line) > 0, "{line:?}");
        }
        for line in [
            "silent echo 1",
            "si View",
            "3View",
            "silentView",
            "be View",
            "ho View",
            "sandbox View",
            "fil /x/ View",
            "filter View",
            "filter /x View",
            "filter /[/ View",
            "filter /\\M\\[/]/ View",
            "filter /[[:nope:]/]/ View",
            "filter /\\V\\[]/]/ View",
            "filter «x y« View",
            "filter \u{2014}x y\u{2014} View",
        ] {
            assert_eq!(view_commands(line), 0, "{line:?}");
        }
    }
}
