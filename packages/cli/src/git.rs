//! Git metadata detection — branch name and commit SHA.

use std::process::Command;

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
    None
}
