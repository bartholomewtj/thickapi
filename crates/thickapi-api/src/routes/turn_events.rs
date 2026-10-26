use std::convert::Infallible;

use axum::{
    extract::{Path, Query, State},
    response::sse::{Event, KeepAlive, Sse},
};
use futures_core::Stream;
use serde::Deserialize;

use crate::AppState;

#[derive(Deserialize)]
pub struct EventsQuery {
    pub since: Option<u64>,
}

pub async fn get_turn_events(
    State(state): State<AppState>,
    Path(turn_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let since = query.since.unwrap_or(0);
    let stream = async_stream::stream! {
        if let Some(events) = state.registry.events_since(&turn_id, since) {
            for stored in events {
                yield Ok(crate::routes::sse_json_event(stored.seq, stored.event));
            }
        } else {
            let dir = thickapi_core::turn_artifact_dir(
                &state.config.router.data_dir,
                &turn_id,
            );
            if let Ok(events) = thickapi_core::read_events_jsonl(&dir) {
                for stored in events.into_iter().filter(|e| e.seq > since) {
                    yield Ok(crate::routes::sse_json_event(stored.seq, stored.event));
                }
            }
        }

        if let Some(mut rx) = state.registry.subscribe(&turn_id) {
            let mut seq = state.registry.last_seq(&turn_id).unwrap_or(since);
            while let Ok(event) = rx.recv().await {
                seq += 1;
                let is_end = matches!(&event, thickapi_core::TurnEvent::TurnEnd { .. });
                yield Ok(crate::routes::sse_json_event(seq, event));
                if is_end {
                    break;
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
