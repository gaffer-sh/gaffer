//! Git metadata detection — branch name and commit SHA.

use std::path::Path;
use std::process::Command;

use gaffer_core::upload::CommitMetadata;

/// Detect the current git branch. CI env vars take priority.
pub fn detect_branch() -> Option<String> {
    // GitHub Actions
    if let Ok(ref_name) = std::env::var("GITHUB_HEAD_REF") {
        if !ref_name.is_empty() {
            return Some(ref_name);
        }
    }
    if let Ok(ref_name) = std::env::var("GITHUB_REF_NAME") {
        if !ref_name.is_empty() {
            return Some(ref_name);
        }
    }

    // GitLab CI
    if let Ok(branch) = std::env::var("CI_COMMIT_BRANCH") {
        if !branch.is_empty() {
            return Some(branch);
        }
    }

    // Fallback to local git
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let branch = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !branch.is_empty() && branch != "HEAD" {
                    Some(branch)
                } else {
                    None
                }
            } else {
                None
            }
        })
}

/// Detect the current git commit SHA. CI env vars take priority.
pub fn detect_commit() -> Option<String> {
    if let Ok(sha) = std::env::var("GITHUB_SHA") {
        if !sha.is_empty() {
            return Some(sha);
        }
    }

    if let Ok(sha) = std::env::var("CI_COMMIT_SHA") {
        if !sha.is_empty() {
            return Some(sha);
        }
    }

    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let sha = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !sha.is_empty() {
                    Some(sha)
                } else {
                    None
                }
            } else {
                None
            }
        })
}

/// Read author, committer and message for `sha` from the checkout at `dir`.
/// `None` when there is no git, no checkout, or the commit isn't in it (a
/// shallow clone): the upload then goes ahead without them.
pub fn read_commit_metadata(sha: &str, dir: &Path) -> Option<CommitMetadata> {
    // `sha` comes from a flag and lands in argv, so a value like
    // `--output=x` must not reach git as an option.
    if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let output = Command::new("git")
        .args(["-c", "log.showSignature=false", "log", "-1"])
        .arg(format!("--format={}", CommitMetadata::GIT_LOG_FORMAT))
        .arg(sha)
        .current_dir(dir)
        // Inside a git hook these point at the hook's repo, not at `dir`.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    CommitMetadata::from_git_log(&String::from_utf8_lossy(&output.stdout))
}

/// Detect the CI provider from environment variables.
pub fn detect_ci_provider() -> Option<String> {
    if std::env::var("GITHUB_ACTIONS").is_ok() {
        return Some("github-actions".to_string());
    }
    if std::env::var("GITLAB_CI").is_ok() {
        return Some("gitlab".to_string());
    }
    if std::env::var("CIRCLECI").is_ok() {
        return Some("circleci".to_string());
    }
    if std::env::var("JENKINS_URL").is_ok() {
        return Some("jenkins".to_string());
    }
    if std::env::var("BUILDKITE").is_ok() {
        return Some("buildkite".to_string());
    }
    if std::env::var("TRAVIS").is_ok() {
        return Some("travis".to_string());
    }
    if std::env::var("TF_BUILD").is_ok() {
        return Some("azure-devops".to_string());
    }
    if std::env::var("BITBUCKET_PIPELINE_UUID").is_ok() {
        return Some("bitbucket".to_string());
    }
    if std::env::var("WOODPECKER").is_ok() {
        return Some("woodpecker".to_string());
    }
    None
}

/// Detect the default branch name for the repository.
/// Tries `git symbolic-ref refs/remotes/origin/HEAD`, then falls back to
/// checking if "main" or "master" branches exist locally.
pub fn detect_default_branch() -> Option<String> {
    // Try git symbolic-ref first (requires `git remote set-head origin --auto` to have run)
    if let Some(branch) = Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let refpath = String::from_utf8_lossy(&o.stdout).trim().to_string();
                // Strip "refs/remotes/origin/" prefix
                refpath.strip_prefix("refs/remotes/origin/").map(|s| s.to_string())
            } else {
                None
            }
        })
    {
        return Some(branch);
    }

    // Fallback: check if "main" or "master" exist as local branches
    for candidate in &["main", "master"] {
        let result = Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{}", candidate)])
            .output();
        if result.is_ok_and(|o| o.status.success()) {
            return Some(candidate.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs git in `dir` and only there. A pre-commit hook exports `GIT_DIR` and
    /// friends, and inheriting them once made `git init` here rewrite the real
    /// repo's `core.worktree`.
    fn git(dir: &Path, args: &[&str]) -> String {
        let inherited = std::env::vars().map(|(key, _)| key);
        let mut command = Command::new("git");
        for key in inherited.filter(|key| key.starts_with("GIT_")) {
            command.env_remove(key);
        }
        let out = command
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Ada")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Grace")
            .env("GIT_COMMITTER_EMAIL", "grace@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn reads_the_named_commit_from_a_checkout() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(
            dir.path(),
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "fix: a thing",
                "-m",
                "the body",
            ],
        );
        let sha = git(dir.path(), &["rev-parse", "HEAD"]);

        let commit = read_commit_metadata(&sha, dir.path()).unwrap();

        assert_eq!(commit.author_name, "Ada");
        assert_eq!(commit.committer_name, "Grace");
        assert!(!commit.author_date.is_empty());
        assert!(!commit.committer_date.is_empty());
        assert_eq!(commit.message, "fix: a thing\nthe body");
    }

    #[test]
    fn outside_a_checkout_there_is_nothing_to_read() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_commit_metadata("abc123", dir.path()), None);
    }

    #[test]
    fn a_commit_the_checkout_lacks_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
        assert_eq!(
            read_commit_metadata(&"a".repeat(40), dir.path()),
            None
        );
    }

    #[test]
    fn a_sha_that_is_not_hex_never_reaches_git() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "first"]);
        let target = dir.path().join("written");
        let flag = format!("--output={}", target.display());

        assert_eq!(read_commit_metadata(&flag, dir.path()), None);
        assert_eq!(read_commit_metadata("HEAD", dir.path()), None);
        assert_eq!(read_commit_metadata("", dir.path()), None);
        assert!(!target.exists());
    }
}
