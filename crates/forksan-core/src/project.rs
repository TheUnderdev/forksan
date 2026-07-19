//! Project root resolution: the boundary reports are shared within.
//!
//! The project root of a working directory is the nearest ancestor holding a
//! `.forksan/` directory (that's where fork identity lives), falling back to
//! the nearest ancestor holding `.git`, falling back to the directory
//! itself. Canonicalized so paths like `/tmp` vs `/private/tmp` agree.

use std::path::{Path, PathBuf};

/// Resolve the project root for `cwd`.
pub fn project_root(cwd: &Path) -> PathBuf {
    let start = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut cur = Some(start.as_path());
    while let Some(d) = cur {
        if d.join(".forksan").is_dir() {
            return d.to_path_buf();
        }
        cur = d.parent();
    }
    let mut cur = Some(start.as_path());
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return d.to_path_buf();
        }
        cur = d.parent();
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn forksan_wins_over_git_and_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let repo = base.join("repo");
        let sub = repo.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();

        // .git fallback.
        assert_eq!(project_root(&sub), repo);

        // .forksan closer than .git wins.
        fs::create_dir_all(repo.join("a/.forksan")).unwrap();
        assert_eq!(project_root(&sub), repo.join("a"));

        // Nothing at all: the cwd itself.
        let bare = base.join("bare");
        fs::create_dir_all(&bare).unwrap();
        assert_eq!(project_root(&bare), bare);
    }
}
