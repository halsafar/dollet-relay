//! Login, token refresh, API keys, and the extractors every other handler uses.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::request::Parts;
use axum::routing::{get, post};
use axum::{Json, Router};
use dollet_core::auth::{API_KEY_HEADER, API_KEY_SCHEME, Jwt, jwt, password};
use dollet_core::config::TrustedProxies;
use dollet_core::domain::{User, UserLevel};
use dollet_core::settings::{self, NetworkAccess, cidr};
use dollet_core::{Error, db};
use serde::Deserialize;
use serde_json::{Value, json};

use super::error::{ApiError, ApiResult};
use crate::AppState;

/// Endpoint class used for `network_access` allowlists.
const UI: &str = "UI";

/// Read the instance signing key per request.
///
/// Read per request rather than cached. The cost is one indexed read of a
/// single-row table, far cheaper than the PBKDF2 a login does anyway, and it
/// means a key rotated in the database takes effect without a restart.
pub async fn signer(state: &AppState) -> Result<Jwt, Error> {
    Jwt::load_or_create(&state.db).await
}

/// An authenticated caller. Any user level; handlers that need more say so by
/// extracting [`AdminUser`] instead.
pub struct CurrentUser(pub User);

/// A caller at admin level, which is what most of this API requires.
pub struct AdminUser(pub User);

/// Whether a peer may be believed about `X-Forwarded-For`.
///
/// This is the whole difference between an allowlist and a suggestion: any
/// client can send any forwarding header, so the value is only evidence when
/// the connection itself came from a proxy the operator nominated.
fn is_trusted_proxy(peer: IpAddr, policy: &TrustedProxies) -> bool {
    match policy {
        TrustedProxies::None => false,
        TrustedProxies::PrivateAndLoopback => cidr::is_loopback_or_private(peer),
        // Parsed at startup, so a request that carries a forwarding header
        // does not re-parse the whole list to decide whether to read it.
        TrustedProxies::Cidrs(list) => list.iter().any(|cidr| cidr.contains(peer)),
    }
}

/// The address the request actually came from.
///
/// The peer address is the only thing a client cannot forge. Forwarding
/// headers are consulted *only* when the peer is a trusted proxy, and then
/// from the right-hand end: everything to the right of the first untrusted hop
/// was written by infrastructure, everything to its left was written by the
/// client and is worth nothing.
///
/// When every hop *is* trusted the chain was written end to end by the
/// operator's own proxies, and the leftmost entry is the one that is a client
/// rather than a proxy. Falling back to the peer there would report the
/// innermost proxy as the client, which is one address for everybody.
///
/// `X-Real-IP` is a second opinion, not a tiebreaker: it is read only when no
/// `X-Forwarded-For` arrived at all. A proxy that sets both sets them to the
/// same thing, so a request where they disagree is one where a client picked
/// the header its chain does not use.
pub fn resolve_client_ip(
    peer: Option<IpAddr>,
    parts: &Parts,
    policy: &TrustedProxies,
) -> Option<IpAddr> {
    let peer = peer?;
    if !is_trusted_proxy(peer, policy) {
        return Some(peer);
    }

    if parts.headers.contains_key("x-forwarded-for") {
        let forwarded: Vec<IpAddr> = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(|entry| entry.trim().parse::<IpAddr>().ok())
            .collect();

        return forwarded
            .iter()
            .rev()
            .find(|ip| !is_trusted_proxy(**ip, policy))
            .or_else(|| forwarded.first())
            .copied()
            .or(Some(peer));
    }

    // A single-hop proxy that only sets X-Real-IP.
    parts
        .headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.trim().parse().ok())
        .or(Some(peer))
}

/// Whether the connection itself came from a nominated proxy, which is the
/// only condition under which any forwarding header means anything.
pub fn peer_is_trusted(parts: &Parts, policy: &TrustedProxies) -> bool {
    peer_of(parts).is_some_and(|peer| is_trusted_proxy(peer, policy))
}

