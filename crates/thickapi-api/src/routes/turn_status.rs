use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thickapi_core::TurnError;

use crate::routes::turn::{error_response, stored_result};
use crate::AppState;

#[derive(Serialize)]
struct RunningStatus {
    state: String,
    turn_id: String,
}

pub async fn get_turn_status(
    State(state): State<AppState>,
    Path(turn_id): Path<String>,
) -> Response {
    if let Some(status) = state.registry.status(&turn_id) {
        return match status {
            thickapi_core::TurnStatus::Finished(result) => {
                (StatusCode::OK, Json(result)).into_response()
            }
            thickapi_core::TurnStatus::Running | thickapi_core::TurnStatus::RunningWithState(_) => {
                (
                    StatusCode::OK,
                    Json(RunningStatus {
                        state: "running".into(),
                        turn_id,
                    }),
                )
                    .into_response()
            }
        };
    }

    if let Some(result) = stored_result(&state, &turn_id) {
        return (StatusCode::OK, Json(result)).into_response();
    }

    error_response(TurnError::TurnNotFound(turn_id))
}
