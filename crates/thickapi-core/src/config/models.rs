use serde::{Deserialize, Serialize};

use crate::types::BackendId;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    #[serde(default)]
    pub aliases: std::collections::BTreeMap<String, String>,
    pub models: Vec<ModelEntry>,
}

/// How a thin `/v1/chat/completions` request reaches the vendor (SPEC §19.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Thick turn only; the row is not served on the thin surface.
    #[default]
    CliTurn,
    /// Direct vendor API with the CLI's stored OAuth token.
    OauthDirect,
    /// A real `claude.exe` per request with Pi's tools exposed as a proposal-only MCP catalogue.
    CliBridge,
}

fn default_true() -> bool {
    true
}

pub fn default_input() -> Vec<String> {
    vec!["text".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ModelEntry {
    pub id: String,
    pub backend: BackendId,
    pub cli_model: String,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub max_tokens: u64,
    #[serde(default)]
    pub thinking: bool,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub name: String,
    pub provider: Option<String>,
    /// Thin-surface transport (SPEC §19.2). Default `cli_turn` keeps every thick row unchanged.
    #[serde(default)]
    pub transport: Transport,
    /// Wire model id for `oauth_direct` (falls back to `cli_model`).
    #[serde(default)]
    pub vendor_model: Option<String>,
    /// Per-row kill switch (SPEC §19.6): `false` removes the row from `/v1/models` and `sync-pi`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Accepted content parts: `text`, and `image` only when the adapter implements images.
    #[serde(default = "default_input")]
    pub input: Vec<String>,
}

impl ModelEntry {
    /// A synthetic `cli_bridge` row for `doctor` / `/ready` probes (not from `models.yaml`).
    pub fn bridge_probe(cli_model: &str) -> Self {
        Self {
            id: format!("probe/claude/{cli_model}"),
            backend: BackendId::ClaudeCode,
            cli_model: cli_model.to_string(),
            context_window: 0,
            max_tokens: 0,
            thinking: false,
            cost: 0.0,
            name: String::new(),
            provider: None,
            transport: Transport::CliBridge,
            vendor_model: None,
            enabled: true,
            input: default_input(),
        }
    }

    /// Wire id sent to the vendor on `oauth_direct`, else the CLI model id.
    pub fn wire_model(&self) -> &str {
        self.vendor_model.as_deref().unwrap_or(&self.cli_model)
    }

    pub fn accepts_image(&self) -> bool {
        self.input.iter().any(|i| i == "image")
    }

    /// Whether this row is served on the thin `/v1/chat/completions` surface.
    pub fn thin_capable(&self) -> bool {
        self.enabled && self.transport != Transport::CliTurn
    }
}

impl ModelsConfig {
    /// Resolve an id, alias, or `subs/`-less id (`claude/sonnet`) to a catalogue row (SPEC §19.6).
    pub fn find(&self, model: &str) -> Option<&ModelEntry> {
        let id = self
            .aliases
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_string());
        if let Some(entry) = self.models.iter().find(|m| m.id == id) {
            return Some(entry);
        }
        // Both `subs/claude/sonnet` and `claude/sonnet` resolve, whichever way the row is written.
        if let Some(stripped) = id.strip_prefix("subs/") {
            return self.models.iter().find(|m| m.id == stripped);
        }
        if !id.starts_with("or/") {
            let prefixed = format!("subs/{}", id);
            return self.models.iter().find(|m| m.id == prefixed);
        }
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            backend: BackendId::ClaudeCode,
            cli_model: "claude-sonnet-4-6".into(),
            context_window: 0,
            max_tokens: 0,
            thinking: true,
            cost: 0.0,
            name: String::new(),
            provider: None,
            transport: Transport::default(),
            vendor_model: None,
            enabled: true,
            input: default_input(),
        }
    }

    #[test]
    fn defaults_keep_thick_rows_unchanged() {
        let yaml = "id: subs/claude/sonnet\nbackend: claude_code\ncli_model: claude-sonnet-4-6\n";
        let m: ModelEntry = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(m.transport, Transport::CliTurn);
        assert!(m.enabled);
        assert_eq!(m.input, vec!["text".to_string()]);
        assert!(!m.thin_capable());
        assert_eq!(m.wire_model(), "claude-sonnet-4-6");
    }

    #[test]
    fn find_accepts_prefixed_unprefixed_and_alias() {
        let cfg = ModelsConfig {
            aliases: [("sonnet".to_string(), "subs/claude/sonnet".to_string())].into(),
            models: vec![entry("subs/claude/sonnet")],
        };
        assert!(cfg.find("subs/claude/sonnet").is_some());
        assert!(cfg.find("claude/sonnet").is_some());
        assert!(cfg.find("sonnet").is_some());
        assert!(cfg.find("claude/opus").is_none());
    }
}
