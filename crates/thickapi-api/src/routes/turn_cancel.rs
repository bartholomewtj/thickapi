use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::routes::turn::{error_response, stored_result};
use crate::AppState;
use thickapi_core::TurnError;

#[derive(Serialize)]
struct CancelBody {
    state: String,
    turn_id: String,
}

pub async fn post_turn_cancel(
    State(state): State<AppState>,
    Path(turn_id): Path<String>,
) -> Response {
    match state.registry.cancel_token(&turn_id) {
        // Evicted from memory but finished on disk: still "already finished", not "never seen".
        None if stored_result(&state, &turn_id).is_some() => {
            error_response(TurnError::TurnAlreadyFinished(turn_id))
        }
        None => error_response(TurnError::TurnNotFound(turn_id)),
        Some(token) => {
            if let Some(status) = state.registry.status(&turn_id) {
                if matches!(status, thickapi_core::TurnStatus::Finished(_)) {
                    return error_response(TurnError::TurnAlreadyFinished(turn_id));
                }
            }
            token.cancel();
            (
                StatusCode::OK,
                Json(CancelBody {
                    state: "cancelling".into(),
                    turn_id,
                }),
            )
                .into_response()
        }
    }
}
