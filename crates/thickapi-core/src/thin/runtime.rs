//! Thin-surface runtime: one adapter per vendor, per-vendor OAuth state, private-dir sweeping.
//! Built once at `serve` start and shared by the route, `/ready` and `doctor`.
//!
//! [`VENDORS`] is the one table that maps a catalogue backend to its adapter and a `/ready`
//! vendor key to its block (R22 C6): `adapter_for`, `vendor_snapshot`, `wait_cli_versions`
//! and `model_check::vendor_of` all read it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::keys::LiveKeys;
use crate::config::models::{ModelEntry, Transport};
use crate::config::router::ClaudeOauthMode;
use crate::config::ConfigBundle;
use crate::thin::capture::{CaptureSwitches, Tap};
use crate::thin::claude_bridge::{BridgeSettings, ClaudeBridge};
use crate::thin::fixture_set;
use crate::thin::oauth::antigravity::AgyAdapter;
use crate::thin::oauth::codex::CodexAdapter;
use crate::thin::oauth::creds::{CredError, Token};
use crate::thin::oauth::cursor::CursorAdapter;
use crate::thin::oauth::grok::GrokAdapter;
use crate::thin::oauth::state::{redact_log, Availability, VendorState};
use crate::thin::oauth::vendor::CliBacked;
use crate::thin::oauth::ClaudeAdapter;
use crate::thin::{private_dir, ModelAdapter};
use crate::types::BackendId;

/// Dead private directories older than this are swept (SPEC §19.9).
pub const SWEEP_STALE_AFTER: Duration = Duration::from_secs(60);

/// One thin vendor: its `/ready` key, the catalogue backend it serves, its adapter and what
/// its `/ready` block is built from. A new vendor is one row in [`VENDORS`] plus its adapter.
pub struct VendorEntry {
    pub vendor: &'static str,
    pub backend: BackendId,
    pub adapter: fn(&ThinRuntime) -> Arc<dyn ModelAdapter>,
    pub view: fn(&ThinRuntime) -> VendorView<'_>,
}

/// What a vendor's `/ready` block reads.
pub enum VendorView<'a> {
    /// Claude: token, planned paths (direct and Haiku), the bridge, and both fixture sets.
    Claude,
    /// An OAuth-direct vendor whose CLI token and version a [`CliBacked`] owns (C1).
    Cli {
        cli: &'a CliBacked,
        state: &'a VendorState,
        /// Cursor also reports the client version it sends.
        client_version: bool,
    },
}

pub static VENDORS: &[VendorEntry] = &[
    VendorEntry {
        vendor: "claude",
        backend: BackendId::ClaudeCode,
        adapter: |rt| rt.claude.clone(),
        view: |_| VendorView::Claude,
    },
    VendorEntry {
        vendor: "grok",
        backend: BackendId::Grok,
        adapter: |rt| rt.grok.clone(),
        view: |rt| VendorView::Cli {
            cli: &rt.grok.cli,
            state: &rt.grok_state,
            client_version: false,
        },
    },
    VendorEntry {
        vendor: "agy",
        backend: BackendId::Agy,
        adapter: |rt| rt.agy.clone(),
        view: |rt| VendorView::Cli {
            cli: &rt.agy.cli,
            state: &rt.agy_state,
            client_version: false,
        },
    },
    VendorEntry {
        vendor: "cursor",
        backend: BackendId::Cursor,
        adapter: |rt| rt.cursor.clone(),
        view: |rt| VendorView::Cli {
            cli: &rt.cursor.cli,
            state: &rt.cursor_state,
            client_version: true,
        },
    },
    VendorEntry {
        vendor: "codex",
        backend: BackendId::Codex,
        adapter: |rt| rt.codex.clone(),
        view: |rt| VendorView::Cli {
            cli: &rt.codex.cli,
            state: &rt.codex_state,
            client_version: false,
        },
    },
];

/// The registry row serving `backend`; `None` for the paid `pi` lane.
pub fn entry_for(backend: BackendId) -> Option<&'static VendorEntry> {
    VENDORS.iter().find(|v| v.backend == backend)
}

pub struct ThinRuntime {
    tmp_root: PathBuf,
    trace_keep: u32,
    lan: bool,
    claude: Arc<ClaudeAdapter>,
    claude_state: Arc<VendorState>,
    bridge: Arc<ClaudeBridge>,
    grok: Arc<GrokAdapter>,
    grok_state: Arc<VendorState>,
    agy: Arc<AgyAdapter>,
    agy_state: Arc<VendorState>,
    cursor: Arc<CursorAdapter>,
    cursor_state: Arc<VendorState>,
    codex: Arc<CodexAdapter>,
    codex_state: Arc<VendorState>,
    /// Reload-on-mtime key lookup so keys minted or revoked after start work without a restart.
    keys: LiveKeys,
}

