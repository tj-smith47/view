//! The walk that keeps every `nvim_exec_lua` call in this crate at the two
//! parameters nvim requires: the chunk, and the array of its arguments.
//!
//! nvim refuses any other count with an error reply, and an async waiter
//! resolves that reply to its safe default, so a call with one parameter
//! answers every time and answers nothing. The picker's buffer list shipped
//! that way and opened empty in every session.
//!
//! Every source file under `crates/view-engine/src` is read with its inline
//! test modules taken out, so a call added anywhere in the shipped code is
//! checked with no edit here. The source is read through
//! `view_test_support::rust_source`, which blanks comments and literals
//! first, so a comma or a bracket either one carries is never counted.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use view_test_support::rust_source::{blank_non_code, closing, item_end, top_level_items};

const METHOD: &str = "\"nvim_exec_lua\"";

fn rust_sources(dir: &Path, into: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a readable source directory") {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            rust_sources(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(path);
        }
    }
}

/// Blanked `code` with every inline `#[cfg(test)]` module blanked as well,
/// so its fixtures are not read and the code after it still is.
///
/// Other attributes may stand between the `cfg` and the `mod`. An
/// out-of-line `mod name;` opens no body here, so the walk reads on past
/// it.
fn without_test_modules(code: &str) -> String {
    let mut out = code.to_owned();
    let mut from = 0;
    while let Some(offset) = code[from..].find("#[cfg(test)]") {
        let at = from + offset;
        from = at + 1;
        let mut rest = code[at + "#[cfg(test)]".len()..].trim_start();
        while rest.starts_with("#[") {
            let Some(close) = closing(rest, 1) else {
                break;
            };
            rest = rest[close + 1..].trim_start();
        }
        let declaration = rest.strip_prefix("pub ").unwrap_or(rest);
        let Some(after_mod) = declaration.strip_prefix("mod ") else {
            continue;
        };
        let name_end = after_mod
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after_mod.len());
        if !after_mod[name_end..].trim_start().starts_with('{') {
            continue;
        }
        let open = code.len() - after_mod.len() + name_end;
        let open = open + code[open..].find('{').unwrap_or(0);
        let end = closing(code, open).map_or(code.len(), |close| close + 1);
        out.replace_range(at..end, &code[at..end].replace(|c: char| c != '\n', " "));
    }
    out
}

/// The number of parameters the call naming `"nvim_exec_lua"` at `at`
/// sends, read from the argument after the method name: it has to be a
/// `vec![...]`, field-labelled or not, and anything else is an error
/// naming what stands there.
fn parameters(code: &str, at: usize) -> Result<usize, String> {
    let method_end = item_end(code, at + METHOD.len());
    if code.as_bytes().get(method_end) != Some(&b',') {
        return Err("no argument follows the method name".to_owned());
    }
    let start = method_end + 1;
    let argument = code[start..item_end(code, start)].trim();
    let unlabelled = match argument.split_once(':') {
        Some((label, value))
            if !value.starts_with(':')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            value.trim_start()
        }
        _ => argument,
    };
    let list = unlabelled
        .strip_prefix("vec!")
        .map(str::trim_start)
        .filter(|list| list.starts_with('['))
        .ok_or_else(|| format!("expected a vec![...] of parameters, found `{unlabelled}`"))?;
    let close = closing(list, 0).ok_or("the vec![...] never closes")?;
    Ok(top_level_items(&list[1..close]).len())
}

/// Every `"nvim_exec_lua"` call in `source` outside its test modules, by
/// line number, with the parameter count its params argument carries.
fn sites(source: &str) -> Vec<(usize, Result<usize, String>)> {
    let code = without_test_modules(&blank_non_code(source));
    source
        .match_indices(METHOD)
        // a call's method name is a literal in code, which blanking keeps
        // the quotes of; a comment's or a test module's is spaces
        .filter(|(at, _)| {
            code.as_bytes()[*at] == b'"' && code.as_bytes()[at + METHOD.len() - 1] == b'"'
        })
        .map(|(at, _)| {
            let line = source[..at].matches('\n').count() + 1;
            (line, parameters(&code, at))
        })
        .collect()
}

