//! `router.yaml` keys from before the `thin.vendors.<id>` group (R22 C5, SPEC §10.1).
//!
//! The structs refuse keys they do not know (audit D8), so a file written for an older build
//! is rewritten here first: every flat `thin.<vendor>_*` key moves to its place under
//! `thin.vendors`, and `measured_versions` (deleted at H9) is dropped. Each key found is
//! reported once, and [`LegacyKey::warn`] logs it; nothing here refuses a file.

use serde_yaml::{Mapping, Value};

/// What happened to a legacy key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyAction {
    /// Its value now lives at the new key.
    Moved,
    /// The new key was set too, and wins: the legacy value is ignored.
    Shadowed,
    /// The key no longer exists anywhere (`measured_versions`).
    Removed,
}

/// One legacy key found in `router.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyKey {
    /// Where it was found, dotted from the file root (`thin.cursor_cwd`).
    pub key: String,
    /// Where it belongs now (`thin.vendors.cursor.cwd`); empty for a removed key.
    pub new_key: String,
    pub action: LegacyAction,
}

impl LegacyKey {
    /// One `warn` line for this key.
    pub fn warn(&self) {
        let (key, new_key) = (self.key.as_str(), self.new_key.as_str());
        match self.action {
            LegacyAction::Moved => tracing::warn!(
                key,
                new_key,
                "router.yaml: legacy key; move it to {new_key}"
            ),
            LegacyAction::Shadowed => tracing::warn!(
                key,
                new_key,
                "router.yaml: legacy key ignored because {new_key} is also set; delete it"
            ),
            LegacyAction::Removed => {
                tracing::warn!(
                    key,
                    "router.yaml: legacy key ignored (removed at H9); delete it"
                )
            }
        }
    }
}

/// Top-level keys that no longer mean anything.
const REMOVED: &[&str] = &["measured_versions"];

/// The OAuth-direct vendors the flat `oauth_*` budgets applied to.
const OAUTH_FIRST_EVENT: &[&str] = &[
    "vendors.claude.budgets.first_event_seconds",
    "vendors.grok.budgets.first_event_seconds",
    "vendors.agy.budgets.first_event_seconds",
];
const OAUTH_IDLE: &[&str] = &[
    "vendors.claude.budgets.idle_seconds",
    "vendors.grok.budgets.idle_seconds",
    "vendors.agy.budgets.idle_seconds",
];

/// Legacy `thin.<key>` (dotted for a nested one) → where its value goes, under `thin.`. A key
/// with several targets (the shared `oauth_*` budgets) fills each vendor that does not set its
/// own.
pub const THIN_ALIASES: &[(&str, &[&str])] = &[
    ("oauth_first_event_seconds", OAUTH_FIRST_EVENT),
    ("oauth_idle_seconds", OAUTH_IDLE),
    ("claude_path", &["vendors.claude.path"]),
    (
        "bridge_max_concurrent",
        &["vendors.claude.bridge.max_concurrent"],
    ),
    (
        "bridge_timeout_seconds",
        &["vendors.claude.bridge.timeout_seconds"],
    ),
    (
        "bridge_idle_seconds",
        &["vendors.claude.bridge.idle_seconds"],
    ),
    (
        "mcp_ready_timeout_ms",
        &["vendors.claude.bridge.mcp_ready_timeout_ms"],
    ),
    ("bridge.tolerate", &["vendors.claude.bridge.tolerate"]),
    ("grok_base_url", &["vendors.grok.base_url"]),
    ("grok_credentials_path", &["vendors.grok.credentials_path"]),
    ("grok_client_version", &["vendors.grok.client_version"]),
    (
        "grok_tool_call_seconds",
        &["vendors.grok.budgets.tool_call_floor_seconds"],
    ),
    ("agy_base_url", &["vendors.agy.base_url"]),
    ("agy_credentials_path", &["vendors.agy.credentials_path"]),
    ("agy_client_version", &["vendors.agy.client_version"]),
    ("agy_project", &["vendors.agy.project"]),
    ("cursor_base_url", &["vendors.cursor.base_url"]),
    (
        "cursor_credentials_path",
        &["vendors.cursor.credentials_path"],
    ),
    ("cursor_client_version", &["vendors.cursor.client_version"]),
    ("cursor_cwd", &["vendors.cursor.cwd"]),
    (
        "cursor_first_output_seconds",
        &["vendors.cursor.budgets.first_output_seconds"],
    ),
    (
        "cursor_idle_seconds",
        &["vendors.cursor.budgets.idle_seconds"],
    ),
    (
        "cursor_idle_after_text_with_tools_seconds",
        &["vendors.cursor.budgets.idle_after_text_with_tools_seconds"],
    ),
];

/// The new key a legacy one names in its warning.
fn new_key_label(targets: &[&str]) -> String {
    match targets {
        [one] => format!("thin.{one}"),
        [first, ..] => {
            // `vendors.<id>.<rest>` for every target, differing only in `<id>`.
            let rest = first.splitn(3, '.').nth(2).unwrap_or(first);
            let ids: Vec<&str> = targets.iter().filter_map(|t| t.split('.').nth(1)).collect();
            format!("thin.vendors.{{{}}}.{rest}", ids.join(","))
        }
        [] => String::new(),
    }
}

