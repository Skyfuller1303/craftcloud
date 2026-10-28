//! Checks that relative links in Markdown files resolve to existing files or
//! directories. Pure std; external links (`http:`, `https:`, `mailto:`) and
//! pure anchors are skipped, `#anchor` / `?query` suffixes are stripped.
//! Anchors themselves are not validated.

use std::fs;
use std::path::{Path, PathBuf};

const SKIPPED_DIRS: [&str; 4] = ["target", ".git", "node_modules", "dist"];

/// Checks every Markdown file below `root`. Returns the number of resolved
/// relative links, or a list of problems.
pub fn check_repository(root: &Path) -> Result<usize, Vec<String>> {
    let mut files = Vec::new();
    let mut problems = Vec::new();
    collect_markdown(root, &mut files, &mut problems);
    files.sort();

    let mut checked = 0;
    for file in &files {
        let text = match fs::read_to_string(file) {
            Ok(text) => text,
            Err(err) => {
                problems.push(format!("{}: cannot read: {err}", display(file, root)));
                continue;
            }
        };
        let base = file.parent().unwrap_or(root);
        for (line_no, target) in extract_links(&text) {
            let Some(path_part) = relative_path_part(&target) else {
                continue;
            };
            let resolved = match path_part.strip_prefix('/') {
                Some(rooted) => root.join(rooted),
                None => base.join(path_part),
            };
            if resolved.exists() {
                checked += 1;
            } else {
                problems.push(format!(
                    "{}:{line_no}: broken link `{target}`",
                    display(file, root)
                ));
            }
        }
    }

    if problems.is_empty() {
        Ok(checked)
    } else {
        Err(problems)
    }
}

fn display(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn collect_markdown(dir: &Path, files: &mut Vec<PathBuf>, problems: &mut Vec<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            problems.push(format!("{}: cannot list directory: {err}", dir.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                problems.push(format!("{}: cannot read entry: {err}", dir.display()));
                continue;
            }
        };
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let skip = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| SKIPPED_DIRS.contains(&n));
            if !skip {
                collect_markdown(&path, files, problems);
            }
        } else if file_type.is_file() && path.extension().is_some_and(|e| e == "md") {
            files.push(path);
        }
    }
}

/// Returns the path portion of a link target if it is a relative file link.
fn relative_path_part(target: &str) -> Option<&str> {
    let lower = target.to_ascii_lowercase();
    if lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("mailto:") {
        return None;
    }
    let end = target.find(['#', '?']).unwrap_or(target.len());
    let path = target.get(..end)?;
    if path.is_empty() { None } else { Some(path) }
}

/// Extracts `(line, target)` for inline links `[text](target)`, ignoring
/// fenced code blocks and inline code spans.
fn extract_links(text: &str) -> Vec<(usize, String)> {
    let mut links = Vec::new();
    let mut in_fence = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let visible = strip_inline_code(line);
        let mut rest = visible.as_str();
        while let Some((_, after)) = rest.split_once("](") {
            let Some((inside, remainder)) = after.split_once(')') else {
                break;
            };
            if let Some(target) = link_target(inside) {
                links.push((index + 1, target));
            }
            rest = remainder;
        }
    }
    links
}

fn link_target(inside: &str) -> Option<String> {
    let inside = inside.trim();
    let target = if let Some(stripped) = inside.strip_prefix('<') {
        stripped.split_once('>').map_or(stripped, |(t, _)| t)
    } else {
        inside.split_whitespace().next().unwrap_or("")
    };
    if target.is_empty() {
        None
    } else {
        Some(target.to_owned())
    }
}

fn strip_inline_code(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_code = false;
    for c in line.chars() {
        if c == '`' {
            in_code = !in_code;
        } else if !in_code {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_inline_links_and_skips_code() {
        let text =
            "see [a](docs/a.md) and `[b](nope.md)`\n```\n[c](nope2.md)\n```\n[d](<x y.md> \"t\")";
        let links = extract_links(text);
        assert_eq!(
            links,
            vec![(1, "docs/a.md".to_owned()), (5, "x y.md".to_owned())]
        );
    }

    #[test]
    fn classifies_targets() {
        assert_eq!(relative_path_part("https://x.y"), None);
        assert_eq!(relative_path_part("HTTP://x.y"), None);
        assert_eq!(relative_path_part("mailto:a@b"), None);
        assert_eq!(relative_path_part("#anchor"), None);
        assert_eq!(relative_path_part("a.md#top"), Some("a.md"));
        assert_eq!(relative_path_part("a.md?x=1"), Some("a.md"));
    }

    #[test]
    fn detects_broken_and_valid_links() {
        let dir = std::env::temp_dir().join(format!("cc-docs-check-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.md"), "[ok](sub/b.md#x) [bad](missing.md)").unwrap();
        fs::write(
            dir.join("sub").join("b.md"),
            "[up](../a.md) [web](https://e.x)",
        )
        .unwrap();

        let result = check_repository(&dir);
        let problems = result.unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("missing.md"));

        fs::write(dir.join("a.md"), "[ok](sub/b.md#x)").unwrap();
        assert_eq!(check_repository(&dir), Ok(2));
        let _ = fs::remove_dir_all(&dir);
    }
}
