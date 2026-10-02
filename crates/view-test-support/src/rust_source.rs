//! A reader for Rust source text, for tests that check the shape of calls
//! written in a crate's own source.
//!
//! [`blank_non_code`] turns every comment and the contents of every string,
//! raw string and char literal into spaces, byte for byte, so a bracket or
//! a comma a literal or a comment carries is no longer code. The text keeps
//! its length and its line breaks, so an offset or a line number found in
//! the blanked text names the same place in the original. The other
//! functions read the blanked text and so never meet a literal.

/// `source` with every comment and every literal's contents replaced by
/// spaces, keeping each newline and each literal's delimiters.
///
/// A quote that closes no char literal within the next character is a
/// lifetime, and is left as code.
#[must_use]
pub fn blank_non_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for byte in out.iter_mut().take(to).skip(from) {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    let mut at = 0;
    while at < bytes.len() {
        let next = bytes.get(at + 1).copied();
        if let Some(hashes) = raw_string_hashes(bytes, at) {
            let open = at + hashes + 2;
            let mut close = open;
            while close < bytes.len() && !raw_string_closes(bytes, close, hashes) {
                close += 1;
            }
            blank(&mut out, open, close);
            at = close + hashes + 1;
            continue;
        }
        match bytes[at] {
            b'/' if next == Some(b'/') => {
                let end = find_from(bytes, at, b'\n').unwrap_or(bytes.len());
                blank(&mut out, at, end);
                at = end;
            }
            b'/' if next == Some(b'*') => {
                let end = block_comment_end(bytes, at);
                blank(&mut out, at, end);
                at = end;
            }
            b'"' => {
                let mut close = at + 1;
                while close < bytes.len() && bytes[close] != b'"' {
                    close += if bytes[close] == b'\\' { 2 } else { 1 };
                }
                blank(&mut out, at + 1, close);
                at = close + 1;
            }
            b'\'' => match char_literal_end(source, at) {
                Some(close) => {
                    blank(&mut out, at + 1, close);
                    at = close + 1;
                }
                None => at += 1,
            },
            _ => at += 1,
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

fn find_from(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    bytes
        .iter()
        .skip(from)
        .position(|b| *b == needle)
        .map(|offset| from + offset)
}

/// The end of the block comment opening at `at`, past its own nested
/// comments.
fn block_comment_end(bytes: &[u8], at: usize) -> usize {
    let mut depth = 0usize;
    let mut i = at;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1).copied()) {
            (b'/', Some(b'*')) => {
                depth += 1;
                i += 2;
            }
            (b'*', Some(b'/')) => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return i;
                }
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

/// The hash count of the raw string (`r"..."`, `r#"..."#`) opening at
/// `at`, or `None` where no raw string opens there.
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

fn raw_string_closes(bytes: &[u8], at: usize, hashes: usize) -> bool {
    bytes.get(at) == Some(&b'"') && (0..hashes).all(|k| bytes.get(at + 1 + k) == Some(&b'#'))
}

/// The offset of the quote closing the char literal that opens at `at`,
/// or `None` when the quote opens a lifetime.
fn char_literal_end(source: &str, at: usize) -> Option<usize> {
    let rest = source.get(at + 1..)?;
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if first == '\\' {
        // the escaped char may itself be a quote (`'\''`), so the search
        // starts past it
        return rest.get(2..)?.find('\'').map(|end| at + 3 + end);
    }
    let (second_at, second) = chars.next()?;
    (second == '\'').then_some(at + 1 + second_at)
}

/// The offset of the bracket closing the one at `open` in blanked `code`.
#[must_use]
pub fn closing(code: &str, open: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 0usize;
    for (at, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// Where the list item starting at `from` in blanked `code` ends: the
/// offset of the comma that separates it from the next item, or of the
/// bracket that closes the list, or the end of the text.
///
/// A comma inside a nested bracket, a turbofish (`::<A, B>`), a type's
/// generics after `as` or a closure's `->` (`as Map<A, B>`), or a
/// closure's parameter list (`|a, b|`) belongs to the item.
#[must_use]
pub fn item_end(code: &str, from: usize) -> usize {
    let bytes = code.as_bytes();
    let mut depth = 0usize;
    let mut angles = 0usize;
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return at,
            },
            b'<' if angles > 0 || opens_generics(code, from, at) => angles += 1,
            b'>' if angles > 0 && at > 0 && bytes[at - 1] != b'-' => angles -= 1,
            b'|' if depth == 0 && angles == 0 && opens_closure(code, from, at) => {
                at = closure_parameters_end(bytes, at);
            }
            b',' if depth == 0 && angles == 0 => return at,
            _ => {}
        }
        at += 1;
    }
    bytes.len()
}

/// Whether the `<` at `at` opens generics: it follows `::`, or it follows a
/// type path written after `as` or `->`, where Rust reads `<` as nothing
/// else.
fn opens_generics(code: &str, from: usize, at: usize) -> bool {
    let Some(head) = code.get(from..at) else {
        return false;
    };
    if head.ends_with("::") {
        return true;
    }
    let path = head.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_' || c == ':');
    if path.len() == head.len() {
        return false;
    }
    let mut before = path.trim_end();
    // the references and qualifiers a type may open with
    loop {
        if let Some(rest) = before.strip_suffix(['&', '*']) {
            before = rest.trim_end();
            continue;
        }
        let word_at = before
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '\''))
            .map_or(0, |i| i + 1);
        let word = before.get(word_at..).unwrap_or_default();
        if word.starts_with('\'') || matches!(word, "mut" | "const" | "dyn" | "impl") {
            before = before.get(..word_at).unwrap_or_default().trim_end();
            continue;
        }
        return word == "as" || before.ends_with("->");
    }
}