impl ThinRuntime {
    pub fn new(cfg: &ConfigBundle) -> Self {
        Self::with_tap(cfg, None)
    }

    /// Build with a wire tap on every adapter (`thickapi capture`, SPEC §19.11). `serve`
    /// passes `None`.
    pub fn with_tap(cfg: &ConfigBundle, tap: Tap) -> Self {
        Self::with_capture(cfg, tap, CaptureSwitches::default())
    }

    /// [`Self::with_tap`] plus the capture-only adapter switches (F1, #181). Only
    /// `thickapi capture` calls this; `serve` builds with [`Self::new`], every switch off.
    pub fn with_capture(cfg: &ConfigBundle, tap: Tap, switches: CaptureSwitches) -> Self {
        let settings = BridgeSettings::from_config(cfg);
        let tmp_root = settings.tmp_root.clone();
        let mut bridge = ClaudeBridge::new(settings);
        bridge.set_tap(tap.clone());
        let bridge = Arc::new(bridge);
        // Mirrored to disk so a restart remembers a block (SPEC §19.15 R1).
        let claude_state = Arc::new(VendorState::with_persistence(
            claude_state_path(cfg),
            bridge.claude_version(),
        ));
        let mut claude = ClaudeAdapter::new(cfg, bridge.clone(), claude_state.clone());
        claude.set_tap(tap.clone());
        let grok_state = Arc::new(VendorState::new());
        let mut grok = GrokAdapter::new(&cfg.router.thin, grok_state.clone());
        grok.set_tap(tap.clone());
        let agy_state = Arc::new(VendorState::new());
        let mut agy = AgyAdapter::new(&cfg.router.thin, agy_state.clone());
        agy.set_tap(tap.clone());
        let codex_state = Arc::new(VendorState::new());
        let mut codex = CodexAdapter::new(&cfg.router.thin, codex_state.clone());
        codex.set_tap(tap.clone());
        let cursor_state = Arc::new(VendorState::new());
        let mut cursor = CursorAdapter::new(cfg, cursor_state.clone());
        cursor.set_tap(tap);
        if let Some(shape) = switches.cursor_read_miss_shape {
            cursor.set_read_miss_shape(shape);
        }
        let runtime = Self {
            tmp_root,
            trace_keep: cfg.router.thin.trace_keep,
            lan: cfg.router.lan,
            claude: Arc::new(claude),
            claude_state,
            bridge,
            grok: Arc::new(grok),
            grok_state,
            agy: Arc::new(agy),
            agy_state,
            cursor: Arc::new(cursor),
            cursor_state,
            codex: Arc::new(codex),
            codex_state,
            keys: LiveKeys::from_snapshot(cfg.config_dir.join("keys.yaml"), cfg.keys.clone()),
        };
        // `--version` only, in the background: /ready and the first request must not be the
        // first probe, and a tokio worker must not sit in the poll (#124). `/ready` reads the
        // `_now` getters and never waits; later probes follow the exe's mtime. HTTP is not
        // touched here.
        for v in VENDORS {
            if let VendorView::Cli { cli, .. } = (v.view)(&runtime) {
                let _ = cli.cli_version_now();
            }
        }
        runtime
    }

    pub fn tmp_root(&self) -> &PathBuf {
        &self.tmp_root
    }

    pub fn bridge(&self) -> &Arc<ClaudeBridge> {
        &self.bridge
    }

    pub fn claude(&self) -> &Arc<ClaudeAdapter> {
        &self.claude
    }

    pub fn grok(&self) -> &Arc<GrokAdapter> {
        &self.grok
    }

    pub fn agy(&self) -> &Arc<AgyAdapter> {
        &self.agy
    }

    pub fn cursor(&self) -> &Arc<CursorAdapter> {
        &self.cursor
    }

    pub fn codex(&self) -> &Arc<CodexAdapter> {
        &self.codex
    }

    pub fn keys(&self) -> &LiveKeys {
        &self.keys
    }

    /// Remove private request directories whose daemon/child pids are gone.
    pub fn sweep(&self) -> usize {
        private_dir::sweep_dead_keeping(&self.tmp_root, SWEEP_STALE_AFTER, self.trace_keep)
    }

    /// The adapter serving a catalogue row, or `None` when the vendor has no thin adapter yet.
    pub fn adapter_for(&self, entry: &ModelEntry) -> Option<Arc<dyn ModelAdapter>> {
        if entry.transport == Transport::CliTurn || !entry.enabled {
            return None;
        }
        entry_for(entry.backend).map(|v| (v.adapter)(self))
    }

