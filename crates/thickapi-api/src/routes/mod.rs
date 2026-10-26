pub mod chat_completions;
pub mod health;
pub mod models;
pub mod ready;
pub mod turn;
pub mod turn_cancel;
pub mod turn_events;
pub mod turn_status;

use axum::response::sse::Event;

pub(crate) fn sse_json_event(id: u64, event: impl serde::Serialize) -> Event {
    match Event::default().id(id.to_string()).json_data(event) {
        Ok(ev) => ev,
        Err(err) => {
            tracing::error!(%err, "failed to serialize SSE event");
            Event::default()
        }
    }
}
