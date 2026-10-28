//! Guards the "read-only" project rule: no GraphQL mutations, and no
//! `query_path` pointing outside the `graphql/` directory.

use std::fs;
use std::path::{Path, PathBuf};

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

#[test]
fn graphql_files_contain_no_mutations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    files_with_ext(&root.join("graphql"), "graphql", &mut files);
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        for (i, line) in text.lines().enumerate() {
            let is_mutation = line
                .trim_start()
                .strip_prefix("mutation")
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'));
            assert!(
                !is_mutation,
                "mutation found in {}:{}: {}",
                file.display(),
                i + 1,
                line
            );
        }
    }
}

#[test]
fn no_graphql_query_derive_outside_graphql_dir() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    files_with_ext(&root.join("src"), "rs", &mut files);
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
            assert!(
                path.starts_with("graphql/"),
                "query_path {:?} in {} must start with graphql/",
                path,
                file.display()
            );
        }
    }
}
