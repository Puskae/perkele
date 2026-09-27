//! Per-family SSE plumbing shared by all entities (grocery, calendar,
//! announcements). Handlers `poke` after any mutation; every subscribed
//! client of that family gets a tick and refetches whatever it shows.

use crate::AppState;
use crate::session::CurrentUser;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

/// Wake every SSE listener of `family_id`. A missing entry just means nobody
/// is listening right now — nothing to do.
pub(crate) async fn poke(state: &AppState, family_id: i64) {
    let map = state.sync_tx.lock().await;
    if let Some(tx) = map.get(&family_id) {
        let _ = tx.send(());
    }
}

/// SSE endpoint body: subscribe to the caller's family channel (created
/// lazily on first listen) and emit an "updated" event per poke.
pub(crate) async fn sse_events(
    user: CurrentUser,
    State(state): State<AppState>,
) -> impl axum::response::IntoResponse {
    let rx = {
        let mut map = state.sync_tx.lock().await;
        map.entry(user.family_id)
            .or_insert_with(|| broadcast::channel(16).0)
            .subscribe()
    };

    let stream = BroadcastStream::new(rx).filter_map(|r| {
        r.ok()
            .map(|_| Ok::<Event, std::convert::Infallible>(Event::default().data("updated")))
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