/// Rewrite a parsed `router.yaml` to the current shape, in place; the legacy keys found, in
/// table order. `Err` only when a new-shape parent a key has to move into is not a mapping.
pub fn migrate(doc: &mut Value) -> Result<Vec<LegacyKey>, String> {
    let mut found = Vec::new();
    let Some(root) = doc.as_mapping_mut() else {
        return Ok(found);
    };
    for key in REMOVED {
        if root.remove(*key).is_some() {
            found.push(LegacyKey {
                key: (*key).to_string(),
                new_key: String::new(),
                action: LegacyAction::Removed,
            });
        }
    }
    let Some(thin) = root.get_mut("thin").and_then(Value::as_mapping_mut) else {
        return Ok(found);
    };
    for (legacy, targets) in THIN_ALIASES {
        let Some(value) = take(thin, legacy) else {
            continue;
        };
        let mut moved = false;
        for target in *targets {
            let (parent, leaf) = parent_of(thin, target)?;
            if !parent.contains_key(leaf) {
                parent.insert(Value::from(leaf), value.clone());
                moved = true;
            }
        }
        found.push(LegacyKey {
            key: format!("thin.{legacy}"),
            new_key: new_key_label(targets),
            action: if moved {
                LegacyAction::Moved
            } else {
                LegacyAction::Shadowed
            },
        });
    }
    Ok(found)
}

/// Remove and return the value at dotted `path` under `map`. A parent left empty by the
/// removal (`thin.bridge` once `tolerate` has moved) goes too; one with other keys stays, for
/// the structs to refuse by name.
fn take(map: &mut Mapping, path: &str) -> Option<Value> {
    match path.split_once('.') {
        None => map.remove(path),
        Some((head, rest)) => {
            let child = map.get_mut(head)?.as_mapping_mut()?;
            let value = take(child, rest)?;
            if child.is_empty() {
                map.remove(head);
            }
            Some(value)
        }
    }
}

/// The mapping that holds dotted `path`'s last segment, created (or a `null` replaced) on the
/// way down, and that segment.
fn parent_of<'a, 'p>(
    map: &'a mut Mapping,
    path: &'p str,
) -> Result<(&'a mut Mapping, &'p str), String> {
    let (dirs, leaf) = path.rsplit_once('.').unwrap_or(("", path));
    let mut at = map;
    let mut walked = String::from("thin");
    for seg in dirs.split('.').filter(|s| !s.is_empty()) {
        walked.push('.');
        walked.push_str(seg);
        let slot = at
            .entry(Value::from(seg))
            .or_insert_with(|| Value::Mapping(Mapping::new()));
        if slot.is_null() {
            *slot = Value::Mapping(Mapping::new());
        }
        at = slot
            .as_mapping_mut()
            .ok_or_else(|| format!("router.yaml: {walked} must be a mapping"))?;
    }
    Ok((at, leaf))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn doc(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn a_fanned_out_key_is_labelled_with_every_vendor() {
        assert_eq!(
            new_key_label(OAUTH_IDLE),
            "thin.vendors.{claude,grok,agy}.budgets.idle_seconds"
        );
        assert_eq!(
            new_key_label(&["vendors.cursor.cwd"]),
            "thin.vendors.cursor.cwd"
        );
    }

    #[test]
    fn a_legacy_key_moves_under_vendors() {
        let mut d = doc("thin:\n  cursor_cwd: C:/work\n  keepalive_seconds: 9\n");
        let found = migrate(&mut d).unwrap();
        assert_eq!(
            found,
            vec![LegacyKey {
                key: "thin.cursor_cwd".into(),
                new_key: "thin.vendors.cursor.cwd".into(),
                action: LegacyAction::Moved,
            }]
        );
        assert_eq!(
            d,
            doc("thin:\n  keepalive_seconds: 9\n  vendors:\n    cursor:\n      cwd: C:/work\n")
        );
    }

    #[test]
    fn the_new_key_wins_over_the_legacy_one() {
        let mut d = doc(
            "thin:\n  oauth_idle_seconds: 50\n  vendors:\n    grok:\n      budgets:\n        idle_seconds: 900\n",
        );
        let found = migrate(&mut d).unwrap();
        assert_eq!(
            found[0].action,
            LegacyAction::Moved,
            "claude and agy take it"
        );
        let v = &d["thin"]["vendors"];
        assert_eq!(v["grok"]["budgets"]["idle_seconds"], Value::from(900));
        assert_eq!(v["claude"]["budgets"]["idle_seconds"], Value::from(50));
        assert_eq!(v["agy"]["budgets"]["idle_seconds"], Value::from(50));

        let mut d = doc("thin:\n  cursor_cwd: a\n  vendors:\n    cursor:\n      cwd: b\n");
        let found = migrate(&mut d).unwrap();
        assert_eq!(found[0].action, LegacyAction::Shadowed);
        assert_eq!(d["thin"]["vendors"]["cursor"]["cwd"], Value::from("b"));
    }

    #[test]
    fn nested_bridge_tolerate_moves_and_an_emptied_parent_goes() {
        let mut d = doc("thin:\n  bridge:\n    tolerate:\n      extra_tools: [Foo]\n");
        migrate(&mut d).unwrap();
        assert!(d["thin"].get("bridge").is_none());
        assert_eq!(
            d["thin"]["vendors"]["claude"]["bridge"]["tolerate"]["extra_tools"][0],
            Value::from("Foo")
        );
    }

    #[test]
    fn measured_versions_is_dropped_and_reported() {
        let mut d = doc("listen: x\nmeasured_versions:\n  claude: 2.1.0\n");
        let found = migrate(&mut d).unwrap();
        assert_eq!(found[0].key, "measured_versions");
        assert_eq!(found[0].action, LegacyAction::Removed);
        assert!(d.get("measured_versions").is_none());
    }

    #[test]
    fn a_non_mapping_parent_is_an_error_naming_it() {
        let mut d = doc("thin:\n  cursor_cwd: a\n  vendors: 3\n");
        let err = migrate(&mut d).unwrap_err();
        assert!(err.contains("thin.vendors"), "{err}");
    }
}
