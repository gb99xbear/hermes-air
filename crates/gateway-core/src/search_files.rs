//! Native, dependency-free fallback engine for the `search_files` tool.
//!
//! `ripgrep` is fast and stays the first choice, but it is an **external
//! binary**: on any host that does not ship `rg` the tool used to fail
//! outright with `success: false`. The Python original this crate is ported
//! from searches "via rg with find/grep fallbacks"; this module restores that
//! contract in-process, so `search_files` keeps working on a bare host (and a
//! clean CI runner needs no extra toolchain packages).
//!
//! Scope: the same behaviour the rg path already exposes - file-name search,
//! content search, `files_only` and `count` output modes, `file_glob`
//! filtering and `--sort modified`. Context lines (`-C`) are not emitted here,
//! matching what the current rg parser keeps.

use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

/// Directory names never descended into: build output and VCS metadata are
/// noise for every search, and `rg` skips them by default too.
const PRUNED_DIRS: [&str; 3] = ["target", "node_modules", ".git"];

/// One content match, shaped like the rg parser's output.
#[derive(Debug, Clone)]
pub struct NativeMatch {
    pub path: String,
    pub line: usize,
    pub content: String,
}

/// Turn a shell-style glob into an anchored regex. Only `*` and `?` are
/// wildcards; every other character is matched literally.
fn glob_to_regex(glob: &str) -> Option<Regex> {
    let mut re = String::from("^");
    for ch in glob.chars() {
        match ch {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).ok()
}

/// Match a candidate against a glob. A glob containing `/` is matched against
/// the path relative to the search root; otherwise against the file name, the
/// way `rg -g` behaves.
fn glob_matches(glob: &str, file_name: &str, rel_path: &str) -> bool {
    match glob_to_regex(glob) {
        Some(re) => {
            if glob.contains('/') {
                re.is_match(rel_path)
            } else {
                re.is_match(file_name)
            }
        }
        None => false,
    }
}

/// Collect every regular file under `root`, pruning noisy directories and
/// dot-entries (rg's default: hidden files are not searched).
fn collect_files(root: &Path, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        out.push(root.to_path_buf());
        return;
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let path = entry.path();
            if file_type.is_dir() {
                if PRUNED_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() {
                out.push(path);
            }
        }
    }
}

/// Path string as the tools report it: the full path the caller asked about.
fn display_path(root: &Path, path: &Path) -> String {
    if let (Ok(r), Ok(p)) = (root.canonicalize(), path.canonicalize()) {
        if let Ok(rel) = p.strip_prefix(&r) {
            return root.join(rel).to_string_lossy().to_string();
        }
    }
    path.to_string_lossy().to_string()
}

/// File-name search. Returns the matching paths in discovery order, or
/// `modified` order when asked (newest first).
pub fn search_file_names(
    root: &Path,
    pattern: &str,
    file_glob: Option<&str>,
    order_modified: bool,
) -> Result<Vec<String>, String> {
    if !root.exists() {
        return Err(format!("path does not exist: {}", root.display()));
    }

    let mut found: Vec<PathBuf> = Vec::new();
    collect_files(root, &mut found);

    let name_filter = (!pattern.is_empty() && pattern != "*").then_some(pattern);

    let mut results: Vec<(PathBuf, String)> = Vec::new();
    for path in found {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let rel = display_path(root, &path);

        if let Some(p) = name_filter {
            if !glob_matches(p, &file_name, &rel) {
                continue;
            }
        }
        if let Some(fg) = file_glob {
            if !glob_matches(fg, &file_name, &rel) {
                continue;
            }
        }
        results.push((path, rel));
    }

    if order_modified {
        results.sort_by(|a, b| {
            let ma = fs::metadata(&a.0).and_then(|m| m.modified()).ok();
            let mb = fs::metadata(&b.0).and_then(|m| m.modified()).ok();
            mb.cmp(&ma)
        });
    }

    Ok(results.into_iter().map(|(_, rel)| rel).collect())
}

/// Content search across every text file under `root`. Returns matches in
/// walk order with 1-based line numbers; non-UTF-8 files are skipped exactly
/// as rg skips binaries.
pub fn search_contents(
    root: &Path,
    pattern: &str,
    file_glob: Option<&str>,
) -> Result<Vec<NativeMatch>, String> {
    if !root.exists() {
        return Err(format!("path does not exist: {}", root.display()));
    }

    let re = Regex::new(pattern).map_err(|e| format!("invalid pattern: {}", e))?;

    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(root, &mut files);

    let mut matches: Vec<NativeMatch> = Vec::new();
    for path in files {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let rel = display_path(root, &path);

        if let Some(fg) = file_glob {
            if !glob_matches(fg, &file_name, &rel) {
                continue;
            }
        }

        let content = match fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        for (idx, line) in content.lines().enumerate() {
            if re.is_match(line) {
                matches.push(NativeMatch {
                    path: rel.clone(),
                    line: idx + 1,
                    content: line.chars().take(500).collect(),
                });
            }
        }
    }

    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hermes_native_search_{}", tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn glob_wildcards_behave_like_rg() {
        assert!(glob_matches("*.py", "main.py", "main.py"));
        assert!(!glob_matches("*.py", "main.rs", "main.rs"));
        assert!(glob_matches("file_?.txt", "file_a.txt", "file_a.txt"));
        assert!(glob_matches("src/*.rs", "lib.rs", "src/lib.rs"));
        assert!(glob_matches(
            "*config*",
            "app_config.yaml",
            "app_config.yaml"
        ));
    }

    #[test]
    fn file_name_search_filters_and_orders() {
        let dir = temp_dir("names");
        fs::write(dir.join("alpha.txt"), "x").unwrap();
        fs::write(dir.join("beta.txt"), "x").unwrap();
        fs::write(dir.join("gamma.md"), "x").unwrap();

        let all = search_file_names(&dir, "*", None, false).unwrap();
        assert_eq!(all.len(), 3);

        let txt = search_file_names(&dir, "*.txt", None, false).unwrap();
        assert_eq!(txt.len(), 2);
        assert!(txt.iter().all(|p| p.ends_with(".txt")));

        let sorted = search_file_names(&dir, "*", None, true).unwrap();
        assert_eq!(sorted.len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_search_finds_lines_and_counts_them() {
        let dir = temp_dir("content");
        fs::write(dir.join("a.txt"), "target_token_123 here\nother line\n").unwrap();
        fs::write(dir.join("b.txt"), "nothing\ntarget_token_123 again\n").unwrap();

        let matches = search_contents(&dir, "target_token_123", None).unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line, 1);
        assert_eq!(matches[1].line, 2);

        let scoped = search_contents(&dir, "target_token_123", Some("*.md")).unwrap();
        assert!(scoped.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_search_rejects_a_bad_pattern() {
        let dir = temp_dir("bad");
        assert!(search_contents(&dir, "([", None).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_path_is_reported_not_silently_empty() {
        let missing = std::env::temp_dir().join("hermes_native_search_definitely_missing");
        let _ = fs::remove_dir_all(&missing);
        assert!(search_file_names(&missing, "*", None, false).is_err());
        assert!(search_contents(&missing, "x", None).is_err());
    }
}
