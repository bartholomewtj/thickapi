mod deny_globs;
mod git;
mod patterns;
mod tree;

use std::path::Path;

use anyhow::Result;

use crate::types::{EnforceMode, EnforcePolicy, PermissionBreach};

pub use deny_globs::{deny_globs, path_deny_rule_export as path_deny_rule};
pub use git::{git_add_commit, git_snapshot, init_test_repo, is_git_repo};
pub use patterns::{
    always_writable, changed_paths, denied_match, matches, permitted, protected_match, HEAD_KEY,
};
pub use tree::{
    tree_enforce, tree_snapshot, TreeSnapshot, TREE_SNAPSHOT_CAP, TREE_WALK_MAX_DEPTH,
    TREE_WALK_MAX_FILES,
};

#[derive(Debug, Clone)]
pub enum Snapshot {
    Git(std::collections::BTreeMap<String, String>),
    Tree(Box<TreeSnapshot>),
}

#[derive(Debug, Clone)]
pub struct EnforceOutcome {
    pub touched_paths: Vec<String>,
    pub breaches: Vec<PermissionBreach>,
    pub rolled_back: Vec<String>,
}

impl EnforceOutcome {
    pub fn has_breach(&self) -> bool {
        !self.breaches.is_empty()
    }
}

pub struct Enforcer;

impl Enforcer {
    pub fn deny_globs(policy: &EnforcePolicy, data_dir: &Path) -> Vec<String> {
        deny_globs::deny_globs(policy, data_dir)
    }

    pub fn path_deny_rule(pattern: &str) -> String {
        deny_globs::path_deny_rule_export(pattern)
    }

    pub fn resolve_mode(cwd: &Path, policy: &EnforcePolicy) -> EnforceMode {
        match policy.mode {
            EnforceMode::GitFingerprint if is_git_repo(cwd) => EnforceMode::GitFingerprint,
            EnforceMode::GitFingerprint => EnforceMode::TreeCopy,
            other => other,
        }
    }

    pub fn snapshot(&self, cwd: &Path, mode: EnforceMode, backup_dir: &Path) -> Result<Snapshot> {
        match mode {
            EnforceMode::GitFingerprint => Ok(Snapshot::Git(git::git_snapshot(cwd)?)),
            EnforceMode::TreeCopy => Ok(Snapshot::Tree(Box::new(tree::tree_snapshot(
                cwd, backup_dir,
            )?))),
        }
    }

    pub fn enforce(
        &self,
        cwd: &Path,
        policy: &EnforcePolicy,
        data_dir: &Path,
        before: &Snapshot,
    ) -> Result<EnforceOutcome> {
        match before {
            Snapshot::Git(map) => git::git_enforce(cwd, policy, data_dir, map),
            Snapshot::Tree(tree) => tree::tree_enforce(cwd, policy, data_dir, tree.as_ref()),
        }
    }
}
