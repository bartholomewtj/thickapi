#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use thickapi_core::router::{claude_cli_session, resolve, resolve_combo, resolve_model};
use thickapi_core::{Store, TurnSpec};

fn example_config() -> thickapi_core::config::ModelsConfig {
    serde_yaml::from_str(include_str!("../../../configs/models.yaml.example")).expect("models")
}

#[test]
fn subs_claude_rows_resolve_to_newest_versions() {
    let models = example_config();
    let fable = resolve_model("subs/claude/fable", &models).expect("resolve");
    assert_eq!(fable.cli_model, "claude-fable-5-1");
    let unprefixed = resolve_model("claude/fable", &models).expect("unprefixed");
    assert_eq!(unprefixed.id, fable.id);
    assert_eq!(unprefixed.cli_model, "claude-fable-5-1");
    let opus = resolve_model("subs/claude/opus", &models).expect("resolve");
    // #146 bumped the row to Opus 5.5; unrelated to A3 (#147) but stale here.
    assert_eq!(opus.cli_model, "claude-opus-5-5");
    let sonnet = resolve_model("subs/claude/sonnet", &models).expect("resolve");
    assert_eq!(sonnet.cli_model, "claude-sonnet-5-5");
    let sonnet_alias = resolve_model("sonnet", &models).expect("alias");
    assert_eq!(sonnet_alias.id, "subs/claude/sonnet");
    assert!(
        resolve_model("subs/claude/haiku", &models).is_err(),
        "A4: haiku is not catalogued (SPEC 10.3, families nobody wants are not rows)"
    );
    assert!(
        resolve_model("haiku", &models).is_err(),
        "A4: the haiku alias is gone with the row"
    );
}

#[test]
fn new_claude_session_uses_session_id_not_resume() {
    let config = example_config_bundle();
    let store = Store::open_in_memory().expect("store");
    let spec = TurnSpec {
        model: "subs/claude/fable".into(),
        prompt: "hi".into(),
        system_prompt: None,
        system_prompt_file: None,
        cwd: std::env::temp_dir(),
        session_id: "fresh-sess".into(),
        thinking: thickapi_core::Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Off,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo: None,
        allow_paid: false,
        timeout_seconds: 30,
        client: None,
        turn_id: None,
        metadata: serde_json::Map::new(),
    };
    let resolved = resolve(&spec, &config, &store).expect("resolve");
    assert!(resolved.session.cli_session.is_none());
}

fn example_config_bundle() -> thickapi_core::ConfigBundle {
    thickapi_core::ConfigBundle {
        config_dir: std::path::PathBuf::from("."),
        router: thickapi_core::config::RouterConfig {
            listen: "127.0.0.1:8787".into(),
            data_dir: std::path::PathBuf::from("."),
            lan: false,
            default_timeout_seconds: 30,
            queue_timeout_seconds: 300,
            max_queue_depth: 16,
            max_paid_usd_per_day: 5.0,
            drain_seconds: 120,
            refuse_paths: vec![],
            ..Default::default()
        },
        accounts: serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
            .expect("accounts"),
        models: example_config(),
        combos: serde_yaml::from_str(include_str!("../../../configs/combos.yaml.example"))
            .expect("combos"),
        keys: thickapi_core::config::keys::KeysConfig::default(),
    }
}

#[test]
fn uuid_v5_session_stable() {
    let a = claude_cli_session("sess-1");
    let b = claude_cli_session("sess-1");
    assert_eq!(a, b);
    assert_ne!(a, claude_cli_session("sess-2"));
}

#[test]
fn unknown_model_errors() {
    let models = example_config();
    let err = resolve_model("subs/claude/missing", &models).unwrap_err();
    assert!(err.to_string().contains("unknown"));
}

