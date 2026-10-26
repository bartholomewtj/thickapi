use thiserror::Error;

use crate::types::PermissionBreach;

#[derive(Debug, Error)]
pub enum TurnError {
    #[error("refused_workspace: {0}")]
    RefusedWorkspace(String),
    #[error("unknown_model: {0}")]
    UnknownModel(String),
    #[error("spawn_failed: {0}")]
    SpawnFailed(String),
    #[error("turn_timeout: {0}")]
    TurnTimeout(String),
    #[error("invalid_spec: {0}")]
    InvalidSpec(String),
    #[error("permission_breach: {0:?}")]
    PermissionBreach(Vec<PermissionBreach>),
    #[error("turn_not_found: {0}")]
    TurnNotFound(String),
    #[error("turn_already_finished: {0}")]
    TurnAlreadyFinished(String),
    #[error("turn_in_progress: {0}")]
    TurnInProgress(String),
    /// A re-`POST` of a spent `turn_id` with a different prompt. The first persist wins.
    #[error("turn_conflict: {0}")]
    TurnConflict(String),
    #[error("queue_timeout: {0}")]
    QueueTimeout(String),
    #[error("rate_limited: {0}")]
    RateLimited(String),
    /// The vendor has refused the subscription itself; waiting out a rate window will not help.
    #[error("subscription_blocked: {0}")]
    SubscriptionBlocked(String),
    #[error("draining: {0}")]
    Draining(String),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for TurnError {
    /// A `TurnError` that travelled through `anyhow` comes back out as itself, so its code and
    /// HTTP status survive the trip; anything else is internal.
    fn from(err: anyhow::Error) -> Self {
        match err.downcast::<TurnError>() {
            Ok(typed) => typed,
            Err(other) => TurnError::Other(other.to_string()),
        }
    }
}

impl TurnError {
    pub fn code(&self) -> &'static str {
        match self {
            TurnError::RefusedWorkspace(_) => "refused_workspace",
            TurnError::UnknownModel(_) => "unknown_model",
            TurnError::SpawnFailed(_) => "spawn_failed",
            TurnError::TurnTimeout(_) => "turn_timeout",
            TurnError::InvalidSpec(_) => "invalid_spec",
            TurnError::PermissionBreach(_) => "permission_breach",
            TurnError::TurnNotFound(_) => "turn_not_found",
            TurnError::TurnAlreadyFinished(_) => "turn_already_finished",
            TurnError::TurnInProgress(_) => "turn_in_progress",
            TurnError::TurnConflict(_) => "turn_conflict",
            TurnError::QueueTimeout(_) => "queue_timeout",
            TurnError::RateLimited(_) => "rate_limited",
            TurnError::SubscriptionBlocked(_) => "subscription_blocked",
            TurnError::Draining(_) => "draining",
            TurnError::Other(_) => "internal_error",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            TurnError::RefusedWorkspace(_)
            | TurnError::UnknownModel(_)
            | TurnError::InvalidSpec(_) => 400,
            TurnError::PermissionBreach(_) => 422,
            TurnError::TurnNotFound(_) => 404,
            TurnError::TurnAlreadyFinished(_)
            | TurnError::TurnInProgress(_)
            | TurnError::TurnConflict(_) => 409,
            TurnError::RateLimited(_) => 429,
            TurnError::SubscriptionBlocked(_) => 402,
            TurnError::Draining(_) => 503,
            TurnError::SpawnFailed(_) => 502,
            TurnError::TurnTimeout(_) | TurnError::QueueTimeout(_) => 504,
            TurnError::Other(_) => 500,
        }
    }

    pub fn retry_after_seconds(&self) -> Option<u64> {
        match self {
            TurnError::RateLimited(_) => Some(60),
            _ => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_error_survives_a_trip_through_anyhow() {
        let wrapped: anyhow::Error = TurnError::UnknownModel("nope".into()).into();
        let back = TurnError::from(wrapped);
        assert_eq!(back.code(), "unknown_model");
        assert_eq!(back.http_status(), 400);

        let plain = TurnError::from(anyhow::anyhow!("disk on fire"));
        assert_eq!(plain.code(), "internal_error");
        assert_eq!(plain.http_status(), 500);
    }
}
