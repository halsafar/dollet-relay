//! The stats WebSocket.
//!
//! Authenticated by **subprotocol**, not by query parameter: a browser cannot
//! set an `Authorization` header on a WebSocket, and a token in the URL lands
//! in `TraceLayer`'s log line and in every proxy access log in front of it.
//! The client offers `['auth.jwt', <access token>]`; the server validates the
//! second entry and echoes back `auth.jwt`, which a browser requires — a
//! handshake where the server selects none of the offered protocols fails.
//!
//! Frames are filtered per receiver rather than broadcast. `channel_stats`
//! carries channel UUIDs usable against the anonymous stream endpoint, the
//! upstream URLs behind them, and client IP addresses.

use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, State};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::get;
use dollet_core::auth::jwt;
use dollet_core::db;
use dollet_core::domain::{User, UserLevel};
use serde_json::{Value, json};

use super::auth::{ClientAddr, signer};
use super::error::ApiError;
use crate::AppState;

/// The subprotocol the server selects, and the marker the token follows.
const AUTH_PROTOCOL: &str = "auth.jwt";

/// How often a subscribed admin receives a stats frame. Fast enough that the
/// Stats page feels live, slow enough that it is not a load source itself.
const TICK: Duration = Duration::from_secs(2);

pub fn router() -> Router<AppState> {
    Router::new().route("/ws", get(upgrade))
}

/// The offered subprotocols, in the order the client listed them.
fn offered(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

/// The caller, authenticated from the offered subprotocols.
///
/// An extractor rather than the first few lines of the handler, and placed
/// *before* `WebSocketUpgrade` in the argument list, so a bad token is a 401
/// rather than whatever the upgrade machinery says about a request it was
/// never going to accept.
pub struct WsUser(pub User);

impl FromRequestParts<AppState> for WsUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let protocols = offered(&parts.headers);
        let token = protocols
            .iter()
            .position(|value| value == AUTH_PROTOCOL)
            .and_then(|index| protocols.get(index + 1))
            .ok_or(dollet_core::Error::Unauthorized)?;

        let claims = signer(state).await?.verify(token, jwt::ACCESS)?;
        let user = db::users::get(&state.db, claims.user_id)
            .await?
            .filter(|user| user.is_active)
            .ok_or(dollet_core::Error::Unauthorized)?;

        // The same `UI` allowlist every REST call is behind. This socket
        // carries channel UUIDs playable against the anonymous stream
        // endpoint, the upstream URLs behind them and client IP addresses —
        // an address the operator excluded from the UI must not receive them
        // because it arrived over a different protocol.
        let ClientAddr(client) = ClientAddr::from_request_parts(parts, state)
            .await
            .map_err(|_| dollet_core::Error::Forbidden)?;
        let user = super::auth::check_network(state, client, user).await?;

        Ok(Self(user))
    }
}

async fn upgrade(
    State(state): State<AppState>,
    WsUser(user): WsUser,
    ws: WebSocketUpgrade,
) -> Response {
    // Echoing the protocol back is not optional: a browser fails the handshake
    // if the server selects none of the ones it offered.
    ws.protocols([AUTH_PROTOCOL])
        .on_upgrade(move |socket| serve(socket, state, user))
}

async fn serve(mut socket: WebSocket, state: AppState, user: User) {
    let admin = user.user_level >= UserLevel::Admin;

    if socket
        .send(Message::Text(
            // `{type, data}` like every other frame: `web/src/ws/client.js`
            // hands listeners `envelope.data`, so a top-level `admin` would
            // arrive as `undefined`.
            json!({ "type": "hello", "data": { "admin": admin } })
                .to_string()
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    // A non-admin receives nothing beyond the greeting. The socket stays open
    // so the client's own reconnect logic does not spin, but every payload
    // this server emits today is admin-only.
    if !admin {
        while let Some(Ok(message)) = socket.recv().await {
            if matches!(message, Message::Close(_)) {
                return;
            }
        }
        return;
    }

    let mut ticker = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            },
            _ = ticker.tick() => {
                let stats = match super::stream::registry(&state).await {
                    Ok(registry) => registry.stats(),
                    Err(e) => {
                        tracing::warn!(error = %e, "stats unavailable");
                        return;
                    }
                };

                // The same payload `/proxy/stats/` returns, including what is
                // on each channel. A page that polls one and subscribes to the
                // other must not see two different shapes.
                let stats = match super::stream::with_now_playing(&state, stats).await {
                    Ok(stats) => stats,
                    Err(e) => {
                        tracing::warn!(error = %e, "now playing unavailable");
                        return;
                    }
                };

                let frame = json!({ "type": "channel_stats", "data": stats }).to_string();
                if socket.send(Message::Text(frame.into())).await.is_err() {
                    return;
                }

                // Job progress rides the same tick rather than a broadcast
                // channel: the table is a handful of rows, a refresh takes
                // minutes, and two-second granularity is finer than anything a
                // progress bar needs.
                // Through the same serializer `/api/core/jobs/` uses, not
                // the raw rows: that endpoint adds `running`, which is the
                // scheduler's own view rather than the row's and is what the
                // Sources page branches on. Raw rows would be a second shape
                // for one concept, missing the field its only consumer reads.
                let jobs: Vec<Value> = match db::jobs::list(&state.db).await {
                    Ok(jobs) => jobs.iter().map(super::core::serialize_job).collect(),
                    // An empty list reads as "no jobs", which is a different
                    // statement from "the query failed"; say nothing instead
                    // and let the next tick try again.
                    Err(e) => {
                        tracing::warn!(error = %e, "job list unavailable");
                        continue;
                    }
                };
                let frame = json!({ "type": "job_progress", "data": jobs }).to_string();
                if socket.send(Message::Text(frame.into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append("sec-websocket-protocol", value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn the_token_is_the_entry_after_the_marker() {
        let one_header = headers(&["auth.jwt, the-token"]);
        assert_eq!(offered(&one_header), vec!["auth.jwt", "the-token"]);

        // Browsers may send the list split across repeated headers.
        let split = headers(&["auth.jwt", "the-token"]);
        assert_eq!(offered(&split), vec!["auth.jwt", "the-token"]);
    }

    #[test]
    fn a_marker_with_nothing_after_it_carries_no_token() {
        let bare = offered(&headers(&["auth.jwt"]));
        let position = bare.iter().position(|v| v == AUTH_PROTOCOL);
        assert_eq!(position, Some(0));
        assert!(bare.get(1).is_none());
    }

    #[test]
    fn an_unrelated_protocol_list_carries_no_token() {
        let other = offered(&headers(&["chat, superchat"]));
        assert!(other.iter().position(|v| v == AUTH_PROTOCOL).is_none());
    }
}
