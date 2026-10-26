//! One thinking ladder for thick and thin (SPEC §19.5).
//!
//! Tables in this file are the source of truth until H10 folds them into §19.5.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::models::ModelEntry;
use crate::thin::error::ThinError;
use crate::types::BackendId;

/// Shared effort ladder. Thick omitted field → [`Effort::Medium`]. Thin `None` = Absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Off,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Same suffix list as Cursor `wire_model` (SPEC §19.5).
pub const CURSOR_EFFORT_SUFFIXES: &[&str] = &[
    "-low", "-medium", "-high", "-xhigh", "-max", "-minimal", "-none",
];

const AGY_CLI_SUFFIXES: &[&str] = &["-low", "-medium", "-high"];

/// Adapter action from [`ThinkingProfile::thick_apply`] / [`ThinkingProfile::thin_apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorThinking {
    Omit,
    Disabled,
    Flag {
        name: &'static str,
        value: &'static str,
    },
    Env {
        name: &'static str,
        value: &'static str,
    },
    Budget {
        level: Effort,
    },
    ConfigLevel {
        level: &'static str,
    },
    BudgetZero,
    WireParam {
        effort: &'static str,
    },
    Ignore,
}

#[derive(Debug, Clone, Copy)]
pub struct ThinkingProfile {
    backend: BackendId,
}

pub fn profile(backend: BackendId) -> ThinkingProfile {
    ThinkingProfile { backend }
}

/// `none`|`off` → Off; unknown → 400. Missing → `None` (Absent). Pi maps write `"off"`, never `"none"`.
pub fn normalise_effort(effort: Option<&str>) -> Result<Option<Effort>, ThinError> {
    let Some(raw) = effort else {
        return Ok(None);
    };
    let e = raw.trim().to_ascii_lowercase();
    match e.as_str() {
        "none" | "off" => Ok(Some(Effort::Off)),
        "minimal" => Ok(Some(Effort::Minimal)),
        "low" => Ok(Some(Effort::Low)),
        "medium" => Ok(Some(Effort::Medium)),
        "high" => Ok(Some(Effort::High)),
        "xhigh" => Ok(Some(Effort::XHigh)),
        "max" => Ok(Some(Effort::Max)),
        _ => Err(ThinError::invalid_request(
            format!("reasoning_effort {e:?} is not one of off|minimal|low|medium|high|xhigh|max"),
            Some("reasoning_effort"),
        )),
    }
}

/// Yaml `thinking` is a human hint. Pi UI comes from this, never `ModelEntry.thinking`.
pub fn reasoning_ui(entry: &ModelEntry) -> bool {
    match entry.backend {
        BackendId::ClaudeCode | BackendId::Grok | BackendId::Agy | BackendId::Codex => true,
        BackendId::Cursor => cursor_effort_suffix(entry.wire_model()).is_some(),
        BackendId::Pi => entry.thinking,
    }
}

/// Effort suffix after stripping a trailing `-fast`, same order as Cursor `wire_model`.
pub fn cursor_effort_suffix(id: &str) -> Option<&str> {
    let mut name = id;
    if let Some(stem) = name.strip_suffix("-fast") {
        name = stem;
    }
    for suffix in CURSOR_EFFORT_SUFFIXES {
        if name.strip_suffix(suffix).is_some() {
            return Some(&suffix[1..]);
        }
    }
    None
}

/// Codex models that take `reasoning.effort: none` (measured 2026-10-24: `gpt-6-luna` yes;
/// `gpt-6.1-sol` and `gpt-6-astra` answer `400 Unsupported value: 'none'`). No Codex model takes
/// `minimal`, and `ultra` (listed by `/codex/models`) is `400` on the wire for all three.
pub fn codex_accepts_none(wire_model: &str) -> bool {
    wire_model.ends_with("-luna")
}

/// Whether an agy row can turn thinking off. Claude behind Antigravity thinks whatever the
/// `thinkingConfig` (`thinkingBudget: 0`, `includeThoughts: false` and `thinkingLevel: minimal`
/// all still return thought parts; `none` is `400`; measured 2026-10-25 on
/// `claude-opus-5-5-low|medium`), so its off/minimal rungs are hidden and its lowest level is
/// `low`. Gemini rows honour `thinkingBudget: 0`.
pub fn agy_can_disable_thinking(wire_model: &str) -> bool {
    !wire_model.starts_with("claude-")
}

