//! Catalogue currency check (SPEC §10.3): every `subs/*` row in `models.yaml` must name the
//! newest version of its model family that the vendor's own list offers. Fetching is per
//! vendor (`ClaudeAdapter::list_models`, `GrokAdapter::list_models`, `agy models`,
//! `cursor-agent --list-models`); the comparison is pure and tested here.
//!
//! Model ids are read as `<family tokens> + <version numbers> + <effort/mode suffixes>`:
//! `claude-fable-5-1-thinking-high` → family `claude-fable`, version `5.1`;
//! `gemini-3.8-flash-tiered` → `gemini-flash` `3.8`; `cursor-grok-4.7-high` → `cursor-grok`
//! `4.7`; `auto` → `auto`, no version. Date stamps (`-20251101`) are ignored.

use std::collections::BTreeMap;

use crate::config::models::{ModelEntry, ModelsConfig};
use crate::thin::oauth::cursor::CursorAdapter;
use crate::thin::runtime::ThinRuntime;
use crate::types::BackendId;

/// The `context` value `wire_model_with_context` sends when a Cursor row does not ask for
/// a wider tier — the tier `cursor-agent --list-models` actually prints one row for.
const CURSOR_DEFAULT_CONTEXT: &str = "256k";

/// Effort levels, modes and marketing suffixes that never distinguish a model family.
const IGNORED_TOKENS: &[&str] = &[
    "low", "medium", "high", "xhigh", "max", "minimal", "none", "extra", "thinking", "fast",
    "tiered", "latest", "batch",
];

/// `(family, version)` for a vendor or catalogue id. Family tokens are joined with `-`.
pub fn family_version(id: &str) -> (String, Vec<u32>) {
    let mut family: Vec<&str> = Vec::new();
    let mut version: Vec<u32> = Vec::new();
    for token in id.split(['-', '.']) {
        if token.is_empty() {
            continue;
        }
        if token.chars().all(|c| c.is_ascii_digit()) {
            // Date stamps (8 digits) and build numbers never rank versions.
            if token.len() <= 3 {
                if let Ok(n) = token.parse::<u32>() {
                    version.push(n);
                }
            }
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if IGNORED_TOKENS.contains(&lower.as_str()) {
            continue;
        }
        family.push(token);
    }
    (family.join("-").to_ascii_lowercase(), version)
}

fn version_string(v: &[u32]) -> String {
    v.iter().map(u32::to_string).collect::<Vec<_>>().join(".")
}

/// The thin vendor a catalogue row belongs to — the list `models check` compares it with, its
/// `/ready` key and the name in its errors; `None` for the paid `pi` lane. The one answer is
/// the registry row (`runtime::VENDORS`) serving the backend.
pub fn vendor_of(backend: BackendId) -> Option<&'static str> {
    crate::thin::runtime::entry_for(backend).map(|v| v.vendor)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowStatus {
    /// The newest version of its family the vendor offers.
    Current,
    /// The vendor has a newer version of the same family (`newest` is one vendor id for it).
    Stale { newest: String },
    /// No vendor id shares the row's family and version.
    Missing,
    /// The vendor list could not be fetched; nothing known.
    Unchecked,
    /// A Cursor row whose `wire_model_with_context` sends a non-default `context` parameter:
    /// `cursor-agent --list-models` prints one id per family and cannot distinguish tiers, so
    /// family/version matching a live id does not confirm this row's tier (#147).
    ContextUnverifiable,
}

#[derive(Debug, Clone)]
pub struct RowReport {
    pub id: String,
    pub vendor: &'static str,
    pub wire: String,
    pub status: RowStatus,
}

#[derive(Debug, Clone)]
pub struct VendorReport {
    pub vendor: &'static str,
    /// `Err(reason)` when the list could not be fetched.
    pub ids: Result<Vec<String>, String>,
    /// Families the vendor offers that no catalogue row names: `(family, newest version)`.
    pub uncatalogued: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    pub rows: Vec<RowReport>,
    pub vendors: Vec<VendorReport>,
}

impl CheckReport {
    /// `true` when no row is stale or missing. Unchecked and context-unverifiable rows
    /// (`[warn]`) do not fail the report.
    pub fn current(&self) -> bool {
        self.rows.iter().all(|r| {
            matches!(
                r.status,
                RowStatus::Current | RowStatus::Unchecked | RowStatus::ContextUnverifiable
            )
        })
    }

    pub fn stale_or_missing(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.status, RowStatus::Stale { .. } | RowStatus::Missing))
            .count()
    }

    /// Doctor-style lines: one per row, then one per vendor.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.rows {
            out.push(match &r.status {
                RowStatus::Current => format!("  [ok] {} -> {} (newest in family)", r.id, r.wire),
                RowStatus::Stale { newest } => format!(
                    "  [fail] {} -> {} is behind: {} offers {}",
                    r.id, r.wire, r.vendor, newest
                ),
                RowStatus::Missing => format!(
                    "  [fail] {} -> {} not in the {} list",
                    r.id, r.wire, r.vendor
                ),
                RowStatus::Unchecked => format!("  [warn] {} -> {} unchecked", r.id, r.wire),
                RowStatus::ContextUnverifiable => format!(
                    "  [warn] {}: context tier not verifiable from cursor-agent --list-models",
                    r.id
                ),
            });
        }
        for v in &self.vendors {
            match &v.ids {
                Ok(ids) => {
                    let extra: Vec<String> = v
                        .uncatalogued
                        .iter()
                        .map(|(f, ver)| {
                            if ver.is_empty() {
                                f.clone()
                            } else {
                                format!("{f} {ver}")
                            }
                        })
                        .collect();
                    out.push(format!(
                        "  {} list: {} ids; not catalogued: {}",
                        v.vendor,
                        ids.len(),
                        if extra.is_empty() {
                            "-".to_string()
                        } else {
                            extra.join(", ")
                        }
                    ));
                }
                Err(e) => out.push(format!("  [warn] {} list unavailable: {}", v.vendor, e)),
            }
        }
        out
    }
}