fn peer_of(parts: &Parts) -> Option<IpAddr> {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
}

/// The client address, already resolved against the trusted-proxy policy.
pub struct ClientAddr(pub Option<IpAddr>);

impl FromRequestParts<AppState> for ClientAddr {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(resolve_client_ip(
            peer_of(parts),
            parts,
            &state.config.trusted_proxies,
        )))
    }
}

async fn authenticate(state: &AppState, parts: &Parts) -> Result<User, Error> {
    let client = resolve_client_ip(peer_of(parts), parts, &state.config.trusted_proxies);
    let headers = &parts.headers;

    if let Some(key) = api_key_from(headers) {
        if let Some(user) = db::users::by_api_key(&state.db, &key).await? {
            return check_network(state, client, user).await;
        }
        return Err(Error::Unauthorized);
    }

    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(Error::Unauthorized)?;

    let claims = signer(state).await?.verify(token.trim(), jwt::ACCESS)?;
    let user = db::users::get(&state.db, claims.user_id)
        .await?
        .filter(|user| user.is_active)
        .ok_or(Error::Unauthorized)?;

    check_network(state, client, user).await
}

/// An unknown address against a restricted endpoint **denies**.
///
/// The alternative — allow when the address cannot be determined — would make
/// this allowlist enforceable only against clients that volunteer a blocked
/// address, which is to say not enforceable at all.
pub async fn check_network(
    state: &AppState,
    client: Option<IpAddr>,
    user: User,
) -> Result<User, Error> {
    let access = settings::load::<NetworkAccess>(&state.db).await?;

    match client {
        Some(ip) if access.allows(UI, ip) => Ok(user),
        Some(_) => Err(Error::Forbidden),
        None if !access.is_restricted(UI) => Ok(user),
        None => {
            tracing::warn!(
                "denying a request with no determinable source address while `network_access` \
                 restricts the UI"
            );
            Err(Error::Forbidden)
        }
    }
}

fn api_key_from(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(key) = headers.get(API_KEY_HEADER).and_then(|v| v.to_str().ok()) {
        return Some(key.trim().to_owned());
    }

    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())?;
    let (scheme, key) = authorization.split_once(' ')?;
    (scheme.eq_ignore_ascii_case(API_KEY_SCHEME)).then(|| key.trim().to_owned())
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(authenticate(state, parts).await?))
    }
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = authenticate(state, parts).await?;
        if user.user_level < UserLevel::Admin {
            return Err(Error::Forbidden.into());
        }
        Ok(Self(user))
    }
}

#[derive(Deserialize)]
pub struct Credentials {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct RefreshRequest {
    refresh: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/token/", post(token))
        .route("/token/refresh/", post(refresh))
        .route("/auth/login/", post(token))
        .route("/auth/logout/", post(logout))
        .route("/initialize-superuser/", get(setup_status).post(bootstrap))
        .route("/api-keys/", get(show_api_key))
        .route("/api-keys/generate/", post(generate_api_key))
        .route("/api-keys/revoke/", post(revoke_api_key))
}

async fn token(
    State(state): State<AppState>,
    client: ClientAddr,
    Json(credentials): Json<Credentials>,
) -> ApiResult<Json<Value>> {
    throttle::check(client.0)?;

    let user = db::users::by_username(&state.db, &credentials.username).await?;

    // Exactly one PBKDF2 run on both paths. Generating a dummy hash here
    // instead would cost a second run, making a missing account measurably
    // slower than a wrong password — the enumeration oracle inverted.
    let stored = user
        .as_ref()
        .map(|u| u.password_hash.as_str())
        .unwrap_or(password::UNUSABLE);
    let matched = password::verify_offthread(&credentials.password, stored)
        .await
        .unwrap_or(false);

    let Some(user) = user.filter(|u| u.is_active && matched) else {
        throttle::record_failure(client.0);
        return Err(Error::Unauthorized.into());
    };

    check_network(&state, client.0, user.clone()).await?;
    throttle::clear(client.0);
    db::users::touch_last_login(&state.db, user.id).await?;

    let pair = signer(&state).await?.issue_pair(user.id)?;
    Ok(Json(
        json!({ "access": pair.access, "refresh": pair.refresh }),
    ))
}

/// Failed-login throttling, per source address.
///
/// Only failures count and a success clears the record, so a busy household
/// never trips it. Process-global rather than part of `AppState` because there
/// is exactly one process and the limit is about the machine, not a request.
mod throttle {
    use std::net::IpAddr;
    use std::sync::LazyLock;
    use std::time::{Duration, Instant};

