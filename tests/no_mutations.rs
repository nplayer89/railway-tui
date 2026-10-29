//! Guards the mutation allowlist: GraphQL mutations live only in
//! `graphql/write/`, they use exactly four mutation fields, only
//! `src/api/write.rs` may derive from them, and no `query_path` points
//! outside the `graphql/` directory.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The only mutation fields the app may ever send.
const ALLOWED_MUTATIONS: [&str; 4] = [
    "variableCollectionUpsert",
    "variableDelete",
    "serviceInstanceRedeploy",
    "serviceInstanceDeploy",
];

fn files_with_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_with_ext(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// GraphQL text with `#` comments removed.
fn strip_comments(text: &str) -> String {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offsets where the keyword `mutation` starts as a whole word.
fn mutation_keywords(text: &str) -> Vec<usize> {
    text.match_indices("mutation")
        .filter(|&(i, m)| {
            let before = text[..i].chars().next_back();
            let after = text[i + m.len()..].chars().next();
            !before.is_some_and(is_name_char) && !after.is_some_and(is_name_char)
        })
        .map(|(i, _)| i)
        .collect()
}

/// The top-level field names in the selection set of the operation that starts at `start`.
/// Aliases (`alias: field`) report the real field; fragment spreads are refused.
fn top_level_fields(text: &str, start: usize, file: &Path) -> Vec<String> {
    let rest = &text[start..];
    let open = rest
        .find('{')
        .unwrap_or_else(|| panic!("mutation without a selection set in {}", file.display()));
    let body = &rest[open + 1..];
    let mut fields = Vec::new();
    let (mut braces, mut parens) = (1usize, 0usize);
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() && braces > 0 {
        let c = chars[i];
        match c {
            '{' => braces += 1,
            '}' => braces -= 1,
            '(' => parens += 1,
            ')' => parens -= 1,
            '.' if braces == 1 && parens == 0 => {
                panic!("fragment spread in a mutation in {}", file.display())
            }
            c if braces == 1 && parens == 0 && (c.is_alphabetic() || c == '_') => {
                let name: String = chars[i..]
                    .iter()
                    .take_while(|&&c| is_name_char(c))
                    .collect();
                i += name.chars().count();
                let next = chars[i..].iter().find(|c| !c.is_whitespace());
                // `alias:` is skipped; the real field name follows the colon.
                if next != Some(&':') {
                    fields.push(name);
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    fields
}

#[test]
fn mutations_live_only_in_graphql_write_and_match_the_allowlist() {
    let write_dir = root().join("graphql").join("write");
    let mut files = Vec::new();
    files_with_ext(&root().join("graphql"), "graphql", &mut files);
    let mut used = BTreeSet::new();
    for file in files {
        let text = strip_comments(&fs::read_to_string(&file).unwrap());
        let starts = mutation_keywords(&text);
        if starts.is_empty() {
            continue;
        }
        assert!(
            file.starts_with(&write_dir),
            "mutation outside graphql/write/: {}",
            file.display()
        );
        for start in starts {
            used.extend(top_level_fields(&text, start, &file));
        }
    }
    let allowed: BTreeSet<String> = ALLOWED_MUTATIONS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        used, allowed,
        "mutation fields must be exactly the allowlist"
    );
}

/// Every `query_path = "..."` in `src/`, with the file it appears in.
fn query_paths() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    files_with_ext(&root().join("src"), "rs", &mut files);
    let mut out = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        let mut rest = text.as_str();
        while let Some(idx) = rest.find("query_path") {
            rest = &rest[idx + "query_path".len()..];
            let Some(after) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let Some(after) = after.trim_start().strip_prefix('"') else {
                continue;
            };
            let path: String = after.chars().take_while(|&c| c != '"').collect();
            out.push((file.clone(), path));
        }
    }
    out
}

#[test]
fn no_graphql_query_derive_outside_graphql_dir() {
    for (file, path) in query_paths() {
        assert!(
            path.starts_with("graphql/"),
            "query_path {:?} in {} must start with graphql/",
            path,
            file.display()
        );
    }
}

#[test]
fn write_operations_are_derived_only_in_write_rs() {
    let write_rs = root().join("src").join("api").join("write.rs");
    let mut seen = 0;
    for (file, path) in query_paths() {
        if path.starts_with("graphql/write/") {
            assert_eq!(
                file, write_rs,
                "query_path {:?} must only appear in src/api/write.rs",
                path
            );
            seen += 1;
        }
    }
    assert_eq!(
        seen,
        ALLOWED_MUTATIONS.len(),
        "write.rs derives one operation per mutation"
    );
}