/// The offset of the `|` closing the closure parameters opened at `open`,
/// past any `|` an or-pattern carries inside a bracket.
fn closure_parameters_end(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    for (at, byte) in bytes.iter().enumerate().skip(open + 1) {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'|' if depth == 0 => return at,
            _ => {}
        }
    }
    bytes.len()
}

/// Whether the `|` at `at` opens a closure's parameters: it stands where
/// the item starts, alone or after `move`.
fn opens_closure(code: &str, from: usize, at: usize) -> bool {
    matches!(code.get(from..at).map(str::trim), Some("" | "move"))
}

/// The items of the list whose contents are blanked `inner` (the text
/// between a bracket and its closer), trimmed, with the empty item a
/// trailing comma leaves dropped.
#[must_use]
pub fn top_level_items(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut from = 0;
    while from <= inner.len() {
        let end = item_end(inner, from);
        let item = inner.get(from..end).unwrap_or_default().trim();
        if !item.is_empty() {
            items.push(item);
        }
        from = end + 1;
    }
    items
}

/// `source` with every inline `#[cfg(test)]` module turned into spaces byte
/// for byte, keeping each newline, so its fixtures are not read and every
/// offset after it names the same place. Comments and literals outside
/// those modules are kept, so a blanked or an unblanked source can be read
/// through it.
///
/// Other attributes may stand between the `cfg` and the `mod`, and the
/// module may carry any visibility. An out-of-line `mod name;` opens no
/// body, so the text after it is kept.
#[must_use]
pub fn without_test_modules(source: &str) -> String {
    const MARK: &str = "#[cfg(test)]";
    let code = blank_non_code(source);
    let mut out = source.as_bytes().to_vec();
    let mut from = 0;
    while let Some(offset) = code.get(from..).and_then(|rest| rest.find(MARK)) {
        let at = from + offset;
        from = at + 1;
        let Some(end) = test_module_end(&code, at + MARK.len()) else {
            continue;
        };
        for byte in out.iter_mut().take(end).skip(at) {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// The offset just past the inline module body that the attributes ending
/// at `after` in blanked `code` stand on, or `None` where they stand on
/// anything else.
fn test_module_end(code: &str, after: usize) -> Option<usize> {
    let mut rest = code.get(after..)?.trim_start();
    while rest.starts_with("#[") {
        let close = closing(rest, 1)?;
        rest = rest.get(close + 1..)?.trim_start();
    }
    let after_mod = without_visibility(rest).strip_prefix("mod")?;
    if !after_mod.starts_with(char::is_whitespace) {
        return None;
    }
    let after_mod = after_mod.trim_start();
    let name_end = after_mod
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(after_mod.len());
    let body = after_mod.get(name_end..)?.trim_start();
    if !body.starts_with('{') {
        return None;
    }
    let open = code.len() - body.len();
    Some(closing(code, open).map_or(code.len(), |close| close + 1))
}

/// `item` past a leading `pub`, `pub(crate)`, `pub(in path)` or the like.
fn without_visibility(item: &str) -> &str {
    let Some(after) = item.strip_prefix("pub") else {
        return item;
    };
    let scoped = after.trim_start();
    if scoped.starts_with('(') {
        return closing(scoped, 0)
            .and_then(|close| scoped.get(close + 1..))
            .map_or(item, str::trim_start);
    }
    if after.starts_with(char::is_whitespace) {
        after.trim_start()
    } else {
        item
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn items_of(list: &str) -> Vec<String> {
        let code = blank_non_code(list);
        let close = closing(&code, 0).unwrap();
        top_level_items(&code[1..close])
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn blanking_keeps_length_lines_and_delimiters() {
        let source = "a(\"x, )\", 'y', r#\"q \"\"#) // c, )\n/* d, ( */ b";
        let code = blank_non_code(source);
        assert_eq!(code.len(), source.len());
        assert_eq!(code, "a(\"    \", ' ', r#\"   \"#)        \n           b");
    }

    #[test]
    fn a_comment_or_literal_carries_no_separator() {
        assert_eq!(items_of("[f(x) // a, b\n]").len(), 1);
        assert_eq!(items_of("[f(x) /* a, ] */]").len(), 1);
        assert_eq!(items_of("[f('\"'), ]").len(), 1);
        assert_eq!(items_of("[f(','), g('\\'')]").len(), 2);
        assert_eq!(items_of("[f(r\"a, \\\")]").len(), 1);
        assert_eq!(items_of("[f(r#\"a \"b, ]\" c\"#)]").len(), 1);
        assert_eq!(items_of("[f(br\"a, ]\")]").len(), 1);
    }

    /// A test module is blanked byte for byte, so a character of several
    /// bytes inside it moves nothing after it, and a module of any
    /// visibility or a non-ASCII name is found.
    #[test]
    fn a_test_module_is_blanked_in_place_whatever_its_name_or_visibility() {
        for module in [
            "#[cfg(test)]\nmod tests {\n    fn é() { f(\"x\") }\n}\n",
            "#[cfg(test)]\npub(crate) mod tests {\n    fn g() {}\n}\n",
            "#[cfg(test)]\npub(in crate::a) mod tests {\n    fn g() {}\n}\n",
            "#[cfg(test)]\n#[allow(x)]\npub mod tésts {\n    fn g() {}\n}\n",
        ] {
            let source = format!("fn shipped() {{}}\n{module}fn after() {{}}\n");
            let out = without_test_modules(&source);
            assert_eq!(out.len(), source.len(), "{source:?}");
            assert_eq!(out.lines().count(), source.lines().count(), "{source:?}");
            assert!(out.starts_with("fn shipped() {}\n"), "{out:?}");
            assert!(out.ends_with("\nfn after() {}\n"), "{out:?}");
            assert!(
                !out.contains("fn g") && !out.contains("fn é") && !out.contains("mod"),
                "the module is still read: {out:?}"
            );
        }
    }

    /// Comments are kept outside a test module, so a walk that reads doc
    /// comments can strip test modules too, and an out-of-line module
    /// keeps the code after it.
    #[test]
    fn only_an_inline_test_modules_body_is_blanked() {
        let source = "/// kept\nfn a() {}\n#[cfg(test)]\nmod outside;\nfn b() {}\n";
        assert_eq!(without_test_modules(source), source);
    }

    #[test]
    fn a_closure_turbofish_or_lifetime_carries_no_separator() {
        assert_eq!(items_of("[|a, b| a + b]").len(), 1);
        assert_eq!(items_of("[move |a, b| a, c]").len(), 2);
        assert_eq!(items_of("[a | b, c]").len(), 2);
        assert_eq!(items_of("[HashMap::<A, B>::new()]").len(), 1);
        assert_eq!(items_of("[f::<'a>(x), g(',')]").len(), 2);
        assert_eq!(items_of("[x as &'static str, ']']").len(), 2);
        assert_eq!(items_of("[a < b, c > d]").len(), 2);
    }

    /// A type after `as` or a closure's `->` takes generics with no
    /// turbofish, and a closure's parameters may carry an or-pattern
    /// inside brackets.
    #[test]
    fn a_generic_type_or_a_closure_pattern_carries_no_separator() {
        assert_eq!(items_of("[x as Map<A, B>, c]").len(), 2);
        assert_eq!(items_of("[x as &'a dyn Fn<A, B>, c]").len(), 2);
        assert_eq!(items_of("[x as &mut T<A, B>]").len(), 1);
        assert_eq!(items_of("[|x| -> Map<A, B> { m }, c]").len(), 2);
        assert_eq!(items_of("[|(Some(a) | None): T| a, c]").len(), 2);
        assert_eq!(items_of("[|a: Map<A, B>, b| a, c]").len(), 2);
        assert_eq!(items_of("[has < b, c > d]").len(), 2);
    }
}