/// Agy `--model` is `cli_model` after stripping a leftover `-low|-medium|-high`.
pub fn strip_agy_effort_suffix(cli_model: &str) -> &str {
    for suffix in AGY_CLI_SUFFIXES {
        if let Some(stem) = cli_model.strip_suffix(suffix) {
            return stem;
        }
    }
    cli_model
}

pub fn apply_thick(
    args: &mut Vec<String>,
    env: &mut HashMap<String, String>,
    actions: &[VendorThinking],
) {
    for action in actions {
        match action {
            VendorThinking::Flag { name, value } => {
                args.push((*name).to_string());
                args.push((*value).to_string());
            }
            VendorThinking::Env { name, value } => {
                env.insert((*name).to_string(), (*value).to_string());
            }
            VendorThinking::Omit
            | VendorThinking::Disabled
            | VendorThinking::Budget { .. }
            | VendorThinking::ConfigLevel { .. }
            | VendorThinking::BudgetZero
            | VendorThinking::WireParam { .. }
            | VendorThinking::Ignore => {}
        }
    }
}

impl VendorThinking {
    pub fn config_level(actions: &[Self]) -> Option<&'static str> {
        actions.iter().find_map(|a| match a {
            Self::ConfigLevel { level } => Some(*level),
            _ => None,
        })
    }

    pub fn wire_param(actions: &[Self]) -> Option<&'static str> {
        actions.iter().find_map(|a| match a {
            Self::WireParam { effort } => Some(*effort),
            _ => None,
        })
    }
}

impl ThinkingProfile {
    pub fn advertise(&self, entry: &ModelEntry) -> Option<HashMap<String, Option<String>>> {
        if !reasoning_ui(entry) {
            return None;
        }
        let mut map = HashMap::new();
        match self.backend {
            BackendId::ClaudeCode | BackendId::Pi => {
                insert(&mut map, "off", Some("off"));
                insert(&mut map, "minimal", Some("minimal"));
                insert(&mut map, "low", Some("low"));
                insert(&mut map, "medium", Some("medium"));
                insert(&mut map, "high", Some("high"));
                insert(&mut map, "xhigh", Some("xhigh"));
                insert(&mut map, "max", Some("max"));
            }
            BackendId::Grok => {
                insert(&mut map, "off", Some("off"));
                insert(&mut map, "minimal", Some("minimal"));
                insert(&mut map, "low", Some("low"));
                insert(&mut map, "medium", Some("medium"));
                insert(&mut map, "high", Some("high"));
                insert(&mut map, "xhigh", Some("xhigh"));
                insert(&mut map, "max", None);
            }
            BackendId::Agy => {
                let off = agy_can_disable_thinking(entry.wire_model());
                insert(&mut map, "off", off.then_some("off"));
                insert(&mut map, "minimal", off.then_some("minimal"));
                insert(&mut map, "low", Some("low"));
                insert(&mut map, "medium", Some("medium"));
                insert(&mut map, "high", Some("high"));
                insert(&mut map, "xhigh", Some("high"));
                insert(&mut map, "max", Some("high"));
            }
            BackendId::Codex => {
                let off = codex_accepts_none(entry.wire_model()).then_some("off");
                insert(&mut map, "off", off);
                insert(&mut map, "minimal", None);
                insert(&mut map, "low", Some("low"));
                insert(&mut map, "medium", Some("medium"));
                insert(&mut map, "high", Some("high"));
                insert(&mut map, "xhigh", Some("xhigh"));
                insert(&mut map, "max", Some("max"));
            }
            BackendId::Cursor => {
                insert(&mut map, "off", None);
                insert(&mut map, "minimal", None);
                insert(&mut map, "low", Some("low"));
                insert(&mut map, "medium", Some("medium"));
                insert(&mut map, "high", Some("high"));
                insert(&mut map, "xhigh", Some("xhigh"));
                insert(&mut map, "max", None);
            }
        }
        Some(map)
    }

