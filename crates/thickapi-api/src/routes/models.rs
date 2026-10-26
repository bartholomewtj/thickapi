use std::collections::HashMap;

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use thickapi_core::config::models::ModelEntry;
use thickapi_core::thinking::{profile, reasoning_ui};

use crate::AppState;

#[derive(Serialize, serde::Deserialize)]
pub struct PiModelCost {
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    pub input: f64,
    pub output: f64,
}

impl PiModelCost {
    fn zero() -> Self {
        Self {
            cache_read: 0.0,
            cache_write: 0.0,
            input: 0.0,
            output: 0.0,
        }
    }
}

#[derive(Serialize, serde::Deserialize)]
pub struct PiDefaultOptions {
    pub thinking: String,
}

#[derive(Serialize, serde::Deserialize)]
pub struct PiModel {
    pub id: String,
    pub name: String,
    #[serde(rename = "contextWindow")]
    pub context_window: u64,
    pub reasoning: bool,
    pub cost: PiModelCost,
    #[serde(rename = "maxTokens")]
    pub max_tokens: u64,
    pub input: Vec<String>,
    #[serde(rename = "thinkingLevelMap", skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<HashMap<String, Option<String>>>,
    #[serde(rename = "defaultOptions", skip_serializing_if = "Option::is_none")]
    pub default_options: Option<PiDefaultOptions>,
}

/// Pi id for a catalogue row: `subs/claude/sonnet` → `claude/sonnet` (SPEC §19.6).
pub fn pi_model_id(id: &str) -> String {
    id.strip_prefix("subs/").unwrap_or(id).to_string()
}

/// Thin-shape Pi entry (SPEC §3.2): unprefixed id, `reasoning` from `reasoning_ui`.
pub fn thin_pi_model_from_entry(m: &ModelEntry) -> PiModel {
    pi_model(m, pi_model_id(&m.id), reasoning_ui(m))
}

fn pi_model(m: &ModelEntry, id: String, reasoning: bool) -> PiModel {
    let max_tokens = if m.max_tokens > 0 {
        m.max_tokens
    } else {
        64_000
    };
    PiModel {
        id,
        name: if m.name.is_empty() {
            m.id.clone()
        } else {
            m.name.clone()
        },
        context_window: m.context_window,
        reasoning,
        cost: PiModelCost::zero(),
        max_tokens,
        input: m.input.clone(),
        thinking_level_map: profile(m.backend).advertise(m),
        default_options: if reasoning {
            Some(PiDefaultOptions {
                thinking: "medium".to_string(),
            })
        } else {
            None
        },
    }
}

#[derive(Serialize, Deserialize)]
pub struct OpenAiModel {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
}

#[derive(Serialize, Deserialize)]
pub struct OpenAiModelList {
    pub object: &'static str,
    pub data: Vec<OpenAiModel>,
}

#[derive(Deserialize, Default)]
pub struct ModelsQuery {
    pub format: Option<String>,
}

fn served_rows(state: &AppState) -> impl Iterator<Item = &ModelEntry> {
    state
        .config
        .models
        .models
        .iter()
        .filter(|m| m.id.starts_with("subs/") && m.enabled)
}

/// Thin lists only advertise rows a thin adapter serves (`transport` set, `enabled`, a vendor
/// adapter wired); `cli_turn`-only rows are thick-turn rows and stay off `/v1/models`.
fn thin_rows(state: &AppState) -> impl Iterator<Item = &ModelEntry> {
    served_rows(state).filter(|m| state.thin.adapter_for(m).is_some())
}

pub fn openai_model_list(state: &AppState) -> OpenAiModelList {
    let created = state.started_unix;
    OpenAiModelList {
        object: "list",
        data: thin_rows(state)
            .map(|m| OpenAiModel {
                id: m.id.clone(),
                object: "model",
                created,
                owned_by: format!("{:?}", m.backend).to_ascii_lowercase(),
            })
            .collect(),
    }
}

