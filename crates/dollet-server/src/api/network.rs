//! The `network_access` gate for endpoints that carry no credentials.
//!
//! HDHR discovery, `/output/m3u` and `/output/epg` are unauthenticated by
//! necessity — Plex and every M3U client fetch them with nothing but a URL —
//! so the allowlist is the only control in front of them. They expose the
//! whole channel inventory and the stream URLs that play it.
//!
//! Same rule as `auth`: an address that cannot be determined is denied when
//! the endpoint is restricted, because allowing it would make the control
//! enforceable only against clients that volunteer a blocked address.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use dollet_core::settings::{self, NetworkAccess};

use super::auth::ClientAddr;
use super::error::ApiError;
use crate::AppState;

/// The endpoint class for playlist, guide and HDHR access.
pub const M3U_EPG: &str = "M3U_EPG";

/// The endpoint class for the stream endpoint itself.
pub const STREAMS: &str = "STREAMS";

/// The endpoint class for the Xtream Codes API.
pub const XC_API: &str = "XC_API";

/// Passes when the caller's address is allowed for [`M3U_EPG`].
pub struct NetworkGate;

impl FromRequestParts<AppState> for NetworkGate {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        check(state, parts, M3U_EPG).await?;
        Ok(Self)
    }
}

async fn check(
    state: &AppState,
    parts: &mut Parts,
    endpoint: &str,
) -> Result<(), dollet_core::Error> {
    let ClientAddr(client) = ClientAddr::from_request_parts(parts, state)
        .await
        .map_err(|_| dollet_core::Error::Forbidden)?;
    allowed(state, client, endpoint).await
}

/// The same decision for handlers that already resolved the address.
pub async fn allowed(
    state: &AppState,
    client: Option<std::net::IpAddr>,
    endpoint: &str,
) -> Result<(), dollet_core::Error> {
    let access = settings::load::<NetworkAccess>(&state.db).await?;
    match client {
        Some(ip) if access.allows(endpoint, ip) => Ok(()),
        Some(_) => Err(dollet_core::Error::Forbidden),
        None if !access.is_restricted(endpoint) => Ok(()),
        None => {
            tracing::warn!(
                endpoint,
                "denying a request with no determinable source address"
            );
            Err(dollet_core::Error::Forbidden)
        }
    }
}