#[test]
fn subs_agy_model_resolves() {
    let models = example_config();
    // T3: effort-suffixed ids are gone; the row keeps the CLI id for the thick lane and carries
    // the Code Assist catalogue id for the thin adapter.
    let resolved = resolve_model("subs/agy/gemini-3.8-flash", &models).expect("resolve");
    assert_eq!(resolved.cli_model, "gemini-3.8-flash");
    assert_eq!(resolved.backend, thickapi_core::BackendId::Agy);
    let unprefixed = resolve_model("agy/gemini-3.8-flash", &models).expect("unprefixed");
    assert_eq!(unprefixed.id, resolved.id);
    let entry = models.find("subs/agy/gemini-3.8-flash").expect("row");
    assert_eq!(entry.wire_model(), "gemini-3.8-flash-tiered");
    // Argon and Opus name their default level; the thin adapter swaps the suffix per request.
    let argon = resolve_model("argon", &models).expect("argon alias");
    assert_eq!(argon.id, "subs/agy/gemini-4-argon");
    assert_eq!(argon.wire_model(), "gemini-4-argon-medium");
    let opus = resolve_model("agy-opus", &models).expect("agy opus alias");
    assert_eq!(opus.id, "subs/agy/opus");
    assert_eq!(opus.wire_model(), "claude-opus-5-5-medium");
}

#[test]
fn subs_cursor_models_resolve() {
    let models = example_config();
    let grok = resolve_model("subs/cursor/grok-4.7", &models).expect("cursor grok");
    assert_eq!(grok.cli_model, "grok-4.7-high");
    assert_eq!(grok.wire_model(), "grok-4.7-high");
    // The pre-rename alias still lands on the row.
    let alias = resolve_model("cursor-grok-4.7-high", &models).expect("alias");
    assert_eq!(alias.id, "subs/cursor/grok-4.7");
    let current = resolve_model("grok-4.7-high", &models).expect("current alias");
    assert_eq!(current.id, "subs/cursor/grok-4.7");
    // One Grok row. Its window is the 500k max-mode tier. #147 dropped the separate row.
    assert_eq!(grok.context_window, 500_000);
    let fable = resolve_model("subs/cursor/fable", &models).expect("cursor fable");
    assert_eq!(fable.cli_model, "claude-fable-5-1-thinking-high");
    let auto = resolve_model("subs/cursor/auto", &models).expect("cursor auto");
    assert_eq!(auto.cli_model, "auto");
    let composer = resolve_model("subs/cursor/composer-2.5", &models).expect("composer");
    assert_eq!(composer.cli_model, "composer-2.5");
    let composer_alias = resolve_model("composer", &models).expect("composer alias");
    assert_eq!(composer_alias.id, "subs/cursor/composer-2.5");
    assert!(models.find("subs/agy/gemini-3.1-pro").is_none());
    assert_eq!(
        models.models.iter().filter(|m| m.id.starts_with("subs/")).count(),
        14,
        "one row per family; codex Sol, Astra and Luna added 2026-10-25; K1 added subs/claude/sonnet, #147 dropped the catalogued Cursor 500k row, A4 dropped subs/claude/haiku, Composer 2.5 restored 2026-10-11, agy Argon and Opus added 2026-10-24"
    );
}

#[test]
fn agy_pool_account_pick() {
    let accounts = serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
        .expect("accounts");
    let store = thickapi_core::Store::open_in_memory().expect("store");
    let account = thickapi_core::router::pick_account(
        thickapi_core::BackendId::Agy,
        &accounts,
        &store,
        "sess-1",
    )
    .expect("pick");
    assert_eq!(account.id, "agy-main");
}

#[test]
fn cursor_pool_account_pick() {
    let accounts = serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
        .expect("accounts");
    let store = thickapi_core::Store::open_in_memory().expect("store");
    let account = thickapi_core::router::pick_account(
        thickapi_core::BackendId::Cursor,
        &accounts,
        &store,
        "sess-cursor",
    )
    .expect("pick");
    assert_eq!(account.id, "cursor-main");
}

#[test]
fn or_model_resolves_to_pi_backend() {
    let models = example_config();
    let resolved = resolve_model("or/anthropic/claude-opus-5", &models).expect("resolve");
    assert_eq!(resolved.backend, thickapi_core::BackendId::Pi);
    assert_eq!(resolved.cli_model, "anthropic/claude-opus-5");
}