#[test]
fn every_exec_lua_call_passes_the_chunk_and_its_argument_array() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    sources.sort();
    let mut checked = 0;
    let mut wrong = Vec::new();
    for path in &sources {
        let source = std::fs::read_to_string(path).expect("a readable source file");
        for (line, params) in sites(&source) {
            checked += 1;
            match params {
                Ok(2) => {}
                Ok(n) => wrong.push(format!("{}:{line}: {n} parameters", path.display())),
                Err(why) => wrong.push(format!("{}:{line}: {why}", path.display())),
            }
        }
    }
    assert!(
        checked > 0,
        "no nvim_exec_lua call found under {}",
        root.display()
    );
    assert!(
        wrong.is_empty(),
        "nvim_exec_lua takes exactly (chunk, args), and these calls send \
         another count:\n{}",
        wrong.join("\n")
    );
}

/// The one-parameter call, written in each shape that hides a separator or
/// a bracket from a reader that does not know Rust's comments, literals,
/// closures and generics, still reads as one parameter.
#[test]
fn a_one_parameter_call_reads_as_one_whatever_it_carries() {
    let shapes = [
        ("a trailing comment", "vec![Value::from(C) // a, b]\n]"),
        ("a block comment", "vec![Value::from(C) /* , x ] */]"),
        ("a char literal", "vec![Value::from(pick('\"', '('))]"),
        ("a raw string", "vec![Value::from(r\"(\\\" + \")\")]"),
        ("a hashed raw string", "vec![r#\"a \", \" b\"#]"),
        ("a closure", "vec![|a, b| a + b]"),
        ("a turbofish", "vec![HashMap::<A, B>::new()]"),
        ("a lifetime", "vec![x as &'a str + y(','), ]"),
    ];
    let misread: Vec<String> = shapes
        .iter()
        .filter_map(|(shape, params)| {
            let source = format!("h.request(\"nvim_exec_lua\", {params});\n");
            let read = sites(&source);
            (read != vec![(1, Ok(1))]).then(|| format!("{shape}: {source:?} read as {read:?}"))
        })
        .collect();
    assert!(misread.is_empty(), "{}", misread.join("\n"));
}

/// The params argument is the one after the method name, and a call whose
/// params are anything but a `vec![...]` is reported.
#[test]
fn the_params_are_the_argument_after_the_method_name() {
    let bound = "h.request(\"nvim_exec_lua\", p, vec![a, b]);\n";
    assert_eq!(
        sites(bound),
        vec![(
            1,
            Err("expected a vec![...] of parameters, found `p`".to_owned())
        )]
    );
    let field = "RpcMessage::Notification {\n    method: \"nvim_exec_lua\".to_owned(),\n    params: vec![c],\n}\n";
    assert_eq!(sites(field), vec![(2, Ok(1))]);
    let alone = "h.notify(\"nvim_exec_lua\");\n";
    assert_eq!(
        sites(alone),
        vec![(1, Err("no argument follows the method name".to_owned()))]
    );
}

/// An inline test module is skipped whatever attributes stand on it, and
/// the code after it, or after an out-of-line test module, is read.
#[test]
fn the_walk_skips_only_the_test_modules_own_bodies() {
    let source = [
        "fn shipped() {",
        "    // h.request(\"nvim_exec_lua\", vec![one]);",
        "    h.request(\"nvim_exec_lua\", vec![one]);",
        "}",
        "#[cfg(test)]",
        "mod out_of_line;",
        "fn after_declaration() { h.request(\"nvim_exec_lua\", vec![two]); }",
        "#[cfg(test)]",
        "#[allow(clippy::unwrap_used)]",
        "mod tests {",
        "    fn fixture() { h.request(\"nvim_exec_lua\", vec![]); }",
        "}",
        "fn after_module() { h.request(\"nvim_exec_lua\", vec![three]); }",
    ]
    .join("\n");
    assert_eq!(sites(&source), vec![(3, Ok(1)), (7, Ok(1)), (13, Ok(1))]);
}