    /// Wait (bounded) until every vendor CLI's `--version` has been probed once, so a
    /// following [`Self::vendor_snapshot`] names them (`doctor`, `capture`).
    pub fn wait_cli_versions(&self) {
        for v in VENDORS {
            let _ = match (v.view)(self) {
                VendorView::Claude => self.bridge.claude_version(),
                VendorView::Cli { cli, .. } => cli.cli_version(),
            };
        }
    }

    /// `/ready` and `doctor` view, one block per [`VENDORS`] row: token presence/expiry, CLI
    /// version, path, last rejection, availability (#110) and fixture set. Under `lan: true`
    /// account notes and expiry are omitted (SPEC §19.9).
    pub fn vendor_snapshot(&self) -> Value {
        let now = crate::thin::oauth::creds::now_unix();
        let mut out = serde_json::Map::new();
        for v in VENDORS {
            let mut block = match (v.view)(self) {
                VendorView::Claude => self.claude_block(now),
                VendorView::Cli {
                    cli,
                    state,
                    client_version,
                } => self.cli_block(cli, state, client_version, now),
            };
            let version = block.get("cli_version").and_then(Value::as_str);
            let status = fixture_status(v.vendor, version);
            for (key, value) in status {
                crate::thin::json_insert(&mut block, key, value);
            }
            out.insert(v.vendor.to_string(), block);
        }
        Value::Object(out)
    }

    fn claude_block(&self, now: u64) -> Value {
        let (path, reason) = self.claude.planned_path();
        let (haiku_path, haiku_reason) = self.claude.planned_path_for("haiku");
        let cli_version = self.bridge.claude_version_now();
        let mut bridge = self.bridge.state().snapshot();
        // The bridge replays its own set, `fixtures/claude-bridge/` (R24).
        for (key, value) in fixture_status("claude-bridge", cli_version.as_deref()) {
            crate::thin::json_insert(&mut bridge, key, value);
        }
        let mut block = json!({
            "enabled": true,
            "oauth_mode": mode_str(self.claude.mode),
            "path": path,
            "path_reason": redact_log(&reason),
            "haiku_path": haiku_path,
            "haiku_path_reason": redact_log(&haiku_reason),
            "cli_version": cli_version,
            "oauth": self.claude_state.snapshot(self.claude.mode != ClaudeOauthMode::Off),
            "bridge": bridge,
            "token": self.token_block(self.claude.token()),
        });
        self.claude_availability(path, now).attach(&mut block);
        block
    }

    /// The grok / agy / Cursor block: each reads one [`CliBacked`] and one [`VendorState`].
    fn cli_block(&self, cli: &CliBacked, state: &VendorState, client: bool, now: u64) -> Value {
        let snap = cli.snapshot();
        let has_token = snap.token.is_ok();
        let mut block = json!({
            "enabled": true,
            "path": "oauth",
            "cli_version": snap.cli_version,
            "oauth": state.snapshot(true),
            "token": self.token_block(snap.token),
        });
        if client {
            crate::thin::json_insert(&mut block, "client_version", json!(snap.client_version));
        }
        vendor_availability(state, has_token, now).attach(&mut block);
        block
    }

    /// `present` / `expired`, plus `expires_in_seconds` and `note` unless `lan: true`.
    fn token_block(&self, token: Result<Token, CredError>) -> Value {
        match token {
            Ok(t) if self.lan => json!({ "present": true, "expired": t.expired() }),
            Ok(t) => json!({
                "present": true,
                "expired": t.expired(),
                "expires_in_seconds": t.seconds_left(),
                "note": t.note,
            }),
            Err(e) => json!({ "present": false, "error": e.to_string() }),
        }
    }

    /// Claude is available while one path can serve (#110). A direct-path block or 401 falls
    /// to the bridge and does not count; bridge drift counts only when the direct path is not
    /// the planned one (the documented double fault, or `claude_oauth.mode: off`). A quota or
    /// rate limit is the account's, so it stops both paths.
    fn claude_availability(&self, planned_path: &str, now: u64) -> Availability {
        let mut out = self.claude_state.availability(
            now,
            &["blocked", "suspected_block", "vendor_login_required"],
        );
        if planned_path == "bridge" {
            if let Some(drift) = self.bridge.state().drift() {
                out.block(format!("bridge drift: {}", drift.reason), None);
            }
        }
        out
    }
}