#[test]
fn resolve_combo_expands_grok_then_or() {
    let combos =
        serde_yaml::from_str(include_str!("../../../configs/combos.yaml.example")).expect("combos");
    let models = example_config();
    let config = example_config_bundle();
    let config = thickapi_core::ConfigBundle {
        models,
        combos,
        ..config
    };
    let spec = TurnSpec {
        model: "subs/grok/grok-4.7".into(),
        prompt: "hi".into(),
        system_prompt: None,
        system_prompt_file: None,
        cwd: std::path::PathBuf::from("."),
        session_id: "sess".into(),
        thinking: thickapi_core::Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Off,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo: Some("grok-then-or".into()),
        allow_paid: false,
        timeout_seconds: 30,
        client: None,
        turn_id: None,
        metadata: serde_json::Map::new(),
    };
    let members = resolve_combo(&spec, &config).expect("combo");
    assert_eq!(members.len(), 2);
    assert_eq!(members[1], "or/anthropic/claude-opus-5");
}

#[test]
fn blocked_account_is_not_picked() {
    let accounts = serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
        .expect("accounts");
    let store = Store::open_in_memory().expect("store");
    store
        .upsert_account_health("claude-main", "rate_limited")
        .expect("health");
    let err = thickapi_core::router::pick_account(
        thickapi_core::BackendId::ClaudeCode,
        &accounts,
        &store,
        "sess-blocked",
    )
    .unwrap_err();
    assert!(err.to_string().contains("blocked"));
}

fn hours_ago(hours: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::hours(hours)).to_rfc3339()
}

#[test]
fn rate_limited_account_is_picked_again_once_the_cooldown_has_passed() {
    let accounts: thickapi_core::config::AccountsConfig =
        serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
            .expect("accounts");
    let store = Store::open_in_memory().expect("store");
    // Default `rate_limited` cooldown is 15 minutes; this row is an hour old.
    store
        .upsert_account_health_at("claude-main", "rate_limited", &hours_ago(1))
        .expect("health");
    let account = thickapi_core::router::pick_account(
        thickapi_core::BackendId::ClaudeCode,
        &accounts,
        &store,
        "sess-cooldown-over",
    )
    .expect("a lapsed rate limit must not brick the pool");
    assert_eq!(account.id, "claude-main");

    // A subscription block cools down over hours, not minutes: an hour is not enough.
    store
        .upsert_account_health_at("claude-main", "subscription_blocked", &hours_ago(1))
        .expect("health");
    let err = thickapi_core::router::pick_account(
        thickapi_core::BackendId::ClaudeCode,
        &accounts,
        &store,
        "sess-still-blocked",
    )
    .unwrap_err();
    assert!(err.to_string().contains("retry in"), "{err}");
}

#[test]
fn pool_health_ttl_override_and_zero_means_until_cleared() {
    let mut accounts: thickapi_core::config::AccountsConfig =
        serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
            .expect("accounts");
    accounts
        .pools
        .get_mut("claude")
        .expect("claude pool")
        .health_ttl_seconds
        .insert("rate_limited".into(), 0);
    let store = Store::open_in_memory().expect("store");
    store
        .upsert_account_health_at("claude-main", "rate_limited", &hours_ago(24 * 30))
        .expect("health");
    let err = thickapi_core::router::pick_account(
        thickapi_core::BackendId::ClaudeCode,
        &accounts,
        &store,
        "sess-manual",
    )
    .unwrap_err();
    assert!(err.to_string().contains("until cleared by hand"), "{err}");
}

#[test]
fn rotating_pool_steps_over_a_blocked_member() {
    let mut accounts: thickapi_core::config::AccountsConfig =
        serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
            .expect("accounts");
    let pool = accounts
        .pools
        .get_mut("openrouter")
        .expect("openrouter pool");
    let mut second = pool
        .accounts
        .first()
        .expect("an openrouter account")
        .clone();
    let first_id = second.id.clone();
    second.id = "or-second".into();
    pool.accounts.push(second);

    let store = Store::open_in_memory().expect("store");
    store
        .upsert_account_health(&first_id, "rate_limited")
        .expect("health");
    for n in 0..4 {
        let account = thickapi_core::router::pick_account(
            thickapi_core::BackendId::Pi,
            &accounts,
            &store,
            &format!("sess-rotate-{n}"),
        )
        .expect("the healthy member serves every turn");
        assert_eq!(account.id, "or-second");
    }
}

