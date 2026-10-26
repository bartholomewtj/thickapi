use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    http::{HeaderMap, HeaderValue},
    response::Response,
};
use thickapi_core::config::models::Transport;
use thickapi_core::thin::model_check::vendor_of;
use thickapi_core::thin::openai::{self, ChatRequest};
use thickapi_core::thin::{ThinError, ThinEvent, ThinRequest};
use thickapi_core::thinking::normalise_effort;
use thickapi_core::types::BackendId;
use tokio::sync::mpsc;

use super::access_log::{self, RequestLog, RequestLogGuard};
use super::collect::collect_response;
use super::headers::{insert_path_headers, iso8601, path_reason_from};
use super::stream::{stream_response, StreamBounds, StreamGuards};
use super::validate::validate;
use super::{AbortOnDrop, Ctx, CHANNEL_DEPTH};
use crate::AppState;

/// `serve --dump-tools`: write the first request's `tools` array once, then clear the flag.
pub(super) fn dump_first_tools(state: &AppState, request: &ChatRequest) {
    let path = {
        let mut slot = state.dump_tools.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };
    let Some(path) = path else {
        return;
    };
    match serde_json::to_vec_pretty(&request.tools) {
        Ok(bytes) => {
            if let Err(err) = std::fs::write(&path, bytes) {
                tracing::warn!(error = %err, path = %path.display(), "dump-tools write failed");
            } else {
                tracing::info!(path = %path.display(), n = request.tools.len(), "dumped thin tools array");
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "dump-tools serialize failed");
        }
    }
}