/// An OAuth-direct vendor with no stored credential cannot serve; otherwise its own record
/// decides (#110).
fn vendor_availability(state: &VendorState, has_token: bool, now: u64) -> Availability {
    let mut out = state.availability(now, &[]);
    if !has_token {
        out.block("no stored credential; log in with the vendor CLI", None);
    }
    out
}

/// `fixture_set` (the newest committed set's name) and, when the CLI version is known but has
/// no set, `fixture_warn` (SPEC §19.11).
fn fixture_status(vendor: &str, cli_version: Option<&str>) -> Vec<(&'static str, Value)> {
    let root = fixture_set::compiled_fixtures_root();
    let mut out = Vec::new();
    if let Some(name) = fixture_set::newest_set(&root, vendor)
        .and_then(|set| set.file_name().map(|n| n.to_string_lossy().into_owned()))
    {
        out.push(("fixture_set", json!(name)));
    }
    let version = cli_version.filter(|v| !v.is_empty());
    if let Some(warn) = version.and_then(|v| fixture_set::missing_fixture_warn(&root, vendor, v)) {
        out.push(("fixture_warn", json!(warn)));
    }
    out
}

/// `<config_dir>/thin/claude_oauth_state.json` (SPEC §19.15 R1).
pub fn claude_state_path(cfg: &ConfigBundle) -> PathBuf {
    cfg.config_dir.join("thin").join("claude_oauth_state.json")
}