    /// Thick omitted field is already [`Effort::Medium`] on `TurnSpec`.
    pub fn thick_apply(&self, effort: Effort) -> Vec<VendorThinking> {
        match self.backend {
            BackendId::ClaudeCode => claude_thick(effort),
            BackendId::Grok => grok_thick(effort),
            BackendId::Agy => agy_thick(effort),
            BackendId::Cursor | BackendId::Codex | BackendId::Pi => vec![VendorThinking::Ignore],
        }
    }

    /// `None` = Absent = vendor default. Never coerced to Medium.
    pub fn thin_apply(&self, effort: Option<Effort>, entry: &ModelEntry) -> Vec<VendorThinking> {
        match self.backend {
            BackendId::ClaudeCode => claude_thin(effort),
            BackendId::Grok => grok_thin(effort),
            BackendId::Agy => agy_thin(effort),
            BackendId::Cursor => cursor_thin(effort, entry),
            BackendId::Codex => codex_thin(effort, entry),
            BackendId::Pi => vec![VendorThinking::Ignore],
        }
    }
}

fn insert(map: &mut HashMap<String, Option<String>>, key: &str, value: Option<&str>) {
    map.insert(key.to_string(), value.map(str::to_string));
}

fn claude_thick(effort: Effort) -> Vec<VendorThinking> {
    match effort {
        Effort::Off | Effort::Minimal => vec![
            VendorThinking::Omit,
            VendorThinking::Env {
                name: "MAX_THINKING_TOKENS",
                value: "0",
            },
        ],
        Effort::Low => vec![VendorThinking::Flag {
            name: "--effort",
            value: "low",
        }],
        Effort::Medium => vec![VendorThinking::Flag {
            name: "--effort",
            value: "medium",
        }],
        Effort::High | Effort::XHigh | Effort::Max => vec![VendorThinking::Flag {
            name: "--effort",
            value: "high",
        }],
    }
}

fn grok_thick(effort: Effort) -> Vec<VendorThinking> {
    let value = match effort {
        Effort::Off | Effort::Minimal => "minimal",
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh | Effort::Max => "xhigh",
    };
    vec![VendorThinking::Flag {
        name: "--reasoning-effort",
        value,
    }]
}

fn agy_thick(effort: Effort) -> Vec<VendorThinking> {
    match effort {
        Effort::Off | Effort::Minimal => vec![VendorThinking::Omit],
        Effort::Low => vec![VendorThinking::Flag {
            name: "--effort",
            value: "low",
        }],
        Effort::Medium => vec![VendorThinking::Flag {
            name: "--effort",
            value: "medium",
        }],
        Effort::High | Effort::XHigh | Effort::Max => vec![VendorThinking::Flag {
            name: "--effort",
            value: "high",
        }],
    }
}

fn claude_thin(effort: Option<Effort>) -> Vec<VendorThinking> {
    match effort {
        None => vec![VendorThinking::Omit],
        Some(Effort::Off) | Some(Effort::Minimal) => vec![VendorThinking::Disabled],
        Some(level) => vec![VendorThinking::Budget { level }],
    }
}

fn grok_thin(effort: Option<Effort>) -> Vec<VendorThinking> {
    match effort {
        None => vec![VendorThinking::Omit],
        Some(Effort::Off) | Some(Effort::Minimal) => {
            vec![VendorThinking::ConfigLevel { level: "minimal" }]
        }
        Some(Effort::Low) => vec![VendorThinking::ConfigLevel { level: "low" }],
        Some(Effort::Medium) => vec![VendorThinking::ConfigLevel { level: "medium" }],
        Some(Effort::High) => vec![VendorThinking::ConfigLevel { level: "high" }],
        Some(Effort::XHigh) | Some(Effort::Max) => {
            vec![VendorThinking::ConfigLevel { level: "xhigh" }]
        }
    }
}