/// Rows the check covers: enabled `subs/*` rows whose backend has a vendor list.
fn checked_rows(models: &ModelsConfig) -> impl Iterator<Item = (&ModelEntry, &'static str)> {
    models
        .models
        .iter()
        .filter(|m| m.enabled && m.id.starts_with("subs/"))
        .filter_map(|m| vendor_of(m.backend).map(|v| (m, v)))
}

/// Newest version per family in a vendor list, with one id carrying it.
fn newest_by_family(ids: &[String]) -> BTreeMap<String, (Vec<u32>, String)> {
    let mut out: BTreeMap<String, (Vec<u32>, String)> = BTreeMap::new();
    for id in ids {
        let (family, version) = family_version(id);
        match out.get_mut(&family) {
            Some((best, best_id)) => {
                if version > *best || (version == *best && id.len() < best_id.len()) {
                    *best = version;
                    *best_id = id.clone();
                }
            }
            None => {
                out.insert(family, (version, id.clone()));
            }
        }
    }
    out
}

/// A Cursor row whose `wire_model_with_context` sends a `context` parameter other than the
/// default (`256k`): `cursor-agent --list-models` prints one id per family and never a
/// context tier, so family/version matching cannot confirm this row picked the right one
/// (#147).
fn cursor_context_unverifiable(entry: &ModelEntry) -> bool {
    if entry.backend != BackendId::Cursor {
        return false;
    }
    let context = (entry.context_window == 500_000).then_some("500k");
    let wire = CursorAdapter::wire_model_with_context(entry.wire_model(), None, context);
    wire.params
        .iter()
        .any(|(k, v)| k == "context" && v != CURSOR_DEFAULT_CONTEXT)
}

