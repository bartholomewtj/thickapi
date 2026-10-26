//! Codex has no thick lane: its rows are thin-only (`transport: oauth_direct`, SPEC §19.8
//! `codex.rs`). The thick lane is frozen (DECISIONS 2026-09-28), so this stub only refuses a
//! `thickapi turn` on a Codex row with a clear error instead of spawning anything.

use std::path::Path;

use anyhow::{bail, Result};

use crate::config::models::ModelEntry;
use crate::types::{BackendId, TurnSpec};

use super::{
    BackendAdapter, LearnedTools, ParsedLine, SessionBinding, SpawnPlan, TurnFailureClass,
};

pub struct CodexThinOnly;

impl BackendAdapter for CodexThinOnly {
    fn id(&self) -> BackendId {
        BackendId::Codex
    }

    fn build_spawn(
        &self,
        _spec: &TurnSpec,
        resolved: &ModelEntry,
        _session: &SessionBinding,
        _data_dir: &Path,
    ) -> Result<SpawnPlan> {
        bail!(
            "{} is thin-only: Codex has no thick lane; use /v1/chat/completions",
            resolved.id
        )
    }

    fn parse_line(&self, _line: &str) -> Vec<ParsedLine> {
        Vec::new()
    }

    fn assistant_text(&self, _event: &serde_json::Value) -> Option<String> {
        None
    }

    fn classify_failure(
        &self,
        _returncode: i32,
        _stderr: &str,
        _last_events: &[serde_json::Value],
    ) -> TurnFailureClass {
        TurnFailureClass::Generic
    }

    fn observe_init(&self, _event: &serde_json::Value) -> Option<LearnedTools> {
        None
    }

    fn session_followed(&self, _event: &serde_json::Value) -> Option<String> {
        None
    }
}