fn agy_thin(effort: Option<Effort>) -> Vec<VendorThinking> {
    match effort {
        None => vec![VendorThinking::Omit],
        Some(Effort::Off) | Some(Effort::Minimal) => vec![VendorThinking::BudgetZero],
        Some(Effort::Low) => vec![VendorThinking::ConfigLevel { level: "low" }],
        Some(Effort::Medium) => vec![VendorThinking::ConfigLevel { level: "medium" }],
        Some(Effort::High) | Some(Effort::XHigh) | Some(Effort::Max) => {
            vec![VendorThinking::ConfigLevel { level: "high" }]
        }
    }
}

/// Absent → omit (`medium`, the vendor default). Off → `none` where the model takes it, else
/// `low`, the lowest it does; minimal → `low`. A client following the advertised map never
/// sends either where they are hidden.
fn codex_thin(effort: Option<Effort>, entry: &ModelEntry) -> Vec<VendorThinking> {
    let effort = match effort {
        None => return vec![VendorThinking::Omit],
        Some(Effort::Off) if codex_accepts_none(entry.wire_model()) => "none",
        Some(Effort::Off) | Some(Effort::Minimal) | Some(Effort::Low) => "low",
        Some(Effort::Medium) => "medium",
        Some(Effort::High) => "high",
        Some(Effort::XHigh) => "xhigh",
        Some(Effort::Max) => "max",
    };
    vec![VendorThinking::WireParam { effort }]
}

