//! Git repo facts, scoped deliberately to the repository level rather than
//! per-file blame.
//!
//! "Last commit that touched this exact path" is an O(commits) walk per
//! file — at a million-file corpus that is the kind of cost the memory and
//! scan-time budgets (`docs/memory-budget.md`) exist to rule out. What's
//! cheap and still useful: which repo a file lives in, and that repo's HEAD
//! (branch, commit, author, time), applied to every file under the root.
//! Per-file history is left for a later milestone if it turns out to matter.
//!
//! Repo lookups walk up from each file to find a `.git` — callers processing
//! many files under one repo will want to cache that walk themselves; this
//! module is a pure, uncached function so it stays simple to reason about
//! and to test.

use std::path::Path;

/// Repository-level facts, applied to every file under the repo root.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct GitFacts {
    pub branch: Option<String>,
    pub head_commit: Option<String>,
    pub head_author: Option<String>,
    pub head_at_unix: Option<i64>,
}

/// Find the git repository containing `path`, if any, and read its HEAD
/// facts. `None` covers every case where there's nothing to report — not in
/// a repo, a corrupt `.git`, a detached HEAD with no resolvable commit —
/// since this is enrichment, never a hard requirement of extraction.
///
/// `gix::discover` requires a directory to start from (it errors on a file
/// path rather than implicitly walking up from one), so a file's parent is
/// what actually gets searched — still correct, since the parent is on the
/// same ancestor chain as the file itself.
pub(crate) fn lookup(path: &Path) -> Option<GitFacts> {
    let start = if path.is_dir() { path } else { path.parent()? };
    let repo = gix::discover(start).ok()?;

    // A detached HEAD has no referent name (`None` here, correctly), while an
    // unborn HEAD (freshly `git init`, no commits yet) does have one — that
    // second case is only distinguished by `head_commit()` failing below, so
    // this alone isn't enough to tell "no branch" apart from "no commits".
    let branch = repo
        .head()
        .ok()
        .and_then(|head| head.referent_name().map(|name| name.shorten().to_string()));

    // Fails on an unborn HEAD (no commits) or a HEAD that doesn't resolve to
    // a commit at all; both cases fall through to `None` for the whole
    // lookup rather than a repo with a branch name but no other facts.
    let commit = repo.head_commit().ok()?;
    let author = commit.author().ok();

    Some(GitFacts {
        branch,
        head_commit: Some(commit.id().to_string()),
        head_author: author.as_ref().map(|a| a.name.to_string()),
        head_at_unix: author.and_then(|a| a.time().ok()).map(|t| t.seconds),
    })
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    /// Builds a real one-commit repo via the system `git` binary — production
    /// code must stay gix-only (no shelling out at runtime), but a test is
    /// allowed to lean on the reference implementation to set up its fixture.
    fn init_repo_with_one_commit(root: &Path) {
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("git binary available");
            assert!(status.success(), "git {args:?} failed");
        };

        git(&["init", "--quiet", "--initial-branch=trunk"]);
        git(&[
            "-c",
            "user.name=Test User",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial commit",
        ]);
    }

    #[test]
    fn finds_facts_from_a_file_nested_several_directories_deep() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_one_commit(dir.path());

        let nested = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).expect("mkdir -p");
        let file = nested.join("leaf.txt");
        std::fs::write(&file, b"hello").expect("write");

        let facts = lookup(&file).expect("facts");
        assert_eq!(facts.branch.as_deref(), Some("trunk"));
        assert_eq!(facts.head_author.as_deref(), Some("Test User"));
        assert!(facts.head_commit.is_some_and(|c| c.len() == 40));
        assert!(facts.head_at_unix.is_some());
    }

    #[test]
    fn a_plain_directory_with_no_repo_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("no_repo_here.txt");
        std::fs::write(&file, b"hello").expect("write");

        assert_eq!(lookup(&file), None);
    }

    #[test]
    fn an_unborn_head_with_no_commits_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .expect("git binary available");
        assert!(status.success());

        let file = dir.path().join("untracked.txt");
        std::fs::write(&file, b"hello").expect("write");

        assert_eq!(lookup(&file), None);
    }
}
