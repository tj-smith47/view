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
/// A comma inside a nested bracket, a turbofish (`::<A, B>`) or a
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
            b'<' if code.get(..at).is_some_and(|head| head.ends_with("::")) || angles > 0 => {
                angles += 1;
            }
            b'>' if angles > 0 && at > 0 && bytes[at - 1] != b'-' => angles -= 1,
            b'|' if depth == 0 && angles == 0 && opens_closure(code, from, at) => {
                at = find_from(bytes, at + 1, b'|').unwrap_or(bytes.len());
            }
            b',' if depth == 0 && angles == 0 => return at,
            _ => {}
        }
        at += 1;
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
}