/// Compare the catalogue with the vendor lists. Vendors absent from `vendors` leave their
/// rows `Unchecked`.
pub fn compare(
    models: &ModelsConfig,
    vendors: &[(&'static str, Result<Vec<String>, String>)],
) -> CheckReport {
    let mut report = CheckReport::default();
    for (entry, vendor) in checked_rows(models) {
        let wire = entry.wire_model().to_string();
        let ids = vendors
            .iter()
            .find(|(v, _)| *v == vendor)
            .and_then(|(_, r)| r.as_ref().ok());
        let status = match ids {
            None => RowStatus::Unchecked,
            Some(ids) => {
                let (family, version) = family_version(&wire);
                let newest = newest_by_family(ids);
                match newest.get(&family) {
                    None => RowStatus::Missing,
                    Some((best, best_id)) => {
                        let present = ids.iter().any(|id| {
                            let (f, v) = family_version(id);
                            f == family && v == version
                        });
                        if *best > version {
                            RowStatus::Stale {
                                newest: best_id.clone(),
                            }
                        } else if present && cursor_context_unverifiable(entry) {
                            RowStatus::ContextUnverifiable
                        } else if present {
                            RowStatus::Current
                        } else {
                            RowStatus::Missing
                        }
                    }
                }
            }
        };
        report.rows.push(RowReport {
            id: entry.id.clone(),
            vendor,
            wire,
            status,
        });
    }
    for (vendor, ids) in vendors {
        let uncatalogued = match ids {
            Ok(ids) => {
                let catalogued: Vec<String> = checked_rows(models)
                    .filter(|(_, v)| v == vendor)
                    .map(|(m, _)| family_version(m.wire_model()).0)
                    .collect();
                newest_by_family(ids)
                    .into_iter()
                    .filter(|(family, _)| !catalogued.contains(family))
                    .map(|(family, (version, _))| (family, version_string(&version)))
                    .collect()
            }
            Err(_) => Vec::new(),
        };
        report.vendors.push(VendorReport {
            vendor,
            ids: ids.clone(),
            uncatalogued,
        });
    }
    report
}

/// Fetch every vendor list the runtime can reach (concurrently) and compare.
pub async fn run(runtime: &ThinRuntime, models: &ModelsConfig) -> CheckReport {
    let (claude, grok, agy, cursor, codex) = tokio::join!(
        runtime.claude().list_models(),
        runtime.grok().list_models(),
        runtime.agy().list_models(),
        runtime.cursor().list_models(),
        runtime.codex().list_models(),
    );
    let as_reason = |r: Result<Vec<String>, crate::thin::ThinError>| r.map_err(|e| e.message);
    let vendors = vec![
        ("claude", as_reason(claude)),
        ("grok", as_reason(grok)),
        ("agy", as_reason(agy)),
        ("cursor", as_reason(cursor)),
        ("codex", as_reason(codex)),
    ];
    compare(models, &vendors)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::models::{default_input, Transport};

    fn fv(id: &str) -> (String, Vec<u32>) {
        family_version(id)
    }

    #[test]
    fn family_and_version_parse_vendor_shapes() {
        assert_eq!(fv("claude-fable-5-1"), ("claude-fable".into(), vec![5, 1]));
        assert_eq!(
            fv("claude-fable-5-1-thinking-high"),
            ("claude-fable".into(), vec![5, 1])
        );
        assert_eq!(fv("claude-opus-5"), ("claude-opus".into(), vec![5]));
        assert_eq!(
            fv("claude-opus-4-5-20251101"),
            ("claude-opus".into(), vec![4, 5])
        );
        assert_eq!(
            fv("claude-4.6-sonnet-medium"),
            ("claude-sonnet".into(), vec![4, 6])
        );
        assert_eq!(
            fv("gemini-3.8-flash-tiered"),
            ("gemini-flash".into(), vec![3, 8])
        );
        assert_eq!(
            fv("gemini-3.8-flash-high"),
            ("gemini-flash".into(), vec![3, 8])
        );
        assert_eq!(fv("gemini-3.1-pro-low"), ("gemini-pro".into(), vec![3, 1]));
        assert_eq!(
            fv("cursor-grok-4.7-high-fast"),
            ("cursor-grok".into(), vec![4, 7])
        );
        assert_eq!(fv("grok-4.7"), ("grok".into(), vec![4, 7]));
        assert_eq!(
            fv("grok-4.20-multi-agent"),
            ("grok-multi-agent".into(), vec![4, 20])
        );
        assert_eq!(fv("gpt-5.6-sol-xhigh"), ("gpt-sol".into(), vec![5, 6]));
        assert_eq!(fv("auto"), ("auto".into(), vec![]));
    }

    #[test]
    fn versions_order_lexicographically() {
        assert!(vec![5u32, 1] > vec![5]);
        assert!(vec![5u32] > vec![4, 8]);
        assert!(vec![4u32, 20] > vec![4, 6]);
    }

    fn row(id: &str, backend: BackendId, wire: &str) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            backend,
            cli_model: wire.into(),
            context_window: 0,
            max_tokens: 0,
            thinking: true,
            cost: 0.0,
            name: String::new(),
            provider: None,
            transport: Transport::OauthDirect,
            vendor_model: None,
            enabled: true,
            input: default_input(),
        }
    }

    fn ids(list: &[&str]) -> Result<Vec<String>, String> {
        Ok(list.iter().map(|s| s.to_string()).collect())
    }

    /// A Cursor row with a given `context_window`, for the context-tier warn case.
    fn cursor_row(id: &str, wire: &str, context_window: u64) -> ModelEntry {
        ModelEntry {
            context_window,
            ..row(id, BackendId::Cursor, wire)
        }
    }

    #[test]
    fn current_stale_missing_and_unchecked() {
        let models = ModelsConfig {
            aliases: Default::default(),
            models: vec![
                row(
                    "subs/claude/fable",
                    BackendId::ClaudeCode,
                    "claude-fable-5-1",
                ),
                row("subs/claude/opus", BackendId::ClaudeCode, "claude-opus-4-6"),
                row("subs/claude/gone", BackendId::ClaudeCode, "claude-haiku-9"),
                row("subs/cursor/auto", BackendId::Cursor, "auto"),
                row(
                    "subs/cursor/fable",
                    BackendId::Cursor,
                    "claude-fable-5-1-thinking-high",
                ),
                row("subs/grok/grok-4.7", BackendId::Grok, "grok-4.7"),
                row(
                    "or/anthropic/claude-opus-5",
                    BackendId::Pi,
                    "anthropic/claude-opus-5",
                ),
            ],
        };
        let vendors = vec![
            (
                "claude",
                ids(&[
                    "claude-fable-5-1",
                    "claude-opus-5",
                    "claude-opus-4-6",
                    "claude-sonnet-5",
                    "claude-haiku-4-5-20251001",
                ]),
            ),
            (
                "cursor",
                ids(&[
                    "auto",
                    "claude-fable-5-1-thinking-low",
                    "claude-fable-5-1-thinking-high",
                    "claude-fable-5-thinking-high",
                    "cursor-grok-4.7-high",
                ]),
            ),
            ("grok", Err("token expired".to_string())),
        ];
        let report = compare(&models, &vendors);
        let status = |id: &str| {
            report
                .rows
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.status.clone())
                .expect(id)
        };
        assert_eq!(status("subs/claude/fable"), RowStatus::Current);
        assert_eq!(
            status("subs/claude/opus"),
            RowStatus::Stale {
                newest: "claude-opus-5".into()
            }
        );
        assert_eq!(status("subs/claude/gone"), RowStatus::Missing);
        assert_eq!(status("subs/cursor/auto"), RowStatus::Current);
        assert_eq!(status("subs/cursor/fable"), RowStatus::Current);
        assert_eq!(status("subs/grok/grok-4.7"), RowStatus::Unchecked);
        assert!(
            report.rows.iter().all(|r| !r.id.starts_with("or/")),
            "paid pi rows have no vendor list"
        );
        assert!(!report.current());
        assert_eq!(report.stale_or_missing(), 2);

        let claude = report
            .vendors
            .iter()
            .find(|v| v.vendor == "claude")
            .unwrap();
        assert_eq!(
            claude.uncatalogued,
            vec![("claude-sonnet".to_string(), "5".to_string())],
            "haiku is catalogued by family (the row is merely missing its version)"
        );
        let cursor = report
            .vendors
            .iter()
            .find(|v| v.vendor == "cursor")
            .unwrap();
        let families: Vec<&str> = cursor
            .uncatalogued
            .iter()
            .map(|(f, _)| f.as_str())
            .collect();
        assert_eq!(families, vec!["cursor-grok"]);
        let grok = report.vendors.iter().find(|v| v.vendor == "grok").unwrap();
        assert!(grok.ids.is_err() && grok.uncatalogued.is_empty());

        let text = report.lines().join("\n");
        assert!(text.contains(
            "[fail] subs/claude/opus -> claude-opus-4-6 is behind: claude offers claude-opus-5"
        ));
        assert!(text.contains("[warn] grok list unavailable: token expired"));
    }

    #[test]
    fn cursor_row_asking_a_non_default_context_tier_warns_instead_of_ok() {
        // `cursor-agent --list-models` prints `grok-4.7-high` once; it never names a
        // `context` tier, so a row whose `context_window` makes `wire_model_with_context`
        // send `context=500k` cannot be confirmed against the live list (#147).
        let models = ModelsConfig {
            aliases: Default::default(),
            models: vec![
                cursor_row("subs/cursor/grok-4.7", "grok-4.7-high", 256_000),
                cursor_row("subs/cursor/grok-4.7-reviewer", "grok-4.7-high", 500_000),
            ],
        };
        let vendors = vec![("cursor", ids(&["auto", "grok-4.7-high"]))];
        let report = compare(&models, &vendors);
        let status = |id: &str| {
            report
                .rows
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.status.clone())
                .expect(id)
        };
        assert_eq!(status("subs/cursor/grok-4.7"), RowStatus::Current);
        assert_eq!(
            status("subs/cursor/grok-4.7-reviewer"),
            RowStatus::ContextUnverifiable
        );
        // Unverifiable does not fail the check — it is a warning, not a finding.
        assert!(report.current());
        assert_eq!(report.stale_or_missing(), 0);
        let text = report.lines().join("\n");
        assert!(text.contains(
            "[warn] subs/cursor/grok-4.7-reviewer: context tier not verifiable from cursor-agent --list-models"
        ));
    }

    #[test]
    fn example_catalogue_is_current_against_recorded_vendor_lists() {
        // The vendor lists as measured 2026-09-17; the example catalogue must be the newest of
        // every family it names. When a vendor ships a successor, update both.
        let models: ModelsConfig =
            serde_yaml::from_str(include_str!("../../../../configs/models.yaml.example"))
                .expect("example");
        let vendors = vec![
            (
                // #146 bumped the catalogued Opus row to 5.5; this list is unrelated to A3
                // (#147) but was left stale (fixed here so the pinned-catalogue test is
                // green again, per the exit criteria).
                "claude",
                ids(&[
                    "claude-fable-5-1",
                    "claude-opus-5-5",
                    "claude-sonnet-5-5",
                    "claude-haiku-4-5",
                    "claude-sonnet-5",
                    "claude-fable-5",
                    "claude-opus-4-8",
                ]),
            ),
            (
                "grok",
                ids(&["grok-4.7", "grok-4.7-build-fast", "grok-4.6", "grok-4.5"]),
            ),
            (
                "agy",
                // Plus Gemini 4 Argon and Claude Opus 5.5 (`agy 1.4.7`, 2026-10-24).
                ids(&[
                    "gemini-4-argon-medium",
                    "gemini-4-argon-high",
                    "gemini-3.8-flash-high",
                    "gemini-3.7-flash-high",
                    "gemini-3.1-pro-high",
                    "claude-opus-5-5-medium",
                    "claude-sonnet-5-5-medium",
                    "claude-sonnet-4-6",
                ]),
            ),
            (
                // What `cursor-agent --list-models` prints for the families this catalogue
                // names. Measured 2026-09-17, plus `composer-2.5` (live list, 2026-10-12).
                // One id per family; no separate id for the 500k Grok 4.7 tier — that tier is
                // a `context` `ModelParameter` on the same `grok-4.7-high` slug (§19.8), not a
                // second catalogue entry, so it cannot appear in this list. `composer-2.5-fast`
                // is the fast variant of the same version, not a newer family member.
                "cursor",
                ids(&[
                    "auto",
                    "claude-fable-5-1-thinking-high",
                    "claude-fable-5-1-thinking-xhigh",
                    "grok-4.7-high",
                    "cursor-grok-4.6-high",
                    "cursor-grok-4.5-high",
                    "claude-opus-5-thinking-high",
                    "composer-2.5",
                    "composer-2.5-fast",
                ]),
            ),
            (
                // `GET /codex/models` without `visibility: hide` rows (2026-10-24).
                "codex",
                ids(&[
                    "gpt-6.1-sol",
                    "gpt-6-astra",
                    "gpt-6-sol",
                    "gpt-6-luna",
                    "gpt-5.6-sol",
                    "gpt-5.6-terra",
                    "gpt-5.6-luna",
                ]),
            ),
        ];
        let report = compare(&models, &vendors);
        assert!(report.current(), "{}", report.lines().join("\n"));
        assert_eq!(
            report.rows.len(),
            14,
            "fourteen subscription rows; codex Sol, Astra and Luna added 2026-10-25; K1 added subs/claude/sonnet, A4 dropped subs/claude/haiku, cursor grok keeps one row (500k; #147 dropped the separate row), Composer 2.5 restored 2026-10-11, agy Argon and Opus added 2026-10-24"
        );
        assert_eq!(
            report
                .rows
                .iter()
                .find(|r| r.id == "subs/cursor/grok-4.7")
                .map(|r| r.status.clone())
                .expect("subs/cursor/grok-4.7"),
            RowStatus::ContextUnverifiable
        );
        assert!(report.lines().join("\n").contains(
            "[warn] subs/cursor/grok-4.7: context tier not verifiable from cursor-agent --list-models"
        ));
    }
}