#[test]
fn openrouter_pool_sticky_reuses_account() {
    let accounts = serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
        .expect("accounts");
    let store = Store::open_in_memory().expect("store");
    let first = thickapi_core::router::pick_account(
        thickapi_core::BackendId::Pi,
        &accounts,
        &store,
        "sticky-or",
    )
    .expect("first");
    store
        .upsert_session_binding(&thickapi_core::adapters::SessionBinding {
            session_id: "sticky-or".into(),
            account_id: first.id.clone(),
            backend: thickapi_core::BackendId::Pi,
            cli_session: Some("pi-sess".into()),
            cwd: std::path::PathBuf::from("."),
        })
        .expect("bind");
    let second = thickapi_core::router::pick_account(
        thickapi_core::BackendId::Pi,
        &accounts,
        &store,
        "sticky-or",
    )
    .expect("second");
    assert_eq!(first.id, second.id);
}

#[test]
fn mixed_auth_pool_rotates_and_a_sticky_session_steps_over_a_block() {
    let mut accounts: thickapi_core::config::AccountsConfig =
        serde_yaml::from_str(include_str!("../../../configs/accounts.yaml.example"))
            .expect("accounts");
    let pool = accounts.pools.get_mut("claude").expect("claude pool");
    let mut env_acct = pool.accounts.first().expect("claude-main").clone();
    env_acct.id = "claude-env".into();
    env_acct.auth = thickapi_core::config::accounts::AuthKind::Env;
    env_acct.env_key = Some("CLAUDE_API_KEY".into());
    pool.accounts.push(env_acct);

    let store = Store::open_in_memory().expect("store");
    let a = thickapi_core::router::pick_account(
        thickapi_core::BackendId::ClaudeCode,
        &accounts,
        &store,
        "mixed-1",
    )
    .expect("first");
    let b = thickapi_core::router::pick_account(
        thickapi_core::BackendId::ClaudeCode,
        &accounts,
        &store,
        "mixed-2",
    )
    .expect("second");
    assert_ne!(
        a.id, b.id,
        "a two-account mixed-auth pool must rotate, not stick to the first"
    );

    store
        .upsert_session_binding(&thickapi_core::adapters::SessionBinding {
            session_id: "sticky-blocked".into(),
            account_id: "claude-main".into(),
            backend: thickapi_core::BackendId::ClaudeCode,
            cli_session: Some("cli-1".into()),
            cwd: std::path::PathBuf::from("."),
        })
        .expect("bind");
    store
        .upsert_account_health("claude-main", "rate_limited")
        .expect("health");
    let stepped = thickapi_core::router::pick_account(
        thickapi_core::BackendId::ClaudeCode,
        &accounts,
        &store,
        "sticky-blocked",
    )
    .expect("step over the blocked sticky account");
    assert_eq!(stepped.id, "claude-env");
}

#[test]
fn thick_resolve_accepts_unprefixed_catalogue_id() {
    let config = example_config_bundle();
    let store = Store::open_in_memory().expect("store");
    let spec = TurnSpec {
        model: "claude/fable".into(),
        prompt: "hi".into(),
        system_prompt: None,
        system_prompt_file: None,
        cwd: std::env::temp_dir(),
        session_id: "unprefixed-sess".into(),
        thinking: thickapi_core::Effort::Medium,
        tools: vec![],
        deny_writes: vec![],
        enforce_policy: None,
        mcp: thickapi_core::McpMode::Off,
        mcp_config: None,
        setting_sources: None,
        allow_api: false,
        combo: None,
        allow_paid: false,
        timeout_seconds: 30,
        client: None,
        turn_id: None,
        metadata: serde_json::Map::new(),
    };
    let resolved = resolve(&spec, &config, &store).expect("resolve");
    assert_eq!(resolved.backend, thickapi_core::BackendId::ClaudeCode);
    assert_eq!(resolved.model.id, "subs/claude/fable");
    assert_eq!(resolved.model.cli_model, "claude-fable-5-1");
}