/// Entry point: mints the one `request_id` this request will carry end to end (`x-request-id`
/// on every response, the access log line, the error envelope), records `t0`, and attaches the
/// id to whatever the body below returns — pre-`Started` rejections included, since those never
/// reach a `RequestLog` (SPEC A2 deliverables 2, 5). The body runs as an inline block, not a
/// second named function, so it does not add a second `Result<Response, ThinError>` return site
/// next to this one for clippy's `result_large_err` to flag (`ThinError` is already over its
/// threshold everywhere in this crate; the brief adds no new `#[allow]` for it).
pub(super) async fn handle(
    state: AppState,
    key: String,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ThinError> {
    let t0 = Instant::now();
    let request_id = openai::new_completion_id();
    let result: Result<Response, ThinError> = async {
        state.refuse_if_draining()?;
        let inflight = crate::ThinInFlightGuard::enter(&state.thin_in_flight);

        let request: ChatRequest = serde_json::from_slice(&body).map_err(|e| {
            ThinError::invalid_request(format!("malformed request body: {e}"), None)
        })?;

        dump_first_tools(&state, &request);

        let entry = state
            .config
            .models
            .find(&request.model)
            .filter(|row| row.thin_capable())
            .cloned()
            .ok_or_else(|| ThinError::model_not_found(&request.model))?;

        let mut ignored = validate(&request, &entry)?;
        let effort = normalise_effort(request.effort())?;
        if entry.backend == BackendId::ClaudeCode
            && entry.transport == Transport::OauthDirect
            && thickapi_core::thin::oauth::claude_oauth::force_thinking_off(
                &request, effort, &entry,
            )
        {
            ignored.push("thinking");
        }

        let adapter = state.thin.adapter_for(&entry).ok_or_else(|| {
            ThinError::not_implemented(format!(
                "no thin adapter for backend {:?} yet",
                entry.backend
            ))
        })?;

        let cursor_cwd = match headers.get("x-thickapi-cwd") {
            None => None,
            Some(value) => {
                let raw = value.to_str().map_err(|_| {
                    ThinError::invalid_spec("x-thickapi-cwd must be one absolute path that exists")
                })?;
                thickapi_core::thin::oauth::cursor::cursor_cwd_override(Some(raw))?
            }
        };
        let req = Arc::new(ThinRequest {
            request,
            entry,
            effort,
            request_id: request_id.clone(),
            path: "oauth",
            fallback: None,
            cursor_cwd,
        });

        let (tx, mut rx) = mpsc::channel::<ThinEvent>(CHANNEL_DEPTH);
        let runner_req = req.clone();
        let runner = AbortOnDrop::new(tokio::spawn(async move {
            let result = adapter.run(&runner_req, tx.clone()).await;
            if let Err(err) = result {
                // Only reaches the client when nothing was sent yet (see ModelAdapter contract).
                let _ = tx.send(ThinEvent::Error(err)).await;
            }
        }));

        // Wait for the vendor to accept the request before committing to a 200.
        let mut model_label = req.entry.id.clone();
        let mut advisory: Option<(f64, Option<String>)> = None;
        let mut early: Vec<ThinEvent> = Vec::new();
        let mut serving_path = req.path;
        let mut serving_fallback = req.fallback;
        let mut serving_reason: Option<&'static str> = None;
        let mut serving_probe: Option<String> = None;
        let mut t_started: Option<Instant> = None;
        let wire_params: Option<String>;
        if req.entry.backend == BackendId::ClaudeCode {
            let (path, why) = state.thin.claude().planned_path_for(req.entry.wire_model());
            serving_path = path;
            serving_reason = path_reason_from(&why);
            if serving_reason == Some("blocked") {
                serving_probe = state.thin.claude().state.probe_next_at().map(iso8601);
            }
        }
        // One function decides a row's vendor (C6). A `pi` row has no thin adapter and never
        // gets here; it keeps the name it always had.
        let vendor = vendor_of(req.entry.backend).unwrap_or("pi");
        let thin = &state.config.router.thin;
        let deadline = (thin.start_deadline_seconds > 0).then(|| {
            tokio::time::Instant::now() + Duration::from_secs(thin.start_deadline_seconds)
        });
        loop {
            let next = match deadline {
                Some(at) => match tokio::time::timeout_at(at, rx.recv()).await {
                    Ok(next) => next,
                    // Dropping `runner` on return aborts the adapter and closes the upstream.
                    Err(_) => {
                        let err = ThinError::vendor_down(
                            vendor,
                            format!(
                                "no response from {} in {}s",
                                vendor, thin.start_deadline_seconds
                            ),
                        )
                        .with_path(
                            serving_path,
                            serving_fallback,
                            serving_reason,
                        );
                        access_log::emit_pre_started(
                            &pre_started_log(
                                &request_id,
                                &key,
                                &req,
                                serving_path,
                                serving_fallback,
                                t0,
                                t_started,
                            ),
                            &err,
                        );
                        return Err(err);
                    }
                },
                None => rx.recv().await,
            };
            match next {
                Some(ThinEvent::Started {
                    model,
                    path,
                    fallback,
                    wire_params: params,
                }) => {
                    t_started = Some(Instant::now());
                    wire_params = params;
                    if !model.is_empty() {
                        model_label = model;
                    }
                    serving_path = path;
                    serving_fallback = fallback;
                    if fallback == Some("oauth->bridge") {
                        serving_reason = Some("blocked");
                        serving_probe = state.thin.claude().state.probe_next_at().map(iso8601);
                    } else if fallback == Some("bridge->oauth")
                        || (path == "oauth" && fallback.is_none())
                    {
                        serving_reason = None;
                        serving_probe = None;
                    }
                    break;
                }
                Some(ThinEvent::Error(err)) => {
                    let err = if err.path().is_some() {
                        err
                    } else {
                        err.with_path(serving_path, serving_fallback, serving_reason)
                    };
                    access_log::emit_pre_started(
                        &pre_started_log(
                            &request_id,
                            &key,
                            &req,
                            serving_path,
                            serving_fallback,
                            t0,
                            t_started,
                        ),
                        &err,
                    );
                    return Err(err);
                }
                Some(ThinEvent::RateLimitAdvisory {
                    utilization,
                    resets_at,
                }) => {
                    advisory = Some((utilization, resets_at));
                }
                Some(other) => early.push(other),
                None => {
                    let err = ThinError::vendor_down(vendor, "adapter ended without a response")
                        .with_path(serving_path, serving_fallback, serving_reason);
                    access_log::emit_pre_started(
                        &pre_started_log(
                            &request_id,
                            &key,
                            &req,
                            serving_path,
                            serving_fallback,
                            t0,
                            t_started,
                        ),
                        &err,
                    );
                    return Err(err);
                }
            }
        }
        // Pick up an advisory the adapter queued right behind `Started`.
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ThinEvent::RateLimitAdvisory {
                    utilization,
                    resets_at,
                } => advisory = Some((utilization, resets_at)),
                other => early.push(other),
            }
        }

        let mut headers = HeaderMap::new();
        if !ignored.is_empty() {
            if let Ok(v) = HeaderValue::from_str(&ignored.join(",")) {
                headers.insert("x-thickapi-ignored", v);
            }
        }
        if let Some((u, reset)) = &advisory {
            let value = match reset {
                Some(r) => format!("{u:.3};reset={r}"),
                None => format!("{u:.3}"),
            };
            if let Ok(v) = HeaderValue::from_str(&value) {
                headers.insert("x-thickapi-rate-limit", v);
            }
        }
        if let Ok(v) = HeaderValue::from_str(&req.request_id) {
            headers.insert("x-request-id", v);
        }
        insert_path_headers(
            &mut headers,
            serving_path,
            serving_fallback,
            serving_reason,
            serving_probe.as_deref(),
        );
        if req.entry.backend == BackendId::Cursor {
            headers.insert("x-thickapi-usage", HeaderValue::from_static("estimated"));
        }

        // One `thickapi_api::access` line owed for this request from here on (SPEC A2 deliverable
        // 3): `Started` has arrived, so every path below is "post-Started" for deliverable 4's
        // `warn` too. `RequestLogGuard` emits it exactly once, on a normal end or (dropped without
        // one) on a client disconnect.
        let log = RequestLogGuard::new(RequestLog {
            request_id: request_id.clone(),
            key,
            model: req.entry.id.clone(),
            wire_model: req.entry.wire_model().to_string(),
            wire_params,
            path: serving_path,
            fallback: serving_fallback,
            stream: req.request.stream,
            t0,
            ttfe_ms: access_log::ttfe_ms(t0, t_started),
        });

        let ctx = Ctx {
            id: req.request_id.clone(),
            created: openai::now_unix(),
            model: model_label,
            parallel: req.request.parallel_tool_calls(),
            include_usage: req.request.include_usage(),
        };

        let route_idle = thin.route_idle(req.entry.backend);
        if req.request.stream {
            let keepalive =
                (thin.keepalive_seconds > 0).then(|| Duration::from_secs(thin.keepalive_seconds));
            Ok(stream_response(
                ctx,
                headers,
                early,
                rx,
                StreamBounds {
                    keepalive,
                    idle: route_idle,
                },
                StreamGuards {
                    runner,
                    inflight: Some(inflight),
                    log,
                },
            ))
        } else {
            let _runner = runner;
            let _inflight = inflight;
            collect_response(ctx, headers, early, rx, route_idle, log).await
        }
    }
    .await;
    result.map_err(|err| err.with_request_id(request_id))
}

/// `RequestLog` for a request that ends inside the wait-for-`Started` loop (start-deadline, a
/// pre-`Started` adapter error, the adapter channel closing early) — it never gets a
/// `RequestLogGuard`, since that is only built once `Started` has arrived, so each such branch
/// logs directly through `access_log::emit_pre_started` (SPEC A2 deliverables 2, 3).
fn pre_started_log(
    request_id: &str,
    key: &str,
    req: &ThinRequest,
    path: &'static str,
    fallback: Option<&'static str>,
    t0: Instant,
    t_started: Option<Instant>,
) -> RequestLog {
    RequestLog {
        request_id: request_id.to_string(),
        key: key.to_string(),
        model: req.entry.id.clone(),
        wire_model: req.entry.wire_model().to_string(),
        wire_params: None,
        path,
        fallback,
        stream: req.request.stream,
        t0,
        ttfe_ms: access_log::ttfe_ms(t0, t_started),
    }
}
