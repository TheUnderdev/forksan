//! Fork discovery: scan `.forksan/forks/` trees for fork definitions.
//!
//! Two layouts are supported inside a forks root, in any mix:
//! - a bare `<name>.md` file with YAML frontmatter
//! - a `<name>/FORK.md` folder
//!
//! Other subfolders are organizational and are descended into (capped
//! nesting). The fork's name is the file stem or the folder name; when two
//! roots define the same name, the first-discovered wins (roots are scanned
//! nearest-project-first, so project forks override user-level ones).

use crate::frontmatter::{parse_fork_file, ForkParse, ParsedFork};
use std::path::{Path, PathBuf};

/// Cap on organizational-subfolder nesting below a forks root.
const MAX_FORK_NESTING_DEPTH: usize = 8;

/// A discovered fork definition.
#[derive(Debug, Clone)]
pub struct ForkEntry {
    pub name: String,
    /// Absolute path of the definition file (`…/<name>.md` or `…/FORK.md`).
    pub path: PathBuf,
    /// The forks root this entry came from (`…/.forksan/forks`).
    pub root: PathBuf,
    pub parsed: ParsedFork,
}

/// The `.forksan/forks` roots relevant to `dir`: each ancestor's (including
/// `dir` itself), nearest first, then `user_forks_root` (the user-level
/// forks directory, e.g. `~/.forksan/forks`) if not already among them.
pub fn fork_roots(dir: &Path, user_forks_root: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let start = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut cur = Some(start.as_path());
    while let Some(d) = cur {
        let candidate = d.join(".forksan").join("forks");
        if candidate.is_dir() {
            roots.push(candidate);
        }
        cur = d.parent();
    }
    if let Some(user) = user_forks_root {
        let user = user.canonicalize().unwrap_or_else(|_| user.to_path_buf());
        if user.is_dir() && !roots.contains(&user) {
            roots.push(user);
        }
    }
    roots
}

/// Discover all forks visible from `dir` (project roots nearest-first, then
/// user-level). Returns entries in discovery order plus collision warnings.
pub fn discover_forks(dir: &Path, user_forks_root: Option<&Path>) -> (Vec<ForkEntry>, Vec<String>) {
    let mut entries: Vec<ForkEntry> = Vec::new();
    let mut warnings = Vec::new();
    for root in fork_roots(dir, user_forks_root) {
        scan_forks_dir(&root, &root, 0, &mut entries, &mut warnings);
    }
    (entries, warnings)
}

fn insert_entry(
    entries: &mut Vec<ForkEntry>,
    warnings: &mut Vec<String>,
    name: String,
    path: PathBuf,
    root: &Path,
) {
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            warnings.push(format!("failed to read {}: {e}", path.display()));
            return;
        }
    };
    let parsed = match parse_fork_file(&name, &content) {
        ForkParse::Fork(p) => p,
        // A companion note that only looks like a fork: warn so a missing
        // `fork: true` marker can't silently disable a real fork.
        ForkParse::NotFork { fork_like: true } => {
            warnings.push(format!(
                "{} has fork-like frontmatter but no `fork: true` — not treated as a fork",
                path.display()
            ));
            return;
        }
        // A plain companion `.md` (or explicit `fork: false`): silently skip.
        ForkParse::NotFork { fork_like: false } => {
            tracing::debug!(path = %path.display(), "not a fork (no `fork: true`), skipping");
            return;
        }
        ForkParse::Invalid => {
            warnings.push(format!(
                "fork '{name}' at {} has invalid frontmatter YAML, skipped",
                path.display()
            ));
            return;
        }
    };
    // Only real forks reserve a name / shadow others.
    if let Some(existing) = entries.iter().find(|e| e.name == name) {
        if existing.path != path {
            warnings.push(format!(
                "fork '{name}' at {} shadowed by {}",
                path.display(),
                existing.path.display()
            ));
        }
        return;
    }
    entries.push(ForkEntry {
        name,
        path,
        root: root.to_path_buf(),
        parsed,
    });
}