fn cursor_thin(effort: Option<Effort>, entry: &ModelEntry) -> Vec<VendorThinking> {
    if cursor_effort_suffix(entry.wire_model()).is_none() {
        return vec![VendorThinking::Ignore];
    }
    match effort {
        None | Some(Effort::Off) | Some(Effort::Minimal) => vec![VendorThinking::Ignore],
        Some(Effort::Low) => vec![VendorThinking::WireParam { effort: "low" }],
        Some(Effort::Medium) => vec![VendorThinking::WireParam { effort: "medium" }],
        Some(Effort::High) => vec![VendorThinking::WireParam { effort: "high" }],
        Some(Effort::XHigh) => vec![VendorThinking::WireParam { effort: "xhigh" }],
        Some(Effort::Max) => vec![VendorThinking::WireParam { effort: "max" }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::models::{default_input, Transport};

    fn entry(
        backend: BackendId,
        cli: &str,
        vendor: Option<&str>,
        yaml_thinking: bool,
    ) -> ModelEntry {
        ModelEntry {
            id: format!("subs/{cli}"),
            backend,
            cli_model: cli.into(),
            context_window: 0,
            max_tokens: 0,
            thinking: yaml_thinking,
            cost: 0.0,
            name: String::new(),
            provider: None,
            transport: Transport::OauthDirect,
            vendor_model: vendor.map(str::to_string),
            enabled: true,
            input: default_input(),
        }
    }

    fn map_get<'a>(map: &'a HashMap<String, Option<String>>, key: &str) -> Option<&'a str> {
        map.get(key).and_then(|v| v.as_deref())
    }

    #[test]
    fn normalise_effort_aliases_and_rejects() {
        assert_eq!(normalise_effort(None).unwrap(), None);
        assert_eq!(normalise_effort(Some("none")).unwrap(), Some(Effort::Off));
        assert_eq!(normalise_effort(Some("off")).unwrap(), Some(Effort::Off));
        assert_eq!(normalise_effort(Some("High")).unwrap(), Some(Effort::High));
        assert!(normalise_effort(Some("ultra")).is_err());
        assert_eq!(serde_json::to_string(&Effort::Off).unwrap(), "\"off\"");
    }

    #[test]
    fn advertise_claude_off_is_off_not_null_or_none() {
        let e = entry(BackendId::ClaudeCode, "claude-fable-5-1", None, false);
        let map = profile(BackendId::ClaudeCode).advertise(&e).expect("map");
        assert_eq!(map_get(&map, "off"), Some("off"));
        assert_ne!(map_get(&map, "off"), Some("none"));
        assert_eq!(map_get(&map, "minimal"), Some("minimal"));
        assert_eq!(map_get(&map, "max"), Some("max"));
    }

    #[test]
    fn advertise_grok_named_off_max_null() {
        let e = entry(BackendId::Grok, "grok-4.7", None, true);
        let map = profile(BackendId::Grok).advertise(&e).expect("map");
        assert_eq!(map_get(&map, "off"), Some("off"));
        assert_eq!(map_get(&map, "minimal"), Some("minimal"));
        assert!(map.get("max").expect("key").is_none());
    }

    #[test]
    fn advertise_agy_xhigh_max_are_high() {
        let e = entry(BackendId::Agy, "gemini-3.8-flash", None, true);
        let map = profile(BackendId::Agy).advertise(&e).expect("map");
        assert_eq!(map_get(&map, "off"), Some("off"));
        assert_eq!(map_get(&map, "xhigh"), Some("high"));
        assert_eq!(map_get(&map, "max"), Some("high"));
    }

    #[test]
    fn advertise_cursor_suffixed_hides_off_minimal_max() {
        let e = entry(
            BackendId::Cursor,
            "cursor-grok-4.7-high",
            Some("cursor-grok-4.7-high"),
            true,
        );
        let map = profile(BackendId::Cursor).advertise(&e).expect("map");
        assert!(map.get("off").expect("key").is_none());
        assert!(map.get("minimal").expect("key").is_none());
        assert_eq!(map_get(&map, "low"), Some("low"));
        assert_eq!(map_get(&map, "xhigh"), Some("xhigh"));
        assert!(map.get("max").expect("key").is_none());
    }

    #[test]
    fn auto_has_no_reasoning_ui() {
        let auto = entry(BackendId::Cursor, "auto", Some("auto"), true);
        assert!(!reasoning_ui(&auto));
        assert!(profile(BackendId::Cursor).advertise(&auto).is_none());
    }

    #[test]
    fn pi_child_advertise_follows_yaml_thinking() {
        let off = entry(BackendId::Pi, "anthropic/claude-opus-5", None, false);
        let on = entry(BackendId::Pi, "anthropic/claude-opus-5", None, true);
        assert!(!reasoning_ui(&off));
        assert!(reasoning_ui(&on));
        let map = profile(BackendId::Pi).advertise(&on).expect("map");
        assert_eq!(map_get(&map, "off"), Some("off"));
    }

    #[test]
    fn grok_reasoning_ui_true_even_when_yaml_false() {
        let e = entry(BackendId::Grok, "grok-4.7", None, false);
        assert!(reasoning_ui(&e));
    }

    #[test]
    fn thick_claude_off_env_no_effort_flag() {
        let mut args = Vec::new();
        let mut env = HashMap::new();
        apply_thick(
            &mut args,
            &mut env,
            &profile(BackendId::ClaudeCode).thick_apply(Effort::Off),
        );
        assert!(!args.iter().any(|a| a == "--effort"));
        assert_eq!(
            env.get("MAX_THINKING_TOKENS").map(String::as_str),
            Some("0")
        );
        let mut args = Vec::new();
        let mut env = HashMap::new();
        apply_thick(
            &mut args,
            &mut env,
            &profile(BackendId::ClaudeCode).thick_apply(Effort::Minimal),
        );
        assert!(!args.iter().any(|a| a == "--effort"));
        assert_eq!(
            env.get("MAX_THINKING_TOKENS").map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn thick_claude_medium_effort_no_env() {
        let mut args = Vec::new();
        let mut env = HashMap::new();
        apply_thick(
            &mut args,
            &mut env,
            &profile(BackendId::ClaudeCode).thick_apply(Effort::Medium),
        );
        assert_eq!(args, vec!["--effort", "medium"]);
        assert!(!env.contains_key("MAX_THINKING_TOKENS"));
    }

    #[test]
    fn thick_grok_off_is_minimal_never_none() {
        for level in [Effort::Off, Effort::Minimal] {
            let actions = profile(BackendId::Grok).thick_apply(level);
            apply_thick(&mut Vec::new(), &mut HashMap::new(), &actions);
            assert!(
                actions.iter().any(|a| matches!(
                    a,
                    VendorThinking::Flag {
                        name: "--reasoning-effort",
                        value: "minimal"
                    }
                )),
                "{level:?}"
            );
            assert!(actions
                .iter()
                .all(|a| !matches!(a, VendorThinking::Flag { value: "none", .. })));
        }
    }

    #[test]
    fn thick_agy_off_omits_effort_medium_flags() {
        let off = profile(BackendId::Agy).thick_apply(Effort::Off);
        assert_eq!(off, vec![VendorThinking::Omit]);
        let med = profile(BackendId::Agy).thick_apply(Effort::Medium);
        assert_eq!(
            med,
            vec![VendorThinking::Flag {
                name: "--effort",
                value: "medium"
            }]
        );
    }

    #[test]
    fn agy_stale_high_suffix_strips() {
        assert_eq!(
            strip_agy_effort_suffix("gemini-3.8-flash-high"),
            "gemini-3.8-flash"
        );
        assert_eq!(
            strip_agy_effort_suffix("gemini-3.8-flash"),
            "gemini-3.8-flash"
        );
    }

    #[test]
    fn thick_cursor_and_pi_ignore() {
        assert_eq!(
            profile(BackendId::Cursor).thick_apply(Effort::High),
            vec![VendorThinking::Ignore]
        );
        assert_eq!(
            profile(BackendId::Pi).thick_apply(Effort::Off),
            vec![VendorThinking::Ignore]
        );
    }

    #[test]
    fn thin_claude_absent_vs_off() {
        let e = entry(BackendId::ClaudeCode, "claude-fable-5-1", None, true);
        assert_eq!(
            profile(BackendId::ClaudeCode).thin_apply(None, &e),
            vec![VendorThinking::Omit]
        );
        assert_eq!(
            profile(BackendId::ClaudeCode).thin_apply(Some(Effort::Off), &e),
            vec![VendorThinking::Disabled]
        );
        assert_eq!(
            profile(BackendId::ClaudeCode).thin_apply(Some(Effort::Medium), &e),
            vec![VendorThinking::Budget {
                level: Effort::Medium
            }]
        );
    }

    #[test]
    fn thin_grok_off_minimal_body_not_none() {
        let e = entry(BackendId::Grok, "grok-4.7", None, true);
        for level in [Effort::Off, Effort::Minimal] {
            let actions = profile(BackendId::Grok).thin_apply(Some(level), &e);
            assert_eq!(
                actions,
                vec![VendorThinking::ConfigLevel { level: "minimal" }]
            );
            assert!(actions
                .iter()
                .all(|a| !matches!(a, VendorThinking::ConfigLevel { level: "none" })));
        }
        assert_eq!(
            profile(BackendId::Grok).thin_apply(None, &e),
            vec![VendorThinking::Omit]
        );
    }

    #[test]
    fn thin_agy_off_is_budget_zero() {
        let e = entry(BackendId::Agy, "gemini-3.8-flash", None, true);
        assert_eq!(
            profile(BackendId::Agy).thin_apply(Some(Effort::Off), &e),
            vec![VendorThinking::BudgetZero]
        );
        assert_eq!(
            profile(BackendId::Agy).thin_apply(Some(Effort::XHigh), &e),
            vec![VendorThinking::ConfigLevel { level: "high" }]
        );
    }

    #[test]
    fn thin_cursor_suffixed_vs_not() {
        let suffixed = entry(
            BackendId::Cursor,
            "cursor-grok-4.7-high",
            Some("cursor-grok-4.7-high"),
            true,
        );
        let unsuffixed = entry(BackendId::Cursor, "auto", Some("auto"), false);
        assert_eq!(
            profile(BackendId::Cursor).thin_apply(Some(Effort::Off), &suffixed),
            vec![VendorThinking::Ignore]
        );
        assert_eq!(
            profile(BackendId::Cursor).thin_apply(Some(Effort::High), &suffixed),
            vec![VendorThinking::WireParam { effort: "high" }]
        );
        assert_eq!(
            profile(BackendId::Cursor).thin_apply(Some(Effort::High), &unsuffixed),
            vec![VendorThinking::Ignore]
        );
        assert_eq!(
            profile(BackendId::Cursor).thin_apply(None, &suffixed),
            vec![VendorThinking::Ignore]
        );
    }
}