fn mode_str(mode: ClaudeOauthMode) -> &'static str {
    match mode {
        ClaudeOauthMode::Prefer => "prefer",
        ClaudeOauthMode::Off => "off",
        ClaudeOauthMode::Only => "only",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::router::{
        AgyCfg, ClaudeCfg, ClaudeOauthConfig, CodexCfg, CursorCfg, GrokCfg, RouterConfig,
        ThinConfig, Vendors,
    };
    use crate::config::{AccountsConfig, CombosConfig, KeysConfig, ModelsConfig};
    use std::collections::BTreeSet;

    /// A runtime over stored credentials in `tmp` (or none), with no real Claude CLI.
    fn runtime(tmp: &tempfile::TempDir, with_creds: bool, lan: bool) -> ThinRuntime {
        ThinRuntime::new(&bundle(tmp, with_creds, lan))
    }

    /// The config [`runtime`] builds from.
    fn bundle(tmp: &tempfile::TempDir, with_creds: bool, lan: bool) -> ConfigBundle {
        let dir = tmp.path();
        let creds = |name: &str, body: Value| {
            let path = dir.join(name);
            if with_creds {
                std::fs::write(&path, body.to_string()).unwrap();
            }
            Some(path)
        };
        let far = "2999-01-01T00:00:00Z";
        let cursor_jwt = {
            use base64::Engine;
            let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(json!({"sub": "pin", "exp": 32_503_680_000u64}).to_string());
            format!("h.{claims}.s")
        };
        let thin = ThinConfig {
            vendors: Vendors {
                claude: ClaudeCfg {
                    path: Some(dir.join("missing").join("claude.exe")),
                    ..Default::default()
                },
                grok: GrokCfg {
                    credentials_path: creds(
                        "grok.json",
                        json!({"https://auth.x.ai::client": {"key": "pin.grok.key",
                            "auth_mode": "oidc", "refresh_token": "r", "expires_at": far,
                            "oidc_issuer": "https://auth.x.ai", "oidc_client_id": "client"}}),
                    ),
                    ..Default::default()
                },
                agy: AgyCfg {
                    credentials_path: creds(
                        "agy.json",
                        json!({"token": {"access_token": "pin.agy", "token_type": "Bearer",
                            "refresh_token": "r", "expiry": far}, "auth_method": "consumer"}),
                    ),
                    ..Default::default()
                },
                cursor: CursorCfg {
                    credentials_path: creds(
                        "cursor.json",
                        json!({"accessToken": cursor_jwt, "refreshToken": cursor_jwt}),
                    ),
                    ..Default::default()
                },
                codex: CodexCfg {
                    credentials_path: creds(
                        "codex.json",
                        json!({"auth_mode": "chatgpt", "tokens": {"access_token": cursor_jwt,
                            "refresh_token": "r", "id_token": "i", "account_id": "a"}}),
                    ),
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        ConfigBundle {
            config_dir: dir.to_path_buf(),
            router: RouterConfig {
                data_dir: dir.join("data"),
                lan,
                claude_oauth: ClaudeOauthConfig {
                    credentials_path: creds(
                        "claude.json",
                        json!({"claudeAiOauth": {"accessToken": "pin-claude", "refreshToken": "r",
                            "expiresAt": 9_999_999_999_999u64, "subscriptionType": "max"}}),
                    ),
                    ..Default::default()
                },
                thin,
                ..Default::default()
            },
            accounts: AccountsConfig {
                pools: Default::default(),
            },
            models: ModelsConfig {
                aliases: Default::default(),
                models: vec![],
            },
            combos: CombosConfig {
                combos: Default::default(),
            },
            keys: KeysConfig::default(),
        }
    }

    /// G1 (#175): the runtime `serve` builds (`new`) and a tap-only capture runtime answer a
    /// missing Cursor pre-read with `FileNotFound`; only `with_capture` picks another shape.
    #[test]
    fn read_miss_shape_is_file_not_found_unless_capture_sets_it() {
        use crate::thin::oauth::cursor::ReadMissShape;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = bundle(&tmp, false, false);
        let serve = ThinRuntime::new(&cfg);
        assert_eq!(
            serve.cursor().read_miss_shape(),
            ReadMissShape::FileNotFound
        );
        let tapped = ThinRuntime::with_tap(&cfg, None);
        assert_eq!(
            tapped.cursor().read_miss_shape(),
            ReadMissShape::FileNotFound
        );
        let capture = ThinRuntime::with_capture(
            &cfg,
            None,
            CaptureSwitches {
                cursor_read_miss_shape: Some(ReadMissShape::Rejected),
            },
        );
        assert_eq!(capture.cursor().read_miss_shape(), ReadMissShape::Rejected);
        assert_eq!(CaptureSwitches::default().cursor_read_miss_shape, None);
    }

    fn keys(v: &Value) -> BTreeSet<String> {
        v.as_object()
            .unwrap_or_else(|| panic!("not an object: {v}"))
            .keys()
            .filter(|k| k.as_str() != "fixture_warn") // present only when the CLI is known
            .cloned()
            .collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    const COMMON: &[&str] = &[
        "enabled",
        "path",
        "cli_version",
        "oauth",
        "token",
        "available",
        "retry_in_seconds",
        "reason",
        "rows_unavailable",
        "fixture_set",
    ];

    /// C6 pin: the `/ready` `thin.vendors.*` field names per vendor. The registry refactor
    /// must leave every one of them in place.
    #[test]
    fn ready_vendor_field_names_are_pinned() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = runtime(&tmp, true, false).vendor_snapshot();
        assert_eq!(
            keys(&snap),
            set(&["claude", "grok", "agy", "cursor", "codex"])
        );
        let mut claude = set(COMMON);
        claude.extend(set(&[
            "oauth_mode",
            "path_reason",
            "haiku_path",
            "haiku_path_reason",
            "bridge",
        ]));
        assert_eq!(keys(&snap["claude"]), claude, "{snap}");
        assert_eq!(keys(&snap["grok"]), set(COMMON), "{snap}");
        assert_eq!(keys(&snap["agy"]), set(COMMON), "{snap}");
        assert_eq!(keys(&snap["codex"]), set(COMMON), "{snap}");
        let mut cursor = set(COMMON);
        cursor.insert("client_version".into());
        assert_eq!(keys(&snap["cursor"]), cursor, "{snap}");
        for vendor in ["claude", "grok", "agy", "cursor", "codex"] {
            assert_eq!(
                keys(&snap[vendor]["token"]),
                set(&["present", "expired", "expires_in_seconds", "note"]),
                "{vendor}: {snap}"
            );
            assert_eq!(snap[vendor]["enabled"], true);
        }
        for vendor in ["grok", "agy", "cursor", "codex"] {
            assert_eq!(snap[vendor]["path"], "oauth");
        }
        assert!(
            keys(&snap["claude"]["bridge"]).contains("skipped"),
            "{snap}"
        );
    }

    /// Under `lan: true` the token block drops expiry and notes; a missing credential is
    /// `present: false` plus the error, and the vendor is unavailable (#110).
    #[test]
    fn ready_token_block_under_lan_and_without_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = runtime(&tmp, true, true).vendor_snapshot();
        for vendor in ["claude", "grok", "agy", "cursor", "codex"] {
            assert_eq!(
                keys(&snap[vendor]["token"]),
                set(&["present", "expired"]),
                "{vendor}: {snap}"
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        let snap = runtime(&tmp, false, false).vendor_snapshot();
        for vendor in ["claude", "grok", "agy", "cursor", "codex"] {
            assert_eq!(
                keys(&snap[vendor]["token"]),
                set(&["present", "error"]),
                "{vendor}: {snap}"
            );
        }
        for vendor in ["grok", "agy", "cursor", "codex"] {
            assert_eq!(snap[vendor]["available"], false, "{vendor}: {snap}");
        }
    }
}