fn scan_forks_dir(
    dir: &Path,
    root: &Path,
    depth: usize,
    entries: &mut Vec<ForkEntry>,
    warnings: &mut Vec<String>,
) {
    if depth > MAX_FORK_NESTING_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = read.filter_map(|e| e.ok()).collect();
    items.sort_by_key(|e| e.file_name());
    for item in items {
        let path = item.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            let fork_md = path.join("FORK.md");
            if fork_md.is_file() {
                insert_entry(entries, warnings, file_name.to_string(), fork_md, root);
            } else {
                scan_forks_dir(&path, root, depth + 1, entries, warnings);
            }
        } else if let Some(stem) = file_name.strip_suffix(".md") {
            if !stem.is_empty() {
                insert_entry(entries, warnings, stem.to_string(), path.clone(), root);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn discovers_both_layouts_and_subfolders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        write(
            &root.join(".forksan/forks/journal.md"),
            "---\nfork: true\ndescription: j\n---\nbody",
        );
        write(
            &root.join(".forksan/forks/cleanup/FORK.md"),
            "---\nfork: true\nrun_on: [idle]\n---\nbody",
        );
        // A companion note (no marker, no fork-like keys) is silently ignored.
        write(
            &root.join(".forksan/forks/maint/deep/notes.md"),
            "no frontmatter reference material",
        );
        // Ignored: dotfiles, non-md files, dirs without FORK.md are recursed only.
        write(&root.join(".forksan/forks/.hidden.md"), "x");
        write(&root.join(".forksan/forks/readme.txt"), "x");

        let (entries, warnings) = discover_forks(&root, None);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["cleanup", "journal"]);
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        let journal = entries.iter().find(|e| e.name == "journal").unwrap();
        assert_eq!(journal.parsed.def.description.as_deref(), Some("j"));
        // Roots are canonicalized (macOS /var vs /private/var).
        assert_eq!(
            journal.root,
            root.canonicalize().unwrap().join(".forksan/forks")
        );
    }

    #[test]
    fn companion_note_with_fork_like_keys_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        // A real fork.
        write(
            &root.join(".forksan/forks/real.md"),
            "---\nfork: true\nrun_on: [idle]\n---\nbody",
        );
        // A migration mistake: fork keys but no marker → warned, not a fork.
        write(
            &root.join(".forksan/forks/oops.md"),
            "---\nrun_on: [idle]\nthrottle: 1h\n---\nbody",
        );
        // Explicit opt-out and a plain note → silent.
        write(
            &root.join(".forksan/forks/note.md"),
            "---\nfork: false\nrun_on: [idle]\n---\nb",
        );
        write(&root.join(".forksan/forks/plain.md"), "just notes");

        let (entries, warnings) = discover_forks(&root, None);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no `fork: true`"), "{}", warnings[0]);
    }

    #[test]
    fn upward_traversal_and_project_overrides_user() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        let inner = outer.join("inner");
        let home = tmp.path().join("home");
        write(
            &outer.join(".forksan/forks/shared.md"),
            "---\nfork: true\n---\nouter body",
        );
        write(
            &inner.join(".forksan/forks/shared.md"),
            "---\nfork: true\n---\ninner body",
        );
        write(
            &inner.join(".forksan/forks/local.md"),
            "---\nfork: true\n---\nlocal",
        );
        write(
            &home.join(".forksan/forks/shared.md"),
            "---\nfork: true\n---\nhome body",
        );
        write(
            &home.join(".forksan/forks/user.md"),
            "---\nfork: true\n---\nuser",
        );
        fs::create_dir_all(&inner).unwrap();

        let (entries, warnings) = discover_forks(&inner, Some(&home.join(".forksan/forks")));
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["local", "shared", "user"]);
        let shared = entries.iter().find(|e| e.name == "shared").unwrap();
        assert_eq!(shared.parsed.body, "inner body");
        // Two shadow warnings: outer and home both lost to inner.
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn invalid_yaml_is_skipped_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        write(&root.join(".forksan/forks/bad.md"), "---\n: [oops\n---\nb");
        write(
            &root.join(".forksan/forks/good.md"),
            "---\nfork: true\n---\nfine",
        );
        let (entries, warnings) = discover_forks(&root, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "good");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn no_forksan_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (entries, warnings) = discover_forks(tmp.path(), None);
        assert!(entries.is_empty());
        assert!(warnings.is_empty());
    }
}
