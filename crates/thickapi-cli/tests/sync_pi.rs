#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use thickapi_cli::commands::sync_pi::{build_subs_block, listen_base_url, merge_blocks};

/// D3 deliverable 5: `sync-pi` writes `http://127.0.0.1:<port>/v1` whenever `listen` is a
/// wildcard bind (`0.0.0.0`, `::`), because Pi on Windows cannot connect to `0.0.0.0`; a
/// loopback or named `listen` is untouched (SPEC §1.6, §19.10).
#[test]
fn sync_pi_rewrites_a_wildcard_listen_to_loopback() {
    assert_eq!(listen_base_url("0.0.0.0:18787"), "http://127.0.0.1:18787");
    assert_eq!(listen_base_url("127.0.0.1:8787"), "http://127.0.0.1:8787");
}

#[test]
fn sync_pi_merge_preserves_other_providers() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let before = root.join("pi-models-before.json");
    let expected = root.join("pi-models-after.json");

    let models = vec![
        serde_json::json!({
            "id": "claude/sonnet",
            "name": "Claude Sonnet (subscription)",
            "contextWindow": 200000,
            "maxTokens": 32000,
            "input": ["text"],
            "reasoning": true,
            "thinkingLevelMap": {
                "off": "off",
                "minimal": "minimal",
                "low": "low",
                "medium": "medium",
                "high": "high",
                "xhigh": "xhigh",
                "max": "max"
            },
            "defaultOptions": { "thinking": "medium" },
            "cost": {
                "cacheRead": 0,
                "cacheWrite": 0,
                "input": 0,
                "output": 0
            }
        }),
        serde_json::json!({
            "id": "grok/grok-4.7",
            "name": "Grok 4.7 (subscription)",
            "contextWindow": 500000,
            "maxTokens": 32000,
            "input": ["text"],
            "reasoning": true,
            "thinkingLevelMap": {
                "off": "off",
                "minimal": "minimal",
                "low": "low",
                "medium": "medium",
                "high": "high",
                "xhigh": "xhigh",
                "max": null
            },
            "defaultOptions": { "thinking": "medium" },
            "cost": {
                "cacheRead": 0,
                "cacheWrite": 0,
                "input": 0,
                "output": 0
            }
        }),
        serde_json::json!({
            "id": "agy/gemini-3.8-flash",
            "name": "Gemini 3.8 Flash (agy subscription)",
            "contextWindow": 1000000,
            "maxTokens": 65536,
            "input": ["text", "image"],
            "reasoning": true,
            "thinkingLevelMap": {
                "off": "off",
                "minimal": "minimal",
                "low": "low",
                "medium": "medium",
                "high": "high",
                "xhigh": "high",
                "max": "high"
            },
            "defaultOptions": { "thinking": "medium" },
            "cost": {
                "cacheRead": 0,
                "cacheWrite": 0,
                "input": 0,
                "output": 0
            }
        }),
        serde_json::json!({
            "id": "cursor/grok-4.7",
            "name": "Grok 4.7 (Cursor subscription)",
            "contextWindow": 500000,
            "maxTokens": 32000,
            "input": ["text"],
            "reasoning": true,
            "thinkingLevelMap": {
                "off": null,
                "minimal": null,
                "low": "low",
                "medium": "medium",
                "high": "high",
                "xhigh": "xhigh",
                "max": null
            },
            "defaultOptions": { "thinking": "medium" },
            "cost": {
                "cacheRead": 0,
                "cacheWrite": 0,
                "input": 0,
                "output": 0
            }
        }),
        serde_json::json!({
            "id": "cursor/auto",
            "name": "Auto (Cursor subscription)",
            "contextWindow": 128000,
            "maxTokens": 32000,
            "input": ["text", "image"],
            "reasoning": false,
            "cost": {
                "cacheRead": 0,
                "cacheWrite": 0,
                "input": 0,
                "output": 0
            }
        }),
    ];
    let subs = build_subs_block("http://127.0.0.1:8787", "ta_test_key", &models);

    let temp = tempfile::tempdir().expect("tempdir");
    let out = temp.path().join("models.json");
    std::fs::copy(&before, &out).expect("copy");
    merge_blocks(&out, &subs).expect("merge");

    let got: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).expect("read")).expect("json");
    let want: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&expected).expect("read")).expect("json");
    assert_eq!(got, want);
}