    use dashmap::DashMap;

    use crate::api::error::ApiError;

    const MAX_FAILURES: u32 = 10;
    const WINDOW: Duration = Duration::from_secs(300);

    static FAILURES: LazyLock<DashMap<IpAddr, (u32, Instant)>> = LazyLock::new(DashMap::new);

    pub fn check(client: Option<IpAddr>) -> Result<(), ApiError> {
        let Some(ip) = client else { return Ok(()) };

        if let Some(entry) = FAILURES.get(&ip)
            && entry.1.elapsed() < WINDOW
            && entry.0 >= MAX_FAILURES
        {
            return Err(ApiError::too_many_requests(
                "too many failed sign-in attempts; try again later",
            ));
        }
        Ok(())
    }

    pub fn record_failure(client: Option<IpAddr>) {
        let Some(ip) = client else { return };

        let mut entry = FAILURES.entry(ip).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }

    pub fn clear(client: Option<IpAddr>) {
        if let Some(ip) = client {
            FAILURES.remove(&ip);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A documentation address, so this cannot collide with the loopback
        /// peer the integration tests sign in from.
        fn ip() -> Option<IpAddr> {
            Some("198.51.100.77".parse().unwrap())
        }

        #[test]
        fn only_failures_count_and_a_success_clears_them() {
            assert!(check(ip()).is_ok());

            for _ in 0..MAX_FAILURES - 1 {
                record_failure(ip());
            }
            assert!(check(ip()).is_ok(), "tripped below the limit");

            record_failure(ip());
            assert!(check(ip()).is_err(), "did not trip at the limit");

            clear(ip());
            assert!(check(ip()).is_ok(), "a success did not clear the record");
        }

        #[test]
        fn a_record_older_than_the_window_neither_blocks_nor_accumulates() {
            // Both halves of the expiry, which are separate code paths and
            // have to agree: `check` must ignore a stale record, and the next
            // failure must start a fresh count rather than adding to it.
            // Getting the second wrong means a household that fails once a
            // week is locked out on the tenth week.
            let ip: IpAddr = "198.51.100.79".parse().unwrap();
            let stale = Instant::now()
                .checked_sub(WINDOW + Duration::from_secs(1))
                .expect("a monotonic clock older than the throttle window");
            FAILURES.insert(ip, (MAX_FAILURES, stale));

            assert!(check(Some(ip)).is_ok(), "a stale record still blocked");

            record_failure(Some(ip));
            assert_eq!(
                FAILURES.get(&ip).unwrap().0,
                1,
                "the stale count was added to rather than reset"
            );
        }

        #[test]
        fn an_unknown_address_is_never_throttled() {
            // Otherwise a deployment with no connect info locks everyone out
            // of logging in at all, which is worse than an unthrottled login.
            for _ in 0..MAX_FAILURES * 2 {
                record_failure(None);
            }
            assert!(check(None).is_ok());
        }
    }
}

async fn refresh(
    State(state): State<AppState>,
    Json(request): Json<RefreshRequest>,
) -> ApiResult<Json<Value>> {
    let access = signer(&state).await?.refresh(&request.refresh)?;
    Ok(Json(json!({ "access": access })))
}

/// Tokens are stateless, so logout only records the event; the client drops
/// its tokens. A blacklist would need shared state for a 30-minute window.
async fn logout(State(state): State<AppState>, user: CurrentUser) -> ApiResult<Json<Value>> {
    db::events::record(
        &state.db,
        "logout",
        None,
        None,
        &json!({ "user": user.0.username }),
    )
    .await?;
    Ok(Json(json!({ "message": "Logout successful" })))
}

/// Whether the instance still needs its first admin. Unauthenticated by
/// necessity: there is nobody to authenticate as yet.
async fn setup_status(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "superuser_exists": db::users::any_admin_exists(&state.db).await? }),
    ))
}