/// `GET /v1/models` (SPEC §3.2, §19.6): OpenAI list shape by default; `?format=pi` returns the
/// thin Pi payload `sync-pi` copies into `providers.subs.models`. (`?format=pi-thick`, the
/// retired `extensions/pi` lane's prefixed list, went at T5.)
pub async fn list_models(
    State(state): State<AppState>,
    Query(query): Query<ModelsQuery>,
) -> Response {
    if let Err(err) = state.refuse_if_draining() {
        return crate::routes::chat_completions::thin_error_response(&err);
    }
    match query.format.as_deref() {
        Some("pi") => {
            let models: Vec<PiModel> = thin_rows(&state).map(thin_pi_model_from_entry).collect();
            Json(models).into_response()
        }
        _ => Json(openai_model_list(&state)).into_response(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use http_body_util::BodyExt;
    use thickapi_core::config::keys::{hash_token, KeyEntry, KeysConfig};
    use thickapi_core::config::models::{default_input, Transport};
    use thickapi_core::types::BackendId;
    use tower::ServiceExt;

    use super::*;
    use crate::middleware::auth::SCOPE_MODELS_READ;
    use crate::{build_router, AppState};

    fn entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            backend: BackendId::ClaudeCode,
            cli_model: "claude-sonnet-4-6".into(),
            context_window: 200_000,
            max_tokens: 32_000,
            thinking: true,
            cost: 0.0,
            name: "Claude Sonnet".into(),
            provider: None,
            transport: Transport::CliBridge,
            vendor_model: None,
            enabled: true,
            input: default_input(),
        }
    }

    #[test]
    fn pi_model_includes_thinking_map_for_claude() {
        let model = thin_pi_model_from_entry(&entry("subs/claude/sonnet"));
        assert!(model.reasoning);
        let map = model.thinking_level_map.expect("map");
        assert_eq!(map.get("off").and_then(|v| v.as_deref()), Some("off"));
        assert_ne!(map.get("off").and_then(|v| v.as_deref()), Some("none"));
        assert_eq!(map.get("medium").and_then(|v| v.as_deref()), Some("medium"));
        assert_eq!(
            model.default_options.as_ref().map(|o| o.thinking.as_str()),
            Some("medium")
        );
    }

    #[test]
    fn thin_pi_model_strips_prefix() {
        let mut e = entry("subs/grok/grok-4.7");
        e.backend = BackendId::Grok;
        e.thinking = false;
        let model = thin_pi_model_from_entry(&e);
        assert_eq!(model.id, "grok/grok-4.7");
        assert!(model.reasoning);
        let map = model.thinking_level_map.expect("map");
        assert!(map.get("max").expect("key").is_none());
        assert_eq!(model.input, vec!["text".to_string()]);
    }

    #[test]
    fn auto_advertises_no_reasoning() {
        let mut auto = entry("subs/cursor/auto");
        auto.backend = BackendId::Cursor;
        auto.cli_model = "auto".into();
        auto.vendor_model = Some("auto".into());
        let a = thin_pi_model_from_entry(&auto);
        assert!(!a.reasoning);
        assert!(a.thinking_level_map.is_none());
        assert!(a.default_options.is_none());
    }

    fn state_with_models(token: &str, models: thickapi_core::config::ModelsConfig) -> AppState {
        let hash = hash_token(token);
        let config = thickapi_core::ConfigBundle {
            config_dir: std::path::PathBuf::from("."),
            router: thickapi_core::config::RouterConfig {
                listen: "127.0.0.1:8787".into(),
                data_dir: std::env::temp_dir().join("thickapi-models-test"),
                ..Default::default()
            },
            accounts: thickapi_core::config::AccountsConfig {
                pools: Default::default(),
            },
            models,
            combos: thickapi_core::config::CombosConfig {
                combos: Default::default(),
            },
            keys: KeysConfig {
                keys: vec![KeyEntry {
                    id: "test".into(),
                    hash,
                    scopes: vec![SCOPE_MODELS_READ.into()],
                    ..Default::default()
                }],
            },
        };
        crate::server::make_state_with_store(
            config,
            Arc::new(thickapi_core::Store::open_in_memory().expect("store")),
        )
    }

    async fn get(
        app: axum::Router,
        uri: &str,
        token: &str,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .header("Authorization", format!("Bearer {}", token))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    #[tokio::test]
    async fn models_default_is_openai_list_shape() {
        let token = "ta_models";
        let models = serde_yaml::from_str(include_str!("../../../../configs/models.yaml.example"))
            .expect("models");
        let app = build_router(state_with_models(token, models));
        let (status, json) = get(app, "/v1/models", token).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(json["object"], "list");
        let data = json["data"].as_array().expect("data");
        assert!(data.iter().all(|m| m["object"] == "model"));
        assert!(data
            .iter()
            .all(|m| m["id"].as_str().unwrap().starts_with("subs/")));
        assert!(data.iter().any(|m| m["id"] == "subs/claude/fable"));
        assert!(data.iter().any(|m| m["id"] == "subs/cursor/auto"));
        assert!(
            data.iter().any(|m| m["id"] == "subs/claude/sonnet"),
            "the example serves subs/claude/sonnet (K1, Sonnet 5.5)"
        );
        assert!(
            data.iter().all(|m| m["id"] != "subs/claude/haiku"),
            "haiku is not catalogued (A4, SPEC 10.3): families nobody wants are not rows"
        );
    }

    #[tokio::test]
    async fn models_pi_format_unprefixed_and_hides_disabled() {
        let token = "ta_models";
        let mut disabled = entry("subs/cursor/auto");
        disabled.backend = BackendId::Cursor;
        disabled.enabled = false;
        // Thick-only row (`cli_turn`): a thick-turn row, off every thin list.
        let mut thick_only = entry("subs/grok/grok-4.7");
        thick_only.backend = BackendId::Grok;
        thick_only.transport = Transport::CliTurn;
        let models = thickapi_core::config::ModelsConfig {
            aliases: Default::default(),
            models: vec![entry("subs/claude/sonnet"), disabled, thick_only],
        };
        let app = build_router(state_with_models(token, models));
        let (status, unknown) = get(app.clone(), "/v1/models?format=pi-thick", token).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            unknown["object"], "list",
            "an unknown format falls back to the OpenAI list"
        );
        let (status, json) = get(app, "/v1/models?format=pi", token).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let list: Vec<PiModel> = serde_json::from_value(json).expect("pi models");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "claude/sonnet");
        assert!(list[0].thinking_level_map.is_some());
        assert_eq!(list[0].max_tokens, 32_000);
        assert!(list[0].cost.input == 0.0 && list[0].cost.output == 0.0);
    }
}