/// Create the instance's first admin, once.
///
/// Through `create_first_admin`, which counts and inserts in one transaction
/// and counts admins regardless of `is_active`: a deactivated-only admin still
/// owns the instance, and two requests arriving together must not both pass a
/// check made outside the insert.
async fn bootstrap(
    State(state): State<AppState>,
    Json(credentials): Json<Credentials>,
) -> ApiResult<Json<Value>> {
    // Cheap check first, atomic check second. Hashing before asking would have
    // this unauthenticated, unthrottled endpoint spend 1.2M PBKDF2 iterations
    // per call for the rest of the instance's life, however many arrived.
    if db::users::any_admin_exists(&state.db).await? {
        return Err(Error::Forbidden.into());
    }

    let created = db::users::create_first_admin(
        &state.db,
        &User {
            id: 0,
            username: credentials.username,
            email: None,
            password_hash: password::hash_offthread(&credentials.password).await,
            is_active: true,
            user_level: UserLevel::Admin,
            api_key: None,
            stream_limit: 0,
            channel_profile_ids: Vec::new(),
            custom_properties: json!({}),
        },
    )
    .await?;

    // `None` means someone else is already the admin. Forbidden rather than a
    // conflict: the caller is being told they may not do this, not that they
    // should retry.
    let user = created.ok_or(Error::Forbidden)?;

    let pair = signer(&state).await?.issue_pair(user.id)?;
    Ok(Json(
        json!({ "access": pair.access, "refresh": pair.refresh }),
    ))
}

async fn show_api_key(user: CurrentUser) -> Json<Value> {
    Json(json!({ "key": user.0.api_key }))
}

#[derive(Deserialize, Default)]
struct KeyTarget {
    user_id: Option<i64>,
}

/// Admins may mint a key for anyone; everyone else only for themselves.
async fn resolve_target(
    state: &AppState,
    caller: User,
    target: Option<i64>,
) -> Result<User, Error> {
    match target {
        None => Ok(caller),
        Some(id) if id == caller.id => Ok(caller),
        Some(_) if caller.user_level < UserLevel::Admin => Err(Error::Forbidden),
        Some(id) => db::users::get(&state.db, id).await?.ok_or(Error::NotFound),
    }
}

async fn generate_api_key(
    State(state): State<AppState>,
    caller: CurrentUser,
    body: Option<Json<KeyTarget>>,
) -> ApiResult<Json<Value>> {
    let target = body.unwrap_or_default().user_id;
    let mut user = resolve_target(&state, caller.0, target).await?;

    let key = dollet_core::auth::new_api_key();
    user.api_key = Some(key.clone());
    let user = db::users::save(&state.db, &user).await?;

    // Returned alongside the user for convenience; the key is stored in the
    // clear, so `GET /api-keys/` can show it again.
    Ok(Json(
        json!({ "key": key, "user": super::users::serialize(&user) }),
    ))
}

async fn revoke_api_key(
    State(state): State<AppState>,
    caller: CurrentUser,
    body: Option<Json<KeyTarget>>,
) -> ApiResult<Json<Value>> {
    let target = body.unwrap_or_default().user_id;
    let mut user = resolve_target(&state, caller.0, target).await?;

    user.api_key = None;
    db::users::save(&state.db, &user).await?;
    Ok(Json(json!({ "success": true })))
}
