//! Integration tests that drive the real `Router`.
//!
//! No sockets and no ports: `tower::ServiceExt::oneshot` feeds a request
//! straight into the service, so these run fully in parallel. Each gets its own
//! SQLite file seeded from `fixtures/sample.sql` (49 channels, 61 streams,
//! 42 logos).

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use dollet_core::config::{Config, TrustedProxies};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::AppState;

/// Matches the hash committed in the fixture.
const PASSWORD: &str = "ipx-test-password";
const USERNAME: &str = "fixtureadmin";
const API_KEY: &str = "fixture-api-key-0000000000000000000000";

/// The rows of any `/api/` list response.
///
/// Every one of them is `{results, count, page, pages}`, so a test never
/// has to know whether the endpoint it called paginates.
fn rows(body: &Value) -> &Vec<Value> {
    body["results"]
        .as_array()
        .unwrap_or_else(|| panic!("not a list response: {body}"))
}

const FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/sample.sql"
));

/// The second seed: a hand-written instance carrying every shape the product
/// supports, rather than the shapes one instance happens to have.
///
/// `fixtures/synthetic/README.md` says why each row group exists.
const SYNTHETIC: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/synthetic/instance.sql"
));

/// Who a request is made as. The synthetic seed defines one user per level,
/// plus the deactivated admin and an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Principal {
    Anonymous,
    Streamer,
    Standard,
    Admin,
    /// Deactivated. Both of its credentials exist and neither may work.
    InactiveAdmin,
    /// The standard user's API key rather than their bearer token: a second
    /// credential for the same person, which must reach exactly as far.
    StandardApiKey,
}

impl Principal {
    /// Every principal the authorization matrix runs, in increasing privilege
    /// so a failure reads top to bottom.
    const ALL: [Self; 6] = [
        Self::Anonymous,
        Self::InactiveAdmin,
        Self::Streamer,
        Self::Standard,
        Self::StandardApiKey,
        Self::Admin,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Streamer => "streamer",
            Self::Standard => "standard",
            Self::Admin => "admin",
            Self::InactiveAdmin => "inactive admin",
            Self::StandardApiKey => "standard's API key",
        }
    }
}

/// Every credential the synthetic seed commits, in one place, so a test never
/// carries a second copy of a password that has to be kept in step with the
/// hash beside it.
mod synthetic {
    pub const ADMIN: (&str, &str) = ("synthadmin", "dollet-test-admin");
    pub const STANDARD: (&str, &str) = ("synthstandard", "dollet-test-standard");
    pub const STREAMER: (&str, &str) = ("synthstreamer", "dollet-test-streamer");
    pub const INACTIVE_ADMIN: (&str, &str) = ("synthinactive", "dollet-test-inactive");

    pub const ADMIN_API_KEY: &str = "synthetic-admin-api-key-00000000000000";
    pub const STANDARD_API_KEY: &str = "synthetic-standard-api-key-0000000000";
    pub const INACTIVE_API_KEY: &str = "synthetic-inactive-api-key-0000000000";

    /// Channel 1000, whose failover list spans both provider accounts.
    pub const CHANNEL_UUID: &str = "bbbbbbbb-0000-4000-8000-000000001000";
    /// The channel with no streams at all, which is how a test reaches the
    /// stream endpoint without opening a session against a provider that is
    /// not there.
    pub const STREAMLESS_UUID: &str = "bbbbbbbb-0000-4000-8000-000000001014";
    /// The channel on the locked `redirect` stream profile, which takes this
    /// server out of the data path entirely.
    pub const REDIRECT_UUID: &str = "bbbbbbbb-0000-4000-8000-000000001013";
    /// The channel flagged `hidden_from_output`.
    pub const HIDDEN_UUID: &str = "bbbbbbbb-0000-4000-8000-000000001007";
    /// The channel whose only stream has no URL.
    pub const NULL_URL_UUID: &str = "bbbbbbbb-0000-4000-8000-000000001016";

    /// `custom_properties.xc_password`, which is what the Xtream API compares
    /// against — never the login password, because an Xtream URL carries the
    /// credential in its query string and that lands in every proxy log.
    pub const ADMIN_XC_PASSWORD: &str = "synth-xc-admin";
    pub const STANDARD_XC_PASSWORD: &str = "synth-xc-standard";
}

/// How a principal presents itself. Kept as a value rather than applied at the
/// call site so the matrix can loop over principals without branching on which
/// header each one needs.
#[derive(Debug, Clone)]
enum Credential {
    None,
    Bearer(String),
    ApiKey(String),
}

impl Credential {
    fn apply(&self, builder: axum::http::request::Builder) -> axum::http::request::Builder {
        match self {
            Self::None => builder,
            Self::Bearer(token) => builder.header("authorization", format!("Bearer {token}")),
            Self::ApiKey(key) => builder.header(dollet_core::auth::API_KEY_HEADER, key),
        }
    }
}

/// Every request is stamped with a peer address, because `axum::serve` does
/// the same through `into_make_service_with_connect_info` and the perimeter
/// check denies a request whose source it cannot determine.
///
/// **A test that needs failed sign-ins in bulk uses its own address.** The
/// login throttle counts failures per address in a process-global map, so ten
/// of them from here would start answering 429 to every other test in this
/// binary that signs in.
const PEER: &str = "127.0.0.1:40000";

/// An `AppState` with no data behind it, for unit tests that only need the
/// configuration — path building, mostly.
pub fn bare_state() -> AppState {
    AppState {
        db: sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap(),
        config: Arc::new(crate::test_support::config(
            std::path::Path::new("/data"),
            TrustedProxies::None,
        )),
    }
}

struct TestApp {
    router: Router,
    state: AppState,
    _dir: TempDir,
}

/// The same shape `main.rs` serves, fallback included: the bare three-segment
/// Xtream route and the SPA catch-all only conflict once they are in the same
/// router, and `/health` is a route the authorization matrix has to answer for
/// like any other.
fn build_router(state: &AppState) -> Router {
    Router::new()
        .route("/health", axum::routing::get(crate::health::health))
        .nest("/api", super::router())
        .merge(super::public_router())
        .fallback(crate::spa::serve)
        .with_state(state.clone())
}

impl TestApp {
    async fn new() -> Self {
        Self::seeded(FIXTURE).await
    }

    /// The same router over the synthetic seed. Identical in every respect but
    /// the rows: a test that passes on one and fails on the other has found a
    /// dependency on the data, which is the whole point of having two.
    async fn synthetic() -> Self {
        Self::seeded(SYNTHETIC).await
    }

    async fn seeded(seed: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = crate::test_support::migrated_db(dir.path()).await;
        sqlx::raw_sql(seed).execute(&db).await.expect("seed");

        let state = AppState {
            db,
            // The shipped default: nothing is a proxy until the operator names
            // one. Tests that need a forwarding header believed say so with
            // `trust_proxies`.
            config: Arc::new(crate::test_support::config(
                dir.path(),
                TrustedProxies::None,
            )),
        };

        Self {
            router: build_router(&state),
            state,
            _dir: dir,
        }
    }

    async fn send(&self, mut request: Request<Body>) -> (StatusCode, Value) {
        let peer: SocketAddr = PEER.parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        self.send_raw(request).await
    }

    /// Exactly as handed over, connect info and all. Only the perimeter tests
    /// need this; everything else wants the default peer.
    async fn send_raw(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router responded");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    /// Point the instance at an explicit advertised origin, the way
    /// `DOLLET_ADVERTISED_BASE_URL` does.
    fn set_advertised_base_url(&mut self, value: Option<String>) {
        self.reconfigure(|config| config.advertised_base_url = value);
    }

    /// Point artwork alone somewhere else, the way `DOLLET_ARTWORK_BASE_URL`
    /// does.
    fn set_artwork_base_url(&mut self, value: Option<String>) {
        self.reconfigure(|config| config.artwork_base_url = value);
    }

    /// Nominate the peers whose `X-Forwarded-*` headers count, the way
    /// `DOLLET_TRUSTED_PROXIES` does.
    fn trust_proxies(&mut self, policy: TrustedProxies) {
        self.reconfigure(|config| config.trusted_proxies = policy);
    }

    /// The router holds its state by value, so a configuration change has to
    /// rebuild it or the handlers keep answering from the old one.
    fn reconfigure(&mut self, change: impl FnOnce(&mut Config)) {
        let mut config = (*self.state.config).clone();
        change(&mut config);
        self.state.config = Arc::new(config);
        self.router = build_router(&self.state);
    }

    /// Response status, headers and raw body, for the outputs that are not
    /// JSON.
    async fn raw(&self, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        let peer: SocketAddr = PEER.parse().unwrap();
        let mut request = Request::builder()
            .method("GET")
            .uri(uri)
            .header("host", "ipx.test:9191")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));

        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    /// A JSON GET with the `Host` the golden corpus uses, so absolute URLs
    /// are comparable.
    async fn public(&self, uri: &str) -> (StatusCode, Value) {
        let (status, _, body) = self.raw(uri).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    async fn anonymous(&self, method: &str, uri: &str) -> (StatusCode, Value) {
        self.send(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn json(
        &self,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));

        let body = match body {
            Some(value) => {
                builder = builder.header("content-type", "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };

        self.send(builder.body(body).unwrap()).await
    }

    async fn post_json(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.send(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    /// Start a request whose source address is spelled out, for the perimeter
    /// tests. Everything else goes through [`Self::send`] and gets `PEER`.
    fn from(&self, peer: &str) -> RequestBuilder<'_> {
        RequestBuilder {
            app: self,
            peer: Some(peer.parse().expect("peer address")),
            headers: Vec::new(),
        }
    }

    /// A request that arrives with no connect info at all.
    fn no_peer(&self) -> RequestBuilder<'_> {
        RequestBuilder {
            app: self,
            peer: None,
            headers: Vec::new(),
        }
    }

    /// The credential a principal presents, obtained the way that principal
    /// would obtain it.
    ///
    /// The deactivated admin is the interesting one: `POST /token/` refuses it,
    /// so there is no token to carry, and what it presents instead is the API
    /// key the seed gives it. Sending *nothing* would prove only that an
    /// anonymous request is refused, which is a different test.
    async fn login_as(&self, who: Principal) -> Credential {
        let password_login = |(username, password): (&'static str, &'static str)| async move {
            let (status, body) = self
                .post_json(
                    "/api/accounts/token/",
                    json!({ "username": username, "password": password }),
                )
                .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{username} could not sign in: {body}"
            );
            Credential::Bearer(body["access"].as_str().expect("access token").to_owned())
        };

        match who {
            Principal::Anonymous => Credential::None,
            Principal::Admin => password_login(synthetic::ADMIN).await,
            Principal::Standard => password_login(synthetic::STANDARD).await,
            Principal::Streamer => password_login(synthetic::STREAMER).await,
            Principal::StandardApiKey => Credential::ApiKey(synthetic::STANDARD_API_KEY.to_owned()),
            Principal::InactiveAdmin => {
                let (status, body) = self
                    .post_json(
                        "/api/accounts/token/",
                        json!({
                            "username": synthetic::INACTIVE_ADMIN.0,
                            "password": synthetic::INACTIVE_ADMIN.1,
                        }),
                    )
                    .await;
                assert_eq!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "a deactivated admin was issued a token: {body}"
                );
                Credential::ApiKey(synthetic::INACTIVE_API_KEY.to_owned())
            }
        }
    }

    /// One request as one principal, with the `Host` the outputs build their
    /// absolute URLs from.
    async fn request_as(
        &self,
        credential: &Credential,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "ipx.test:9191");
        builder = credential.apply(builder);

        let body = match body {
            Some(value) => {
                builder = builder.header("content-type", "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };

        self.send(builder.body(body).unwrap()).await
    }

    /// Log in the way the SPA does and keep the access token.
    async fn login(&self) -> String {
        let (status, body) = self
            .post_json(
                "/api/accounts/token/",
                json!({ "username": USERNAME, "password": PASSWORD }),
            )
            .await;

        assert_eq!(status, StatusCode::OK, "login failed: {body}");
        body["access"].as_str().expect("access token").to_owned()
    }
}

struct RequestBuilder<'a> {
    app: &'a TestApp,
    peer: Option<SocketAddr>,
    headers: Vec<(String, String)>,
}

impl RequestBuilder<'_> {
    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn bearer(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }

    async fn get(self, uri: &str) -> (StatusCode, Value) {
        let mut builder = Request::builder().method("GET").uri(uri);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }

        let mut request = builder.body(Body::empty()).unwrap();
        if let Some(peer) = self.peer {
            request.extensions_mut().insert(ConnectInfo(peer));
        }
        self.app.send_raw(request).await
    }

    async fn post_json(self, uri: &str, body: Value) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }

        let mut request = builder.body(Body::from(body.to_string())).unwrap();
        if let Some(peer) = self.peer {
            request.extensions_mut().insert(ConnectInfo(peer));
        }
        self.app.send_raw(request).await
    }
}

#[tokio::test]
async fn the_router_has_no_conflicting_routes() {
    // axum resolves by specificity and panics at startup on a conflicting
    // insert, so simply building it is the assertion.
    TestApp::new().await;
}

#[tokio::test]
async fn the_sample_instance_seeds_with_the_expected_counts() {
    let app = TestApp::new().await;
    let counts: Vec<(&str, i64)> = vec![
        // Twelve, plus the five hard-case channels the sample carries.
        ("channel", 17),
        ("stream", 21),
        ("logo", 10),
        ("channel_group", 5),
        ("channel_stream", 18),
        ("user", 1),
    ];

    for (table, expected) in counts {
        let actual: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&app.state.db)
            .await
            .unwrap();
        assert_eq!(actual, expected, "{table}");
    }
}

/// The checklist `docs/TESTING.md` sets for the second seed, asserted as
/// queries rather than trusted to a reading of the file.
///
/// It is here rather than in the README because a README cannot fail. Every
/// entry is a shape some later test reaches for, so a row deleted during a
/// tidy-up shows up as this failing with the shape it took away named, instead
/// of as three unrelated tests quietly passing against nothing.
#[tokio::test]
async fn the_synthetic_seed_carries_every_shape_an_output_can_take() {
    let app = TestApp::synthetic().await;

    let present: Vec<(&str, &str)> = vec![
        ("an integer channel number", "SELECT COUNT(*) FROM effective_channel WHERE channel_number = 1.0"),
        ("a fractional channel number", "SELECT COUNT(*) FROM effective_channel WHERE channel_number = 2.5"),
        ("an unnumbered channel", "SELECT COUNT(*) FROM effective_channel WHERE channel_number IS NULL"),
        ("a channel hidden from output", "SELECT COUNT(*) FROM channel WHERE hidden_from_output = 1"),
        ("an adult channel", "SELECT COUNT(*) FROM channel WHERE is_adult = 1"),
        ("an admin-only channel", "SELECT COUNT(*) FROM channel WHERE user_level = 10"),
        ("a standard-only channel", "SELECT COUNT(*) FROM channel WHERE user_level = 1"),
        ("an auto-created channel", "SELECT COUNT(*) FROM channel WHERE auto_created = 1"),
        ("a catch-up channel", "SELECT COUNT(*) FROM channel WHERE is_catchup = 1 AND catchup_days > 0"),
        (
            "a channel with an override on every overridable column",
            "SELECT COUNT(*) FROM channel_override WHERE name IS NOT NULL AND channel_number IS NOT NULL
             AND channel_group_id IS NOT NULL AND logo_id IS NOT NULL AND tvg_id IS NOT NULL
             AND tvc_guide_stationid IS NOT NULL AND epg_data_id IS NOT NULL AND stream_profile_id IS NOT NULL",
        ),
        (
            "a channel whose three streams span two provider accounts",
            "SELECT COUNT(*) FROM (SELECT cs.channel_id FROM channel_stream cs
             JOIN stream s ON s.id = cs.stream_id
             GROUP BY cs.channel_id HAVING COUNT(*) = 3 AND COUNT(DISTINCT s.m3u_account_id) > 1)",
        ),
        (
            "a channel with no streams",
            "SELECT COUNT(*) FROM channel c WHERE NOT EXISTS (SELECT 1 FROM channel_stream WHERE channel_id = c.id)",
        ),
        (
            "a channel whose only stream has no URL",
            "SELECT COUNT(*) FROM channel c JOIN channel_stream cs ON cs.channel_id = c.id
             JOIN stream s ON s.id = cs.stream_id
             GROUP BY c.id HAVING COUNT(*) = 1 AND MAX(s.url IS NULL) = 1",
        ),
        (
            "a channel on a dummy guide source",
            "SELECT COUNT(*) FROM effective_channel c JOIN epg_data d ON d.id = c.epg_data_id
             JOIN epg_source s ON s.id = d.epg_source_id WHERE s.source_type = 'dummy'",
        ),
        (
            "a channel mapped to guide data with no programmes",
            "SELECT COUNT(*) FROM effective_channel c JOIN epg_data d ON d.id = c.epg_data_id
             WHERE NOT EXISTS (SELECT 1 FROM program WHERE epg_data_id = d.id)",
        ),
        (
            "a channel mapped to guide data while carrying no tvg_id",
            "SELECT COUNT(*) FROM effective_channel WHERE epg_data_id IS NOT NULL AND tvg_id IS NULL",
        ),
        (
            "a channel on the redirect stream profile",
            "SELECT COUNT(*) FROM effective_channel c JOIN stream_profile p ON p.id = c.stream_profile_id
             WHERE p.name = 'redirect'",
        ),
        (
            "a channel on a stream profile that names a user agent",
            "SELECT COUNT(*) FROM effective_channel c JOIN stream_profile p ON p.id = c.stream_profile_id
             WHERE p.user_agent_id IS NOT NULL AND p.command = ''",
        ),
        ("a name carrying a double quote", "SELECT COUNT(*) FROM channel WHERE name LIKE '%\"%'"),
        ("a name carrying an ampersand", "SELECT COUNT(*) FROM channel WHERE name LIKE '%&%'"),
        ("a name carrying angle brackets", "SELECT COUNT(*) FROM channel WHERE name LIKE '%<%' AND name LIKE '%>%'"),
        ("a non-ASCII name", "SELECT COUNT(*) FROM channel WHERE name <> CAST(name AS BLOB)"),
        ("a name with leading and trailing whitespace", "SELECT COUNT(*) FROM channel WHERE name <> TRIM(name)"),
        ("a standard M3U account with a URL", "SELECT COUNT(*) FROM m3u_account WHERE account_type = 'standard' AND server_url IS NOT NULL AND locked = 0 AND is_active = 1"),
        ("an Xtream account with credentials and max_streams = 1", "SELECT COUNT(*) FROM m3u_account WHERE account_type = 'xtream_codes' AND username IS NOT NULL AND password IS NOT NULL AND max_streams = 1"),
        ("an inactive account", "SELECT COUNT(*) FROM m3u_account WHERE is_active = 0"),
        ("the locked custom account with a hand-added stream", "SELECT COUNT(*) FROM stream WHERE is_custom = 1 AND m3u_account_id = 1"),
        ("a filter that excludes a group", "SELECT COUNT(*) FROM m3u_filter WHERE filter_type = 'group' AND exclude = 1"),
        ("a provider profile with a real search/replace", "SELECT COUNT(*) FROM m3u_account_profile WHERE search_pattern <> '^(.*)$' AND replace_pattern <> '$1'"),
        ("an XMLTV source", "SELECT COUNT(*) FROM epg_source WHERE source_type = 'xmltv' AND is_active = 1"),
        ("a dummy source", "SELECT COUNT(*) FROM epg_source WHERE source_type = 'dummy'"),
        ("an inactive source", "SELECT COUNT(*) FROM epg_source WHERE is_active = 0"),
        ("an EPG match suggestion", "SELECT COUNT(*) FROM epg_match_suggestion"),
        ("a group with a disabled account link", "SELECT COUNT(*) FROM channel_group_m3u_account WHERE enabled = 0"),
        ("a group with auto channel sync and numbering options", "SELECT COUNT(*) FROM channel_group_m3u_account WHERE auto_channel_sync = 1 AND custom_properties LIKE '%channel_numbering_mode%'"),
        ("a group with no streams", "SELECT COUNT(*) FROM channel_group g WHERE NOT EXISTS (SELECT 1 FROM stream WHERE channel_group_id = g.id)"),
        ("a logo used by exactly one channel", "SELECT COUNT(*) FROM (SELECT l.id FROM logo l JOIN effective_channel c ON c.logo_id = l.id GROUP BY l.id HAVING COUNT(*) = 1)"),
        ("a logo used by several channels", "SELECT COUNT(*) FROM (SELECT l.id FROM logo l JOIN effective_channel c ON c.logo_id = l.id GROUP BY l.id HAVING COUNT(*) > 1)"),
        ("a logo used by nothing", "SELECT COUNT(*) FROM logo l WHERE NOT EXISTS (SELECT 1 FROM channel WHERE logo_id = l.id) AND NOT EXISTS (SELECT 1 FROM channel_override WHERE logo_id = l.id)"),
        ("an admin", "SELECT COUNT(*) FROM user WHERE user_level = 10 AND is_active = 1"),
        ("a standard user with an xc password, a key, a stream limit and a profile", "SELECT COUNT(*) FROM user u WHERE u.user_level = 1 AND u.api_key IS NOT NULL AND u.stream_limit = 1 AND u.custom_properties LIKE '%xc_password%' AND EXISTS (SELECT 1 FROM user_channel_profile WHERE user_id = u.id)"),
        ("a streamer", "SELECT COUNT(*) FROM user WHERE user_level = 0"),
        ("an inactive admin", "SELECT COUNT(*) FROM user WHERE user_level = 10 AND is_active = 0"),
        ("a channel in neither extra profile", "SELECT COUNT(*) FROM channel c WHERE NOT EXISTS (SELECT 1 FROM channel_profile_membership WHERE channel_id = c.id AND channel_profile_id IN (1001, 1002))"),
        ("a job left running by a previous process", "SELECT COUNT(*) FROM job WHERE state = 'running'"),
        ("a notification nobody has acknowledged", "SELECT COUNT(*) FROM notification WHERE acknowledged_at IS NULL"),
        ("a notification acknowledged despite still recurring", "SELECT COUNT(*) FROM notification WHERE acknowledged_at IS NOT NULL AND occurrences > 1"),
        ("a notification raised exactly once", "SELECT COUNT(*) FROM notification WHERE occurrences = 1"),
        ("a programme ending exactly now", "SELECT COUNT(*) FROM program WHERE end_time <= strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00' AND start_time > strftime('%Y-%m-%d %H:%M:%f', 'now', '-2 hours') || '+00:00'"),
        ("a programme spanning now", "SELECT COUNT(*) FROM program WHERE start_time <= strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00' AND end_time > strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'"),
        ("two programmes that overlap", "SELECT COUNT(*) FROM program a JOIN program b ON b.epg_data_id = a.epg_data_id AND b.id > a.id WHERE b.start_time < a.end_time AND b.end_time > a.start_time"),
    ];

    for (shape, query) in present {
        let found: i64 = sqlx::query_scalar(query)
            .fetch_one(&app.state.db)
            .await
            .unwrap_or_else(|e| panic!("{shape}: {e}"));
        assert!(found > 0, "the synthetic seed no longer has {shape}");
    }

    // Every job state the scheduler can leave behind, so a Jobs page test never
    // has to manufacture one.
    let states: Vec<String> = sqlx::query_scalar("SELECT DISTINCT state FROM job ORDER BY state")
        .fetch_all(&app.state.db)
        .await
        .unwrap();
    assert_eq!(
        states,
        vec!["cancelled", "failed", "idle", "running", "success"],
        "a job state went missing from the seed"
    );

    // And the gap: nothing scheduled between +90 and +180 minutes, which is
    // what makes "this channel has nothing on later" a real answer rather than
    // an artefact of a short feed.
    let in_gap: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM program
         WHERE start_time < strftime('%Y-%m-%d %H:%M:%f', 'now', '+170 minutes') || '+00:00'
           AND end_time > strftime('%Y-%m-%d %H:%M:%f', 'now', '+100 minutes') || '+00:00'",
    )
    .fetch_one(&app.state.db)
    .await
    .unwrap();
    assert_eq!(in_gap, 0, "the deliberate gap in the guide was filled in");
}

/// Both seeds have to build the same router, or a test that passes on one and
/// fails on the other says something about the harness rather than the code.
#[tokio::test]
async fn both_seeds_build_the_same_router() {
    let sample = TestApp::new().await;
    let synthetic = TestApp::synthetic().await;

    // `/api/core/version/` needs no data and no credential, so a difference
    // here can only come from the router.
    for app in [&sample, &synthetic] {
        let (status, body) = app.anonymous("GET", "/api/core/version/").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

/// Every principal the matrix runs is a credential that actually resolves to
/// the account the seed says it is.
///
/// Without this the matrix is unfalsifiable in the worst direction: a typo in
/// one password would make that principal anonymous, every "refused" row would
/// still pass, and the test would report the strongest possible result while
/// checking nothing.
#[tokio::test]
async fn every_principal_signs_in_as_the_account_the_seed_describes() {
    let app = TestApp::synthetic().await;

    // `users/me/` is the one endpoint every authenticated level may reach, so
    // it is where "this credential is that person" can be asked directly.
    let expected: [(Principal, &str, i64); 4] = [
        (Principal::Admin, synthetic::ADMIN.0, 10),
        (Principal::Standard, synthetic::STANDARD.0, 1),
        (Principal::Streamer, synthetic::STREAMER.0, 0),
        (Principal::StandardApiKey, synthetic::STANDARD.0, 1),
    ];

    for (who, username, level) in expected {
        let credential = app.login_as(who).await;
        let (status, me) = app
            .request_as(&credential, "GET", "/api/accounts/users/me/", None)
            .await;
        assert_eq!(status, StatusCode::OK, "{}: {me}", who.name());
        assert_eq!(me["username"], username, "{}", who.name());
        assert_eq!(me["user_level"], level, "{}", who.name());
    }

    // The admin's own key reaches the same account as the admin's token, which
    // is what makes the API-key row of the matrix a statement about the *user*
    // rather than about the header.
    let (status, me) = app
        .request_as(
            &Credential::ApiKey(synthetic::ADMIN_API_KEY.to_owned()),
            "GET",
            "/api/accounts/users/me/",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["username"], synthetic::ADMIN.0);

    // The deactivated admin has both a password and a key, and neither works.
    // `login_as` already asserts the password is refused; this is the key.
    let (status, body) = app
        .request_as(
            &app.login_as(Principal::InactiveAdmin).await,
            "GET",
            "/api/accounts/users/me/",
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a deactivated admin's API key authenticated: {body}"
    );

    // And the anonymous principal is genuinely anonymous.
    assert!(matches!(
        app.login_as(Principal::Anonymous).await,
        Credential::None
    ));
    assert_eq!(
        Principal::ALL.len(),
        6,
        "a principal was added without a row above"
    );
}

#[tokio::test]
async fn anonymous_requests_are_rejected() {
    let app = TestApp::new().await;

    for uri in [
        "/api/channels/channels/",
        "/api/channels/streams/",
        "/api/core/settings/",
        "/api/accounts/users/me/",
    ] {
        let (status, _) = app.anonymous("GET", uri).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
}

#[tokio::test]
async fn the_imported_password_hash_logs_in_and_refreshes() {
    let app = TestApp::new().await;
    let (status, tokens) = app
        .post_json(
            "/api/accounts/token/",
            json!({ "username": USERNAME, "password": PASSWORD }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let refresh = tokens["refresh"].as_str().unwrap();
    let (status, refreshed) = app
        .post_json(
            "/api/accounts/token/refresh/",
            json!({ "refresh": refresh }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let access = refreshed["access"].as_str().unwrap();
    let (status, me) = app
        .json("GET", "/api/accounts/users/me/", access, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["username"], USERNAME);
    assert_eq!(me["user_level"], 10);

    // A refresh token must not work as a bearer token.
    let (status, _) = app
        .json("GET", "/api/accounts/users/me/", refresh, None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_user_both_fail() {
    let app = TestApp::new().await;

    let (status, _) = app
        .post_json(
            "/api/accounts/token/",
            json!({ "username": USERNAME, "password": "wrong" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = app
        .post_json(
            "/api/accounts/token/",
            json!({ "username": "nobody", "password": PASSWORD }),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The throttle, over HTTP rather than through its own module.
///
/// `auth::throttle`'s unit tests cover the counter. What they cannot reach is
/// whether `token` consults it at all — `throttle::check` is one line at the
/// top of the handler, and deleting that line leaves every one of them green.
///
/// Driven from a documentation address rather than `PEER`, because the counter
/// is process-global and the rest of this binary signs in from `PEER`.
#[tokio::test]
async fn ten_failed_sign_ins_lock_that_address_out_but_only_that_address() {
    const ATTACKER: &str = "198.51.100.10:50000";
    let app = TestApp::new().await;

    for attempt in 1..=10 {
        let (status, _) = app
            .from(ATTACKER)
            .post_json(
                "/api/accounts/token/",
                json!({ "username": USERNAME, "password": "wrong" }),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
    }

    // The *right* password, and still refused. The check runs ahead of the
    // lookup, so an address that has spent its attempts cannot spend a lucky
    // guess to get out of them.
    let (status, body) = app
        .from(ATTACKER)
        .post_json(
            "/api/accounts/token/",
            json!({ "username": USERNAME, "password": PASSWORD }),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        body.to_string()
            .contains("too many failed sign-in attempts"),
        "{body}"
    );

    // Keyed on the address, so an attacker cannot lock the administrator out
    // of their own instance by failing at it from somewhere else.
    let (status, _) = app
        .from("198.51.100.11:50000")
        .post_json(
            "/api/accounts/token/",
            json!({ "username": USERNAME, "password": PASSWORD }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

/// Both paths must cost exactly one key derivation. Generating a dummy hash
/// for the missing-user case cost two, making "no such user" measurably
/// *slower* than "wrong password" — the enumeration oracle inverted by the
/// defence against one. Counted rather than timed: wall clock in an
/// unoptimised build is far too noisy to catch a 2x difference reliably.
#[tokio::test]
#[ignore = "counts a process-global; needs --test-threads=1 to be meaningful"]
async fn a_missing_account_and_a_wrong_password_cost_the_same() {
    let app = TestApp::new().await;

    let cost = |body: Value| async {
        let before = dollet_core::auth::password::derivations();
        let (status, _) = app.post_json("/api/accounts/token/", body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        dollet_core::auth::password::derivations() - before
    };

    let wrong_password = cost(json!({ "username": USERNAME, "password": "wrong" })).await;
    let no_such_user = cost(json!({ "username": "nobody", "password": PASSWORD })).await;

    assert_eq!(wrong_password, 1);
    assert_eq!(no_such_user, wrong_password);
}

#[tokio::test]
async fn an_api_key_authenticates_without_a_login_round_trip() {
    let app = TestApp::new().await;

    let request = Request::builder()
        .method("GET")
        .uri("/api/accounts/users/me/")
        .header("x-api-key", API_KEY)
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.send(request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["username"], USERNAME);

    // The scheme form is accepted too.
    let request = Request::builder()
        .method("GET")
        .uri("/api/accounts/users/me/")
        .header("authorization", format!("ApiKey {API_KEY}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(request).await.0, StatusCode::OK);

    let request = Request::builder()
        .method("GET")
        .uri("/api/accounts/users/me/")
        .header("x-api-key", "not-the-key")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(request).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn channels_come_back_in_lineup_order_with_effective_values() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, body) = app
        .json("GET", "/api/channels/channels/?all=true", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let channels = rows(&body);
    // The editor shows everything, hidden included: a hidden channel has to be
    // visible somewhere to be un-hidden.
    assert_eq!(channels.len(), 17);
    assert_eq!(channels[0]["effective_channel_number"], 1.0);
    assert_eq!(
        channels[0]["effective_name"],
        fixture_name(&app.state.db, "channel", 171).await
    );
    assert_eq!(
        channels[0]["group_name"],
        fixture_name(&app.state.db, "channel_group", 6).await
    );
    assert!(channels[0]["logo_url"].is_string());
    assert!(channels[0]["override"].is_null());
    assert!(!channels[0]["streams"].as_array().unwrap().is_empty());
}

/// Whether a channel has a guide is `epg_data_id`, never `tvg_id`.
///
/// They are independent columns and the path that maps most channels — fuzzy
/// name matching, and assigning by hand — writes only the first. So "mapped,
/// with no `tvg-id` at all" is the ordinary case, and a lineup that reports the
/// provider's label shows a dash for a channel whose programmes the TV Guide
/// renders perfectly.
#[tokio::test]
async fn a_channel_reports_the_guide_it_is_mapped_to_not_its_provider_label() {
    let app = TestApp::synthetic().await;
    let token = match app.login_as(Principal::Admin).await {
        Credential::Bearer(token) => token,
        other => panic!("the admin did not get a bearer token: {other:?}"),
    };

    let (status, body) = app
        .json("GET", "/api/channels/channels/?all=true", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let channel = |id: i64| {
        rows(&body)
            .iter()
            .find(|channel| channel["id"] == id)
            .unwrap_or_else(|| panic!("channel {id} is missing: {body}"))
    };

    // Guide data assigned, no label to show for it.
    let gap = channel(1012);
    assert!(gap["tvg_id"].is_null(), "{gap}");
    assert!(gap["effective_tvg_id"].is_null(), "{gap}");
    assert_eq!(gap["effective_epg_data_id"], 1003);
    assert_eq!(gap["epg_name"], "Synth Gap Guide");

    // The guide channel's name, not the channel's own — which is the other way
    // this can look right and be wrong.
    let sports = channel(1000);
    assert_eq!(sports["effective_name"], "Synth One");
    assert_eq!(sports["epg_name"], "Synth Sports Guide");

    // And a channel with no mapping has nothing to name.
    let unmapped = channel(1002);
    assert!(unmapped["effective_epg_data_id"].is_null(), "{unmapped}");
    assert!(unmapped["epg_name"].is_null(), "{unmapped}");
}

#[tokio::test]
async fn an_override_changes_the_effective_values_and_the_order() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, updated) = app
        .json(
            "PATCH",
            "/api/channels/channels/173/",
            &token,
            Some(json!({
                "override": { "name": "AAA Renamed", "channel_number": 0.5 }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["effective_name"], "AAA Renamed");
    assert_eq!(updated["effective_channel_number"], 0.5);
    // The provider's own values must be untouched, because sync keeps writing
    // to them and the override has to survive the next refresh.
    assert_eq!(
        updated["name"],
        fixture_name(&app.state.db, "channel", 173).await
    );
    assert_eq!(updated["channel_number"], 2.0);

    let (_, list) = app
        .json("GET", "/api/channels/channels/?all=true", &token, None)
        .await;
    assert_eq!(
        rows(&list)[0]["id"],
        173,
        "override did not reorder the lineup"
    );

    // Searching hits the effective name, not the provider's.
    let (_, found) = app
        .json(
            "GET",
            "/api/channels/channels/?search=Renamed",
            &token,
            None,
        )
        .await;
    assert_eq!(rows(&found).len(), 1);

    // Clearing it reverts without touching the base row.
    let (status, reverted) = app
        .json(
            "PATCH",
            "/api/channels/channels/173/",
            &token,
            Some(json!({ "override": null })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        reverted["effective_name"],
        fixture_name(&app.state.db, "channel", 173).await
    );
}

/// SQLite sorts NULLs first on ASC where PostgreSQL sorts them last, so an
/// unnumbered channel would lead the HDHR lineup and the M3U — the first thing
/// Plex shows.
#[tokio::test]
async fn an_unnumbered_channel_sorts_last_not_first() {
    let app = TestApp::new().await;
    let token = app.login().await;

    // A channel created without a number is given one, so unnumbered is a
    // state the operator has to choose: clear the number after the fact.
    let (status, created) = app
        .json(
            "POST",
            "/api/channels/channels/",
            &token,
            Some(json!({ "name": "No Number" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let unnumbered = created["id"].as_i64().unwrap();
    let (status, cleared) = app
        .json(
            "PATCH",
            &format!("/api/channels/channels/{unnumbered}/"),
            &token,
            Some(json!({ "channel_number": null })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["effective_channel_number"], Value::Null);

    let (_, list) = app
        .json("GET", "/api/channels/channels/?all=true", &token, None)
        .await;
    let ids: Vec<i64> = rows(&list)
        .iter()
        .map(|c| c["id"].as_i64().unwrap())
        .collect();
    // The fixture already carries a numberless channel, so both sort to the
    // end; what matters is that neither leads.
    assert!(
        ids.iter().rev().take(2).any(|id| *id == unnumbered),
        "the new numberless channel did not sort to the end"
    );
    assert_ne!(ids[0], unnumbered, "NULL led the lineup");
    assert_eq!(rows(&list)[0]["effective_channel_number"], 1.0);

    // And on the descending branch, where the tie-break has to be mirrored.
    let (_, descending) = app
        .json(
            "GET",
            "/api/channels/channels/?ordering=-channel_number",
            &token,
            None,
        )
        .await;
    assert!(
        rows(&descending)
            .iter()
            .take(2)
            .any(|c| c["id"] == unnumbered),
        "a numberless channel did not lead the descending order"
    );
}

/// Timestamps are compared as text, and sqlx's own `DateTime<Utc>` encoding
/// uses a `T` separator where the column defaults use a space. `'T'` sorts
/// after `' '`, so two formats in one column silently invert `ORDER BY`.
#[tokio::test]
async fn every_timestamp_column_holds_one_format() {
    let app = TestApp::new().await;
    let token = app.login().await;

    // Touch a row through each writer: the fixture, a create, and an update.
    app.json(
        "POST",
        "/api/channels/channels/",
        &token,
        Some(json!({ "name": "Fresh", "channel_number": 900 })),
    )
    .await;
    app.json(
        "PATCH",
        "/api/channels/channels/171/",
        &token,
        Some(json!({ "name": "Touched" })),
    )
    .await;
    app.json(
        "POST",
        "/api/accounts/api-keys/generate/",
        &token,
        Some(json!({})),
    )
    .await;

    let columns = [
        ("channel", "created_at"),
        ("channel", "updated_at"),
        ("stream", "last_seen"),
        ("stream", "updated_at"),
        ("program", "start_time"),
        ("program", "end_time"),
        ("user", "last_login"),
        ("user", "date_joined"),
        ("m3u_account", "created_at"),
        ("epg_source", "created_at"),
        ("system_event", "occurred_at"),
    ];

    for (table, column) in columns {
        let odd: Option<String> = sqlx::query_scalar(&format!(
            "SELECT {column} FROM {table}
             WHERE {column} IS NOT NULL
               AND {column} NOT GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] \
[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9]+00:00'
             LIMIT 1"
        ))
        .fetch_optional(&app.state.db)
        .await
        .unwrap()
        .flatten();

        assert_eq!(odd, None, "{table}.{column} is in a second format");
    }
}

#[tokio::test]
async fn streams_are_paginated_with_the_drf_envelope() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, body) = app
        .json("GET", "/api/channels/streams/?page_size=10", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 21);
    assert_eq!(rows(&body).len(), 10);
    assert_eq!(body["page"], 1);
    assert_eq!(body["pages"], 3);

    // The one list that refuses to hand over everything at once. A silent cap
    // is how a client comes to believe it has the whole catalogue.
    let (status, refused) = app
        .json("GET", "/api/channels/streams/?all=true", &token, None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        refused["detail"]
            .as_str()
            .is_some_and(|d| d.contains("page")),
        "{refused}"
    );
}

#[tokio::test]
async fn stream_search_is_substring_not_token_prefix() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (_, all) = app
        .json("GET", "/api/channels/streams/?page_size=1", &token, None)
        .await;
    let total = all["count"].as_i64().unwrap();

    let (_, matched) = app
        .json(
            "GET",
            "/api/channels/streams/?search=HD&page_size=100",
            &token,
            None,
        )
        .await;
    let count = matched["count"].as_i64().unwrap();
    assert!(count > 0 && count < total, "expected a partial match set");

    // The reason FTS5 is out: every hit here is mid-word.
    for stream in matched["results"].as_array().unwrap() {
        let name = stream["name"].as_str().unwrap().to_lowercase();
        assert!(name.contains("hd"), "{name} matched without containing HD");
    }
}

/// LIKE wildcards in a search term are the user's literal text, not syntax.
/// Unescaped, `%` matched every row and a name containing `_` could not be
/// searched for at all.
#[tokio::test]
async fn wildcards_in_a_search_term_are_matched_literally() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (_, everything) = app
        .json("GET", "/api/channels/channels/?all=true", &token, None)
        .await;
    let total = rows(&everything).len();

    let (_, wildcard) = app
        .json(
            "GET",
            "/api/channels/channels/?all=true&search=%25",
            &token,
            None,
        )
        .await;
    assert!(rows(&wildcard).len() < total, "`%` matched every channel");

    let (status, created) = app
        .json(
            "POST",
            "/api/channels/channels/",
            &token,
            Some(json!({ "name": "VRIX_HD", "channel_number": 901 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (_, literal) = app
        .json(
            "GET",
            "/api/channels/channels/?search=VRIX_HD",
            &token,
            None,
        )
        .await;
    assert_eq!(rows(&literal).len(), 1);

    // The underscore is not a single-character wildcard, so this matches nothing.
    let (_, as_wildcard) = app
        .json(
            "GET",
            "/api/channels/channels/?search=VRIXXHD",
            &token,
            None,
        )
        .await;
    assert!(rows(&as_wildcard).is_empty());
}

#[tokio::test]
async fn settings_come_back_as_the_seven_groups_and_never_the_signing_key() {
    let app = TestApp::new().await;
    let token = app.login().await;

    // Force the signing key to exist before listing.
    let (status, body) = app.json("GET", "/api/core/settings/", &token, None).await;
    assert_eq!(status, StatusCode::OK);

    let keys: Vec<&str> = rows(&body)
        .iter()
        .map(|row| row["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        keys,
        vec![
            "stream_settings",
            "proxy_settings",
            "network_access",
            "system_settings",
            "epg_settings",
            "numbering_settings",
            "backup_settings",
        ]
    );
    assert!(!body.to_string().contains("jwt_secret"));
}

#[tokio::test]
async fn a_settings_patch_merges_rather_than_replacing() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, updated) = app
        .json(
            "PATCH",
            "/api/core/settings/proxy_settings/",
            &token,
            Some(json!({ "value": { "ring_seconds": 30 } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["value"]["ring_seconds"], 30);
    assert_eq!(updated["value"]["buffering_timeout"], 15);

    // Addressable by id too.
    let (status, by_id) = app.json("GET", "/api/core/settings/2/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_id["key"], "proxy_settings");
    assert_eq!(by_id["value"]["ring_seconds"], 30);
}

#[tokio::test]
async fn a_malformed_cidr_is_refused_instead_of_silently_widening_access() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, body) = app
        .json(
            "PATCH",
            "/api/core/settings/network_access/",
            &token,
            Some(json!({ "value": { "UI": "10.0.0.0/8,nonsense" } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // The offending entry is named against its endpoint class rather than in
    // the sentence; `an_invalid_cidr_is_reported_per_endpoint_class` covers the
    // shape, this covers the refusal.
    assert_eq!(body["fields"]["UI"], "not a CIDR range: nonsense", "{body}");

    // Loopback, because every request in this harness comes from there and the
    // perimeter check is enforced against the peer address.
    let (status, saved) = app
        .json(
            "PATCH",
            "/api/core/settings/network_access/",
            &token,
            Some(json!({ "value": { "UI": "127.0.0.0/8" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["value"]["UI"], "127.0.0.0/8");

    // A later PATCH that mentions only STREAMS must not drop UI: an absent
    // endpoint allows every address, so that would open the admin UI up.
    let (status, merged) = app
        .json(
            "PATCH",
            "/api/core/settings/network_access/",
            &token,
            Some(json!({ "value": { "STREAMS": "192.168.0.0/16" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(merged["value"]["UI"], "127.0.0.0/8");
    assert_eq!(merged["value"]["STREAMS"], "192.168.0.0/16");
}

/// The allowlist has to bind to the address the packets came from. Consulting
/// only `X-Forwarded-For` would allow every direct connection and believe a
/// forged header — a control enforced only against clients that honestly
/// declare a blocked address.
#[tokio::test]
async fn the_perimeter_binds_to_the_peer_address_not_to_a_header() {
    let app = TestApp::new().await;
    let token = app.login().await;
    restrict_ui(&app, &token, "10.0.0.0/8").await;

    let blocked = app
        .from("127.0.0.1:40000")
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(blocked.0, StatusCode::FORBIDDEN, "peer address ignored");

    // A forged header claiming an allowed address.
    let spoofed = app
        .from("127.0.0.1:40000")
        .header("x-forwarded-for", "10.1.2.3")
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(spoofed.0, StatusCode::FORBIDDEN, "forged header believed");

    let allowed = app
        .from("10.1.2.3:50000")
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(allowed.0, StatusCode::OK);
}

#[tokio::test]
async fn a_request_with_no_determinable_source_is_denied_when_restricted() {
    let app = TestApp::new().await;
    let token = app.login().await;

    // Unrestricted: an unknown address must still work, or a misconfigured
    // deployment locks everyone out of a system nobody asked to protect.
    let (status, _) = app
        .no_peer()
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(status, StatusCode::OK);

    restrict_ui(&app, &token, "127.0.0.0/8").await;

    let (status, _) = app
        .no_peer()
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_forwarding_header_counts_only_from_a_trusted_peer() {
    let mut app = TestApp::new().await;
    app.trust_proxies(TrustedProxies::PrivateAndLoopback);
    let token = app.login().await;
    restrict_ui(&app, &token, "203.0.113.0/24").await;

    // 127.0.0.1 is a trusted proxy under the configured policy, so the
    // rightmost untrusted hop in its X-Forwarded-For is the real client.
    let (status, _) = app
        .from("127.0.0.1:40000")
        .header("x-forwarded-for", "198.51.100.7, 203.0.113.9")
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(status, StatusCode::OK);

    // Everything left of the last untrusted hop is client-written and must not
    // be able to promote itself.
    let (status, _) = app
        .from("127.0.0.1:40000")
        .header("x-forwarded-for", "203.0.113.9, 198.51.100.7")
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A public peer is not a proxy, so its header is ignored entirely.
    let (status, _) = app
        .from("198.51.100.7:40000")
        .header("x-forwarded-for", "203.0.113.9")
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Under the shipped policy nothing is a proxy, so every device on the
/// operator's LAN is a client rather than something allowed to say who it is.
/// Trusting RFC1918 by default would let every device on a home LAN say who
/// it is.
#[tokio::test]
async fn a_lan_peer_cannot_rename_itself_under_the_shipped_policy() {
    let app = TestApp::new().await;
    let token = app.login().await;
    restrict_ui(&app, &token, "203.0.113.0/24").await;

    for header in ["x-forwarded-for", "x-real-ip"] {
        let (status, _) = app
            .from("192.168.1.50:40000")
            .header(header, "203.0.113.9")
            .bearer(&token)
            .get("/api/accounts/users/me/")
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{header} was believed");
    }
}

/// `X-Real-IP` is a second opinion, not a tiebreaker. Reading it alongside an
/// `X-Forwarded-For` would let a client past the chain its own proxy wrote by
/// adding the header that chain does not use.
#[tokio::test]
async fn x_real_ip_is_read_only_when_no_forwarded_for_arrived() {
    let mut app = TestApp::new().await;
    app.trust_proxies(TrustedProxies::PrivateAndLoopback);
    let token = app.login().await;
    restrict_ui(&app, &token, "203.0.113.0/24").await;

    // Alone, it is what a single-hop proxy sets.
    let (status, _) = app
        .from("127.0.0.1:40000")
        .header("x-real-ip", "203.0.113.9")
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = app
        .from("127.0.0.1:40000")
        .header("x-forwarded-for", "198.51.100.7")
        .header("x-real-ip", "203.0.113.9")
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "X-Real-IP overrode the chain"
    );
}

/// A chain written end to end by the operator's own proxies has no untrusted
/// hop to find, and falling back to the peer there would report the innermost
/// proxy as the client — one address for the whole LAN.
#[tokio::test]
async fn a_fully_trusted_chain_resolves_to_the_client_the_outermost_proxy_saw() {
    let mut app = TestApp::new().await;
    app.trust_proxies(TrustedProxies::PrivateAndLoopback);
    let token = app.login().await;
    restrict_ui(&app, &token, "10.0.0.0/8").await;

    // nginx on loopback behind a LAN gateway: the leftmost entry is the only
    // one of the three that is a client rather than a proxy.
    let (status, _) = app
        .from("127.0.0.1:40000")
        .header("x-forwarded-for", "10.1.2.3, 192.168.1.5")
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(status, StatusCode::OK);

    // And it is still the chain that decides, not the leftmost entry
    // unconditionally: a client-written hop in front of the real one is to the
    // left of an untrusted address, so the untrusted one wins.
    let (status, _) = app
        .from("127.0.0.1:40000")
        .header("x-forwarded-for", "10.1.2.3, 198.51.100.7")
        .bearer(&token)
        .get("/api/accounts/users/me/")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

async fn restrict_ui(app: &TestApp, token: &str, cidr: &str) {
    let (status, _) = app
        .json(
            "PATCH",
            "/api/core/settings/network_access/",
            token,
            Some(json!({ "value": { "UI": cidr } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_user_can_be_created_already_disabled() {
    let app = TestApp::new().await;
    let admin = app.login().await;

    let (status, created) = app
        .json(
            "POST",
            "/api/accounts/users/",
            &admin,
            Some(json!({
                "username": "pending",
                "password": "pending-pass",
                "is_active": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["is_active"], false);

    let (status, _) = app
        .post_json(
            "/api/accounts/token/",
            json!({ "username": "pending", "password": "pending-pass" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn clearing_a_settings_field_resets_it_instead_of_failing() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, _) = app
        .json(
            "PATCH",
            "/api/core/settings/stream_settings/",
            &token,
            Some(json!({ "value": { "m3u_hash_key": "url,tvg_id" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, cleared) = app
        .json(
            "PATCH",
            "/api/core/settings/stream_settings/",
            &token,
            Some(json!({ "value": { "m3u_hash_key": null } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["value"]["m3u_hash_key"], "url");
    // The rest of the section is untouched by a patch that named one field.
    assert_eq!(cleared["value"]["default_stream_profile"], 3);
}

#[tokio::test]
async fn locked_profiles_ship_and_cannot_be_deleted() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (_, profiles) = app
        .json("GET", "/api/core/streamprofiles/", &token, None)
        .await;
    let names: Vec<&str> = rows(&profiles)
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["ffmpeg", "streamlink", "proxy", "redirect", "vlc"]
    );

    let (status, _) = app
        .json("DELETE", "/api/core/streamprofiles/4/", &token, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Renaming a locked profile is ignored rather than accepted: the proxy
    // resolves `redirect` by name.
    let (status, unchanged) = app
        .json(
            "PATCH",
            "/api/core/streamprofiles/4/",
            &token,
            Some(json!({ "name": "something else", "command": "rm" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(unchanged["name"], "redirect");
    assert_eq!(unchanged["command"], "");
}

#[tokio::test]
async fn a_duplicate_name_is_a_conflict_not_a_server_error() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, _) = app
        .json(
            "POST",
            "/api/channels/groups/",
            &token,
            // The fixture's own group name in a different case. Names are
            // `COLLATE NOCASE`, so this is a duplicate.
            Some(json!({
                "name": fixture_name(&app.state.db, "channel_group", 3)
                    .await
                    .to_lowercase()
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_streamer_cannot_reach_admin_endpoints_or_escalate() {
    let app = TestApp::new().await;
    let admin = app.login().await;

    let (status, created) = app
        .json(
            "POST",
            "/api/accounts/users/",
            &admin,
            Some(json!({ "username": "viewer", "password": "viewer-pass", "user_level": 0 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["user_level"], 0);
    assert_eq!(created["is_staff"], false);

    let (status, tokens) = app
        .post_json(
            "/api/accounts/token/",
            json!({ "username": "viewer", "password": "viewer-pass" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let viewer = tokens["access"].as_str().unwrap();

    let (status, _) = app.json("GET", "/api/accounts/users/", viewer, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = app
        .json("GET", "/api/channels/channels/", viewer, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // `/users/me/` is the one thing they may reach, and it must not let them
    // raise their own level.
    let (status, me) = app
        .json(
            "PATCH",
            "/api/accounts/users/me/",
            viewer,
            Some(json!({ "user_level": 10, "email": "viewer@example.test" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["user_level"], 0);
    assert_eq!(me["email"], "viewer@example.test");
}

#[tokio::test]
async fn the_last_admin_cannot_be_removed() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, body) = app
        .json(
            "PATCH",
            "/api/accounts/users/1/",
            &token,
            Some(json!({ "user_level": 0 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, _) = app
        .json("DELETE", "/api/accounts/users/1/", &token, None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn api_keys_can_be_minted_and_revoked() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, generated) = app
        .json(
            "POST",
            "/api/accounts/api-keys/generate/",
            &token,
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{generated}");
    let key = generated["key"].as_str().unwrap().to_owned();
    assert_ne!(key, API_KEY);

    let request = Request::builder()
        .method("GET")
        .uri("/api/accounts/users/me/")
        .header("x-api-key", &key)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(request).await.0, StatusCode::OK);

    let (status, _) = app
        .json(
            "POST",
            "/api/accounts/api-keys/revoke/",
            &token,
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let request = Request::builder()
        .method("GET")
        .uri("/api/accounts/users/me/")
        .header("x-api-key", &key)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(request).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn channel_profiles_gain_every_channel_and_can_be_pared_down() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, created) = app
        .json(
            "POST",
            "/api/channels/profiles/",
            &token,
            Some(json!({ "name": "Kids" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let profile = created["id"].as_i64().unwrap();

    let (_, detail) = app
        .json(
            "GET",
            &format!("/api/channels/profiles/{profile}/"),
            &token,
            None,
        )
        .await;
    assert_eq!(detail["channels"].as_array().unwrap().len(), 17);

    let (status, _) = app
        .json(
            "POST",
            &format!("/api/channels/profiles/{profile}/channels/bulk-update/"),
            &token,
            Some(json!({ "channel_ids": [171, 173], "enabled": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (_, filtered) = app
        .json(
            "GET",
            &format!("/api/channels/channels/?all=true&channel_profile={profile}"),
            &token,
            None,
        )
        .await;
    assert_eq!(rows(&filtered).len(), 15);
}

#[tokio::test]
async fn logo_usage_counts_overrides_and_cleanup_spares_them() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, logos) = app
        .json(
            "GET",
            "/api/channels/logos/?no_pagination=true",
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let logos = rows(&logos);
    assert_eq!(logos.len(), 10);

    let unused: Vec<i64> = logos
        .iter()
        .filter(|logo| logo["channel_count"] == 0)
        .map(|logo| logo["id"].as_i64().unwrap())
        .collect();
    assert!(!unused.is_empty(), "fixture has no orphaned artwork");

    // Assigning one through an override must protect it from cleanup.
    let rescued = unused[0];
    let (status, _) = app
        .json(
            "PATCH",
            "/api/channels/channels/171/",
            &token,
            Some(json!({ "override": { "logo_id": rescued } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, cleaned) = app
        .json("POST", "/api/channels/logos/cleanup/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(cleaned["deleted"].as_i64().unwrap() > 0);

    // Reachable only through the override, and it survived.
    let (status, _) = app
        .json(
            "GET",
            &format!("/api/channels/logos/{rescued}/"),
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    for orphan in &unused[1..] {
        let (status, _) = app
            .json(
                "GET",
                &format!("/api/channels/logos/{orphan}/"),
                &token,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "logo {orphan} survived");
    }
}

#[tokio::test]
async fn a_channels_failover_order_round_trips() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, streams) = app
        .json("GET", "/api/channels/channels/171/streams/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let original: Vec<i64> = rows(&streams)
        .iter()
        .map(|s| s["id"].as_i64().unwrap())
        .collect();
    assert!(!original.is_empty());

    let mut reversed = original.clone();
    reversed.reverse();
    let (status, reordered) = app
        .json(
            "PUT",
            "/api/channels/channels/171/streams/",
            &token,
            Some(json!({ "ids": reversed })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&reordered)
            .iter()
            .map(|s| s["id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        reversed
    );
}

#[tokio::test]
async fn a_missing_row_is_a_404_rather_than_a_500() {
    let app = TestApp::new().await;
    let token = app.login().await;

    for uri in [
        "/api/channels/channels/99999/",
        "/api/channels/streams/99999/",
        "/api/channels/logos/99999/",
        "/api/core/useragents/99999/",
        "/api/core/settings/not_a_group/",
    ] {
        let (status, _) = app.json("GET", uri, &token, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

/// `fixtures/import/dispatcharr-backup.zip`: one Dispatcharr backup, exactly
/// as a user hands it over, read in-process — no container, no restore. The
/// tests below import it and read back what they expect to find;
/// `fixtures/import/README.md` says what is in it and why.
async fn backup_source() -> super::ImportSource {
    super::ImportSource::backup(&fixture_path("dispatcharr-backup.zip"))
        .await
        .expect("reading the backup fixture")
}

/// A first boot's database with the fixture imported into it.
async fn imported() -> (TempDir, sqlx::SqlitePool, super::importer::ImportReport) {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_instance(&dir).await;
    let report = super::run_import(backup_source().await, &db)
        .await
        .expect("import ran");
    (dir, db, report)
}

async fn count(db: &sqlx::SqlitePool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM \"{table}\""))
        .fetch_one(db)
        .await
        .unwrap()
}

/// The whole migration, checked by reading back what it wrote.
///
/// Not ignored, and needing nothing but the repository: this is the only path
/// a migrating user takes, so it runs on every `cargo test`.
#[tokio::test]
async fn importing_the_backup_writes_the_expected_rows() {
    let (_dir, db, report) = imported().await;

    // What the importer says it wrote. `channel_profile` is 1 here and 2 in
    // the table because the migration seeds an `All` profile, and the locked
    // catalogue rows are skipped rather than imported because this build
    // ships its own. Conflating the report with the tables would hide a real
    // failure in either direction, so both are pinned.
    let counts: Vec<(&str, u64)> = report
        .counts
        .iter()
        .map(|(table, n)| (table.as_str(), *n))
        .collect();
    assert_eq!(
        counts,
        [
            ("channel", 12),
            ("channel_group", 4),
            ("channel_group_m3u_account", 4),
            ("channel_override", 1),
            ("channel_profile", 1),
            ("channel_profile_membership", 2),
            ("channel_stream", 17),
            ("core_setting", 5),
            ("epg_data", 11),
            ("epg_source", 1),
            ("logo", 9),
            ("m3u_account", 2),
            ("m3u_account_profile", 2),
            ("m3u_filter", 3),
            ("output_profile_skipped", 2),
            ("program", 240),
            ("server_group", 1),
            ("stream", 20),
            ("stream_profile_skipped", 5),
            ("user", 2),
            ("user_agent", 3),
            ("user_channel_profile", 1),
        ]
    );
    for (table, expected) in [
        ("channel", 12),
        ("stream", 20),
        ("logo", 9),
        ("channel_stream", 17),
        ("epg_data", 11),
        ("program", 240),
        ("user", 2),
        ("m3u_filter", 3),
        ("channel_profile", 2),
        ("channel_profile_membership", 24),
        ("stream_profile", 5),
        ("output_profile", 2),
    ] {
        assert_eq!(count(&db, table).await, expected, "{table}");
    }

    // Every channel joined the seeded profile, so the outputs are not empty.
    let members: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM channel_profile_membership WHERE channel_profile_id = 1",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(members, 12);

    // The locked catalogue is this project's own, not the source's.
    let parameters: String =
        sqlx::query_scalar("SELECT parameters FROM output_profile WHERE id = 1")
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(parameters.contains("-hide_banner"));

    // `channel_stream.sort_order` *is* the failover order: position 0 is the
    // source tried first. Reordering it silently changes which provider every
    // channel plays from, and nothing in the UI would look wrong.
    let links: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT channel_id, sort_order FROM channel_stream ORDER BY channel_id, sort_order",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    let mut by_channel: std::collections::BTreeMap<i64, Vec<i64>> = Default::default();
    for (channel, order) in &links {
        by_channel.entry(*channel).or_default().push(*order);
    }
    for (channel, orders) in &by_channel {
        let dense: Vec<i64> = (0..orders.len() as i64).collect();
        assert_eq!(orders, &dense, "channel {channel} has a gap or a duplicate");
    }
    assert!(
        by_channel.values().any(|orders| orders.len() > 1),
        "no channel has a failover source to order"
    );

    // Re-running must be safe. `INSERT OR REPLACE` is a DELETE then an
    // INSERT, so a row replaced by a second import fires `ON DELETE CASCADE`
    // from `m3u_account` and `channel` and takes every stream and the whole
    // failover catalogue with it — which shows up as counts that fell.
    super::run_import(backup_source().await, &db)
        .await
        .expect("second import ran");
    for (table, expected) in [
        ("channel", 12),
        ("stream", 20),
        ("channel_stream", 17),
        ("program", 240),
        ("channel_profile_membership", 24),
        ("m3u_filter", 3),
        ("user", 2),
    ] {
        assert_eq!(
            count(&db, table).await,
            expected,
            "{table} after a second import"
        );
    }
}

/// The stored hash is sha256 over a JSON object of the hashed fields, which
/// is what a refresh recomputes to match rows. `sha256(url)` looks identical
/// in a diff and is wrong, and a wrong hash orphans the entire catalogue on
/// the first refresh after cutover.
#[tokio::test]
async fn imported_stream_hashes_are_the_sources_and_not_recomputed() {
    let (_dir, db, _) = imported().await;
    let keys = dollet_core::sync::hash::parse_keys("url");
    assert!(!keys.is_empty(), "the instance's m3u_hash_key is `url`");

    #[derive(sqlx::FromRow)]
    struct StreamRow {
        id: i64,
        name: String,
        url: String,
        tvg_id: Option<String>,
        m3u_account_id: Option<i64>,
        stream_hash: String,
    }
    let streams: Vec<StreamRow> = sqlx::query_as(
        "SELECT id, name, url, tvg_id, m3u_account_id, stream_hash FROM stream ORDER BY id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(streams.len(), 20);

    for stream in &streams {
        let identity = dollet_core::sync::hash::StreamIdentity {
            name: &stream.name,
            url: &stream.url,
            tvg_id: stream.tvg_id.as_deref().unwrap_or_default(),
            group: "",
            m3u_account_id: stream.m3u_account_id.unwrap_or_default(),
            account_type: dollet_core::domain::M3uAccountType::Standard,
            provider_stream_id: None,
        };
        assert_eq!(
            dollet_core::sync::hash::stream_hash(&identity, &keys),
            stream.stream_hash.as_str(),
            "stream {}",
            stream.id
        );
    }
}

/// The rows a clean install leaves empty, planted so their mappings are
/// proven rather than vacuous. `fixtures/import/README.md` lists them.
#[tokio::test]
async fn the_rows_a_clean_install_lacks_come_through() {
    let (_dir, db, _) = imported().await;

    // An override: every output query sorts and filters on the `effective_*`
    // coalesce of override over base, so one dropped in migration renumbers
    // and renames channels with nothing to show for it. The unset fields stay
    // NULL rather than becoming empty strings, which is what makes the
    // coalesce fall through to the base row.
    let (channel, name, number, tvg_id, group, logo, epg): (
        i64,
        String,
        String,
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT channel_id, name, CAST(channel_number AS TEXT), tvg_id, channel_group_id, \
         logo_id, epg_data_id FROM channel_override",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        (channel, name.as_str(), number.as_str(), tvg_id.as_str()),
        (171, "Overridden Name", "900.5", "override&<id>.us")
    );
    assert_eq!((group, logo, epg), (None, None, None));

    // A second user with everything the first leaves NULL. An API key that
    // does not survive is every client reconfigured by hand, and it would not
    // be noticed until one of them stopped working.
    #[derive(sqlx::FromRow)]
    struct UserRow {
        username: String,
        api_key: Option<String>,
        user_level: i64,
        stream_limit: Option<i64>,
        custom_properties: Option<String>,
    }
    let users: Vec<UserRow> = sqlx::query_as(
        "SELECT username, api_key, user_level, stream_limit, custom_properties FROM user \
         ORDER BY id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    let (admin, streamer) = (&users[0], &users[1]);
    assert_eq!(admin.username, "fixtureadmin");
    assert_eq!(
        admin.api_key, None,
        "the admin has no key, which is why the second user exists"
    );
    assert_eq!(admin.user_level, 10, "an admin");
    assert_eq!(streamer.username, "adversarial-streamer");
    assert_eq!(
        streamer.api_key.as_deref(),
        Some("fixture-api-key-0000000000000000000000")
    );
    assert_eq!(streamer.user_level, 0, "not an admin");
    assert_eq!(streamer.stream_limit, Some(3));
    assert!(
        streamer
            .custom_properties
            .as_deref()
            .unwrap_or_default()
            .contains("xc_password"),
        "the Xtream secret rides in custom_properties and is separate from the login"
    );

    // A channel profile with one member disabled. Creating a profile fills it
    // with every channel, and the import then writes the memberships the
    // source had; in the wrong order every deliberately-hidden channel comes
    // back enabled, which is a profile that silently stops hiding anything.
    let disabled: Vec<i64> = sqlx::query_scalar(
        "SELECT channel_id FROM channel_profile_membership \
         WHERE channel_profile_id = 900 AND enabled = 0",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(disabled, [173], "the source's disabled row won");
    let members: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM channel_profile_membership WHERE channel_profile_id = 900",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(members, 12, "and the profile still gained every channel");
    // The user's link to it, which is what scopes what they can see.
    let link: (i64, i64) =
        sqlx::query_as("SELECT user_id, channel_profile_id FROM user_channel_profile")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(link, (900, 900));

    // Stored patterns keep the operator's own text. Rust spells a
    // backreference `\1` where JavaScript spells it `$1`, and `sync::filters`
    // converts at compile time; rewriting the column would show the operator
    // a pattern they never typed. The one that will not compile is kept too —
    // an operator fixes it in the UI, which they cannot do if the import
    // dropped it.
    let patterns: Vec<String> =
        sqlx::query_scalar("SELECT regex_pattern FROM m3u_filter ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(patterns, ["(?<=US: )Meridian", r"(\w+) $1", "(unclosed"]);
    assert_eq!(
        dollet_core::regex_compat::js_backrefs_to_rust(&patterns[1]),
        r"(\w+) \1"
    );
}

/// The importer finishes and then says what it could not carry, rather than
/// aborting: a pattern that will not compile changes which streams a refresh
/// keeps, and the operator has to see it. The CLI exits non-zero on it, and
/// `--report` writes this before that exit.
#[tokio::test]
async fn the_report_names_what_it_could_not_carry() {
    let (_dir, _db, report) = imported().await;

    let failed: Vec<(&str, &str)> = report
        .regex_failures
        .iter()
        .map(|p| (p.source.as_str(), p.pattern.as_str()))
        .collect();
    assert_eq!(failed, [("m3u filter 902", "(unclosed")]);
    let rewritten: Vec<(&str, &str, &str)> = report
        .regex_rewritten
        .iter()
        .map(|p| (p.source.as_str(), p.pattern.as_str(), p.detail.as_str()))
        .collect();
    assert_eq!(
        rewritten,
        [(
            "m3u filter 901",
            r"(\w+) $1",
            r"JS backreference rewritten as `(\w+) \1`"
        )]
    );

    // An operator's 90s retention and their ffmpeg-by-default profile both
    // change behaviour under this build, and both are said out loud rather
    // than silently applied.
    assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    assert!(
        report.warnings.iter().any(|w| w.contains("ring_seconds")),
        "{:?}",
        report.warnings
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("ffmpeg")),
        "{:?}",
        report.warnings
    );

    // What the zip said about itself reaches the report, so a run that went
    // wrong says which backup it came from.
    let backup = report.backup.expect("the metadata reached the report");
    assert_eq!(backup.format.as_deref(), Some("dispatcharr-backup"));
    assert_eq!(backup.version, Some(2));
    assert_eq!(
        backup.created_at.as_deref(),
        Some("2026-09-11T03:00:00.139910+00:00")
    );
}

/// Not "the string survived" — that a hash carries across as text says
/// nothing. What the migration has to preserve is the ability to log in, and
/// every user's API access hangs off the same row. Slow, at Django's 1.2
/// million iterations per verification, which is why it stands alone.
#[tokio::test]
async fn the_imported_passwords_still_verify() {
    let (_dir, db, _) = imported().await;
    let users: Vec<(String, String)> =
        sqlx::query_as("SELECT username, password FROM user ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(users.len(), 2);

    for (username, encoded) in &users {
        assert!(
            dollet_core::auth::password::is_supported(encoded),
            "{username} has a hash this build cannot verify"
        );
        assert!(
            dollet_core::auth::password::verify("ipx-test-password", encoded).unwrap(),
            "{username} cannot log in after the migration"
        );
    }
}

/// A backup whose container version this build has not seen is refused, and
/// nothing is written.
///
/// The format can change without notice, and the importer runs once against
/// someone's only copy of their data, so the failure to prevent is a
/// half-right instance rather than a refusal. `KNOWN_BACKUP_VERSION` is where
/// a new format gets understood and admitted.
#[tokio::test]
async fn a_backup_from_an_unknown_format_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = dollet_core::db::connect(&dir.path().join("dollet.sqlite"))
        .await
        .unwrap();
    dollet_core::db::migrate(&db).await.unwrap();

    let zipped = dir.path().join("backup.zip");
    let dump = fixture_dump();
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&zipped).unwrap());
    let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
    zip.start_file("database.dump", options).unwrap();
    std::io::Write::write_all(&mut zip, &dump).unwrap();
    zip.start_file("metadata.json", options).unwrap();
    std::io::Write::write_all(
        &mut zip,
        br#"{"format": "dispatcharr-backup", "version": 99, "database_file": "database.dump"}"#,
    )
    .unwrap();
    zip.finish().unwrap();

    // `{:#}` rather than `to_string`: the refusal is the cause, under the
    // context naming the file it came from.
    let error = match super::ImportSource::backup(&zipped).await {
        Err(e) => format!("{e:#}"),
        Ok(_) => panic!("a version this build has not seen must not import"),
    };
    assert!(error.contains("version 99"), "{error}");
    assert!(error.contains("version 2 only"), "{error}");

    // Refused before anything was written, not part-way through.
    let channels: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(channels, 0);
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/import")
        .join(name)
}

/// The `database.dump` inside the fixture zip, for a test that wraps it in a
/// zip of its own.
fn fixture_dump() -> Vec<u8> {
    let file = std::fs::File::open(fixture_path("dispatcharr-backup.zip")).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();
    let mut dump = Vec::new();
    std::io::Read::read_to_end(&mut zip.by_name("database.dump").unwrap(), &mut dump).unwrap();
    dump
}

// --- DOLLET_IMPORT_BACKUP ----------------------------------------------------

fn boot_config(dir: &TempDir, backup: Option<PathBuf>) -> Config {
    let mut config = crate::test_support::config(dir.path(), TrustedProxies::None);
    config.import_backup = backup;
    config
}

async fn fresh_instance(dir: &TempDir) -> sqlx::SqlitePool {
    crate::test_support::migrated_db(dir.path()).await
}

async fn users(db: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM user")
        .fetch_one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn no_import_backup_means_no_import() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_instance(&dir).await;

    super::maybe_import_on_boot(&db, &boot_config(&dir, None))
        .await
        .expect("a boot with nothing to do");

    assert_eq!(users(&db).await, 0);
    assert!(!dir.path().join("import-report.json").exists());
}

#[tokio::test]
async fn an_import_backup_is_ignored_once_the_instance_has_users() {
    // The gate is users rather than the file, so an operator who left the
    // variable in their compose file cannot overwrite the instance they have
    // been running for a month by restarting it.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_instance(&dir).await;
    sqlx::raw_sql(
        "INSERT INTO user (id, username, password, is_active, user_level, date_joined)
         VALUES (1, 'somebody', 'x', 1, 10, '2026-09-14 00:00:00')",
    )
    .execute(&db)
    .await
    .unwrap();

    let config = boot_config(&dir, Some(fixture_path("dispatcharr-backup.zip")));
    super::maybe_import_on_boot(&db, &config)
        .await
        .expect("a boot that ignores the variable");

    assert_eq!(users(&db).await, 1, "the existing user was replaced");
    let channels: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(channels, 0, "the backup was imported anyway");
    assert!(!dir.path().join("import-report.json").exists());
}

#[tokio::test]
async fn an_import_backup_that_cannot_be_read_stops_the_boot() {
    // Serving an empty instance to someone who asked for their data back is
    // the confusing failure; refusing to start is the one they can act on.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_instance(&dir).await;

    let missing = dir.path().join("nowhere.zip");
    let error = super::maybe_import_on_boot(&db, &boot_config(&dir, Some(missing.clone())))
        .await
        .expect_err("a missing backup must stop the boot");
    assert!(format!("{error:#}").contains("nowhere.zip"), "{error:#}");

    let garbage = dir.path().join("not-a-backup.zip");
    std::fs::write(&garbage, b"this is not a backup").unwrap();
    let error = super::maybe_import_on_boot(&db, &boot_config(&dir, Some(garbage)))
        .await
        .expect_err("an unreadable backup must stop the boot");
    assert!(
        format!("{error:#}").contains("neither a pg_dump archive nor a zip"),
        "{error:#}"
    );
}

#[tokio::test]
async fn the_first_boot_imports_the_backup_and_the_next_one_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_instance(&dir).await;
    let config = boot_config(&dir, Some(fixture_path("dispatcharr-backup.zip")));

    // The corpus carries a pattern that will not compile, which is a non-zero
    // exit for the CLI and deliberately *not* an error here: the rows are
    // imported, and this process's job is to serve them.
    super::maybe_import_on_boot(&db, &config)
        .await
        .expect("the first boot imports");

    assert_eq!(users(&db).await, 2);
    let channels: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(channels, 12);

    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("import-report.json")).expect("a report"),
    )
    .expect("the report is JSON");
    assert_eq!(report["counts"]["channel"], 12);
    assert_eq!(report["regex_failures"][0]["pattern"], "(unclosed");

    // Now that there are users, a restart is a no-op — which is what lets the
    // variable stay in the compose file.
    std::fs::remove_file(dir.path().join("import-report.json")).unwrap();
    super::maybe_import_on_boot(&db, &config)
        .await
        .expect("the second boot");
    assert!(!dir.path().join("import-report.json").exists());
}

// --- Public outputs: HDHR, M3U, XMLTV, Xtream --------------------------------

/// axum resolves by specificity and panics at startup on a conflicting insert,
/// rather than resolving top-down. The bare three-segment Xtream route is the
/// one that can collide with the SPA catch-all, and it only does so once both
/// are in the same router — which is what `TestApp` builds.
#[tokio::test]
async fn the_bare_xtream_route_does_not_shadow_the_spa() {
    let app = TestApp::new().await;

    // Three segments reach Xtream, which answers bad credentials with a 404
    // page rather than falling through to the SPA.
    let (status, _, body) = app.raw("/nobody/wrong/171").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("<title>Not Found</title>"), "{body}");

    // Two and four segments fall through to the SPA, which serves whatever
    // the frontend build left behind — the point is that Xtream did not claim
    // them and answer with its own 404 page.
    for uri in ["/settings", "/a/b/c/d"] {
        let (_, _, body) = app.raw(uri).await;
        assert!(
            !body.contains("<title>Not Found</title>"),
            "{uri} was claimed by the Xtream route"
        );
    }
}

#[tokio::test]
async fn hdhr_serves_all_thirteen_paths() {
    let app = TestApp::new().await;

    let paths = [
        "/hdhr/discover.json",
        "/hdhr/lineup.json",
        "/hdhr/lineup_status.json",
        "/hdhr/All/discover.json",
        "/hdhr/All/lineup.json",
        "/hdhr/All/lineup_status.json",
        "/hdhr/output_profile/1/discover.json",
        "/hdhr/output_profile/1/lineup.json",
        "/hdhr/output_profile/1/lineup_status.json",
        "/hdhr/All/output_profile/1/discover.json",
        "/hdhr/All/output_profile/1/lineup.json",
        "/hdhr/All/output_profile/1/lineup_status.json",
    ];
    for path in paths {
        let (status, _) = app.public(path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }

    let (status, _, xml) = app.raw("/hdhr/device.xml").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        xml.contains("<BaseURL>http://ipx.test:9191/hdhr</BaseURL>"),
        "{xml}"
    );
}

/// Every lineup URL is absolute. Getting the origin wrong fails as "discovery
/// works, playback doesn't", which is the most confusing way this can break.
#[tokio::test]
async fn the_lineup_matches_the_golden_corpus_and_its_urls_are_absolute() {
    let app = TestApp::new().await;

    let (status, lineup) = app.public("/hdhr/lineup.json").await;
    assert_eq!(status, StatusCode::OK);

    let entries = lineup.as_array().unwrap();
    // 17 rows, less the hidden one and the one with no number: HDHomeRun
    // addresses a channel by `GuideNumber`, so a numberless entry is unplayable.
    assert_eq!(entries.len(), 15);
    assert_eq!(entries[0]["GuideNumber"], "1");
    assert_eq!(
        entries[0]["GuideName"],
        fixture_name(&app.state.db, "channel", 171).await
    );
    assert_eq!(
        entries[0]["URL"],
        "http://ipx.test:9191/proxy/ts/stream/91af9876-76cc-4bd1-bb7e-0d16a5af10a1"
    );

    // An output profile in the path reaches the stream URLs.
    let (_, scoped) = app.public("/hdhr/output_profile/1/lineup.json").await;
    assert!(
        scoped[0]["URL"]
            .as_str()
            .unwrap()
            .ends_with("?output_profile=1"),
        "{}",
        scoped[0]["URL"]
    );

    // One that does not exist falls back to no transcoding rather than
    // pointing every entry at a profile the stream endpoint will ignore.
    let (_, unknown) = app.public("/hdhr/output_profile/99/lineup.json").await;
    assert_eq!(unknown, lineup);

    // A channel profile that does not exist yields an empty lineup, not a 404:
    // Plex treats a 404 as a dead tuner and stops asking.
    let (status, missing) = app.public("/hdhr/NoSuchProfile/lineup.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(missing, serde_json::json!([]));
}

/// `stream_settings.hdhr_output_profile_id` is the fallback for the bare
/// `/hdhr/` tuner — the one a Plex installation already has configured — so it
/// has to reach the lineup URLs.
#[tokio::test]
async fn the_hdhr_lineup_falls_back_to_the_configured_output_profile() {
    let app = TestApp::new().await;
    let token = app.login().await;

    // The snapshot has it null, so the plain lineup is untouched until it is
    // set.
    let (_, before) = app.public("/hdhr/lineup.json").await;
    assert!(
        !before[0]["URL"]
            .as_str()
            .unwrap()
            .contains("output_profile"),
        "{}",
        before[0]["URL"]
    );

    let (status, saved) = app
        .json(
            "PATCH",
            "/api/core/settings/stream_settings/",
            &token,
            Some(json!({ "value": { "hdhr_output_profile_id": 2 } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    let (_, lineup) = app.public("/hdhr/lineup.json").await;
    for entry in lineup.as_array().unwrap() {
        assert!(
            entry["URL"]
                .as_str()
                .unwrap()
                .ends_with("?output_profile=2"),
            "{}",
            entry["URL"]
        );
    }

    // The path form is the more specific request and still wins.
    let (_, scoped) = app.public("/hdhr/output_profile/1/lineup.json").await;
    assert!(
        scoped[0]["URL"]
            .as_str()
            .unwrap()
            .ends_with("?output_profile=1"),
        "{}",
        scoped[0]["URL"]
    );

    // And a setting naming a profile that no longer exists is dropped rather
    // than pointing every entry at something the stream endpoint will ignore.
    let (status, _) = app
        .json(
            "PATCH",
            "/api/core/settings/stream_settings/",
            &token,
            Some(json!({ "value": { "hdhr_output_profile_id": 404 } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (_, dropped) = app.public("/hdhr/lineup.json").await;
    assert_eq!(dropped, before);
}

#[tokio::test]
async fn forwarding_headers_rewrite_the_advertised_origin_only_from_a_trusted_peer() {
    let mut app = TestApp::new().await;
    app.trust_proxies(TrustedProxies::PrivateAndLoopback);

    let forwarded = |peer: &str| {
        let peer: SocketAddr = peer.parse().unwrap();
        let mut request = Request::builder()
            .method("GET")
            .uri("/hdhr/discover.json")
            .header("host", "ipx.test:9191")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "tv.example")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        request
    };

    // 127.0.0.1 is a trusted proxy under the configured policy.
    let (status, trusted) = app.send_raw(forwarded("127.0.0.1:40000")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trusted["BaseURL"], "https://tv.example/hdhr");

    // A public peer is not, so its headers rewrite nothing.
    let (_, untrusted) = app.send_raw(forwarded("198.51.100.7:40000")).await;
    assert_eq!(untrusted["BaseURL"], "http://ipx.test:9191/hdhr");
}

// --- `/api/core/origins/` ----------------------------------------------------
//
// The recorded map is process-global and every test in this binary that touches
// `/hdhr/` or `/output/` writes to it in parallel, so these assert only their
// own entries and give each one a `Host` nothing else uses. The eviction rule
// is exercised against a private map in `origin.rs`, where it can be
// deterministic.

/// The recorded entry for `base_url`, or a failure naming what was there.
async fn seen_origin(app: &TestApp, token: &str, base_url: &str) -> Value {
    let (status, body) = app.json("GET", "/api/core/origins/", token, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    rows(&body)
        .iter()
        .find(|entry| entry["base_url"] == base_url)
        .cloned()
        .unwrap_or_else(|| {
            let seen: Vec<&str> = rows(&body)
                .iter()
                .filter_map(|entry| entry["base_url"].as_str())
                .collect();
            panic!("{base_url} was not recorded; the list holds {seen:?}")
        })
}

#[tokio::test]
async fn asking_for_a_lineup_records_the_address_it_was_asked_on() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, _) = app
        .from("127.0.0.1:40000")
        .header("host", "origins-lineup.test:9191")
        .get("/hdhr/lineup.json")
        .await;
    assert_eq!(status, StatusCode::OK);

    let entry = seen_origin(&app, &token, "http://origins-lineup.test:9191").await;
    assert_eq!(entry["kinds"], json!(["hdhr"]));
    assert_eq!(entry["requests"], 1);
    assert_eq!(entry["first_seen"], entry["last_seen"]);
    let first_seen = entry["first_seen"].clone();

    // The playlist and the guide are separate kinds on the same address, and
    // neither starts a second entry.
    for uri in ["/output/m3u", "/output/epg"] {
        let (status, _) = app
            .from("127.0.0.1:40000")
            .header("host", "origins-lineup.test:9191")
            .get(uri)
            .await;
        assert_eq!(status, StatusCode::OK);
    }

    let entry = seen_origin(&app, &token, "http://origins-lineup.test:9191").await;
    assert_eq!(entry["kinds"], json!(["epg", "hdhr", "m3u"]));
    assert_eq!(entry["requests"], 3);
    // How long this address has been in use, so a repeat request must not move
    // it — `last_seen` is the field that tracks the latest request.
    assert_eq!(entry["first_seen"], first_seen);
    assert_ne!(entry["last_seen"], first_seen);
}

/// The point of recording after the `Origin` extractor rather than off the
/// request line: what the page shows is what the lineup advertised, forwarding
/// rules and all.
#[tokio::test]
async fn a_forwarded_host_is_recorded_only_when_the_peer_is_trusted() {
    let mut app = TestApp::new().await;
    let token = app.login().await;

    async fn ask(app: &TestApp) -> Value {
        let (status, body) = app
            .from("127.0.0.1:40000")
            .header("host", "origins-direct.test:9191")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "origins-proxied.test")
            .get("/hdhr/discover.json")
            .await;
        assert_eq!(status, StatusCode::OK);
        body
    }

    // Nothing is a proxy under the shipped policy, so the peer's own view is
    // what every URL was built from and what gets recorded.
    let body = ask(&app).await;
    assert_eq!(body["BaseURL"], "http://origins-direct.test:9191/hdhr");
    seen_origin(&app, &token, "http://origins-direct.test:9191").await;

    app.trust_proxies(TrustedProxies::PrivateAndLoopback);
    let body = ask(&app).await;
    assert_eq!(body["BaseURL"], "https://origins-proxied.test/hdhr");

    let entry = seen_origin(&app, &token, "https://origins-proxied.test").await;
    assert_eq!(entry["kinds"], json!(["hdhr"]));
}

#[tokio::test]
async fn a_configured_base_url_overrides_everything() {
    let mut app = TestApp::new().await;
    app.set_advertised_base_url(Some("https://tv.example:8443".into()));

    let (_, discover) = app.public("/hdhr/discover.json").await;
    assert_eq!(discover["BaseURL"], "https://tv.example:8443/hdhr");
}

#[tokio::test]
async fn the_playlist_and_guide_match_the_golden_shape() {
    let app = TestApp::new().await;

    let (status, headers, playlist) = app.raw("/output/m3u").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "audio/x-mpegurl");
    assert!(playlist.starts_with(
        "#EXTM3U x-tvg-url=\"http://ipx.test:9191/output/epg\" \
         url-tvg=\"http://ipx.test:9191/output/epg\"\n"
    ));
    // Logos point at this server's cache by default, not the provider's CDN.
    assert!(playlist.contains("tvg-logo=\"http://ipx.test:9191/api/channels/logos/20/cache/\""));
    assert_eq!(playlist.matches("#EXTINF").count(), 16);

    let (_, _, raw_logos) = app.raw("/output/m3u?cachedlogos=false").await;
    let provider_url = fixture_logo_url(&app.state.db, 20).await;
    assert!(
        raw_logos.contains(&format!("tvg-logo=\"{provider_url}\"")),
        "the provider's own URL is not in the uncached playlist"
    );
    assert!(
        raw_logos
            .starts_with("#EXTM3U x-tvg-url=\"http://ipx.test:9191/output/epg?cachedlogos=false\"")
    );

    let (status, headers, guide) = app.raw("/output/epg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/xml");
    assert!(guide.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tv "));
    assert_eq!(guide.matches("<channel id=").count(), 16);
    assert!(guide.trim_end().ends_with("</tv>"));

    // The fixture's programmes are historical, and a guide carries the future,
    // so one has to exist now for the programme path to be exercised at all.
    sqlx::query(
        "INSERT INTO program (epg_data_id, tvg_id, start_time, end_time, title)
         SELECT epg_data_id, 'now', ?, ?, 'On Right Now' FROM channel WHERE id = 171",
    )
    .bind(dollet_core::db::sql_timestamp(chrono::Utc::now()))
    .bind(dollet_core::db::sql_timestamp(
        chrono::Utc::now() + chrono::Duration::hours(1),
    ))
    .execute(&app.state.db)
    .await
    .unwrap();

    let (_, _, fresh) = app.raw("/output/epg?days=1").await;
    assert!(fresh.contains("<title>On Right Now</title>"), "{fresh}");
}

#[tokio::test]
async fn a_named_profile_scopes_the_outputs_and_an_unknown_one_is_a_404() {
    let app = TestApp::new().await;

    let (status, _, playlist) = app.raw("/output/m3u/All").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(playlist.matches("#EXTINF").count(), 16);

    // Unlike the HDHR lineup: a client asking for a named playlist has been
    // misconfigured, and an empty one looks like "no channels today".
    let (status, _, _) = app.raw("/output/m3u/NoSuchProfile").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_guide_is_served_from_cache_on_the_second_request() {
    let app = TestApp::new().await;

    let (_, _, first) = app.raw("/output/epg").await;
    let cached: Vec<_> = std::fs::read_dir(app.state.config.cache_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    assert!(!cached.is_empty(), "nothing was written to the cache");

    // Proves the second response came off disk rather than being regenerated.
    std::fs::write(&cached[0], "<tv>from the cache</tv>").unwrap();
    let (_, _, second) = app.raw("/output/epg").await;
    assert_ne!(first, second);
    assert_eq!(second, "<tv>from the cache</tv>");
}

/// The rendered artefacts of one output sitting in the cache directory.
///
/// Recognised the way `outputs` names them, so a test can tell "the cache was
/// dropped" apart from "the cache was never written" — and so that neither the
/// downloaded provider feeds nor the artwork cache, which share the directory,
/// are ever counted as ours.
fn cached_outputs(app: &TestApp, prefix: &str, extension: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(app.state.config.cache_dir()) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
                && path.extension().is_some_and(|found| found == extension)
        })
        .collect()
}

/// A mapping change has to reach the guide at once: Plex caches what it
/// fetches, so serving a file rendered before the mapping for the rest of its
/// TTL is a stale guide for hours.
#[tokio::test]
async fn the_guide_reflects_a_new_mapping_without_waiting_out_the_cache() {
    let app = TestApp::synthetic().await;
    let admin = app.login_as(Principal::Admin).await;

    // Renders and caches. Channel 1002 maps to no guide data, so it publishes a
    // `<channel>` and no listings.
    let (_, before) = guide_of(&app, "/output/epg").await;
    assert!(
        !before.iter().any(|(channel, _, _)| channel == "1002"),
        "channel 1002 already had listings"
    );
    assert_eq!(cached_outputs(&app, "epg-", "xml").len(), 1);

    let (status, body) = app
        .request_as(
            &admin,
            "PATCH",
            "/api/channels/channels/1002/",
            Some(json!({ "epg_data_id": 1001 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        cached_outputs(&app, "epg-", "xml").is_empty(),
        "the write left the pre-mapping guide on disk"
    );

    // Immediately, with no clock wound past `CACHE_TTL`.
    let (_, after) = guide_of(&app, "/output/epg").await;
    let titles: Vec<&str> = after
        .iter()
        .filter(|(channel, _, _)| channel == "1002")
        .map(|(_, title, _)| title.as_str())
        .collect();
    assert!(
        titles.contains(&"Synth On Now"),
        "the guide still predates the mapping: {titles:?}"
    );
}

/// A rename moves the playlist's entry and the guide's `<display-name>`, which
/// is why a channel write drops both.
#[tokio::test]
async fn the_playlist_and_guide_reflect_a_rename_without_waiting_out_the_cache() {
    let app = TestApp::synthetic().await;
    let admin = app.login_as(Principal::Admin).await;

    let (_, _, playlist) = app.raw("/output/m3u").await;
    let (_, _, guide) = app.raw("/output/epg").await;
    assert!(playlist.contains("Synth One"), "{playlist}");
    assert!(guide.contains("Synth One"));
    assert_eq!(cached_outputs(&app, "m3u-", "m3u").len(), 1);
    assert_eq!(cached_outputs(&app, "epg-", "xml").len(), 1);

    let (status, body) = app
        .request_as(
            &admin,
            "PATCH",
            "/api/channels/channels/1000/",
            Some(json!({ "name": "Synth Renamed" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, _, playlist) = app.raw("/output/m3u").await;
    assert!(
        playlist.contains("Synth Renamed") && !playlist.contains("Synth One"),
        "the playlist still carries the old name"
    );
    let (_, _, guide) = app.raw("/output/epg").await;
    assert!(
        guide.contains("Synth Renamed") && !guide.contains("Synth One"),
        "the guide still carries the old name"
    );
}

#[tokio::test]
async fn accepting_a_guide_suggestion_re_renders_the_guide() {
    let app = TestApp::synthetic().await;
    let admin = app.login_as(Principal::Admin).await;

    // The seeded suggestion names guide data the feed carried no programmes
    // for, so accepting it changes nothing a client could see. Pointed at the
    // sports guide, the decision has the consequence the operator expects.
    sqlx::query("UPDATE epg_match_suggestion SET epg_data_id = 1001 WHERE channel_id = 1002")
        .execute(&app.state.db)
        .await
        .unwrap();

    let (_, before) = guide_of(&app, "/output/epg").await;
    assert!(!before.iter().any(|(channel, _, _)| channel == "1002"));

    let (status, body) = app
        .request_as(&admin, "POST", "/api/epg/suggestions/1002/", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, after) = guide_of(&app, "/output/epg").await;
    assert!(
        after
            .iter()
            .any(|(channel, title, _)| channel == "1002" && title == "Synth On Now"),
        "accepting the suggestion never reached the guide"
    );
}

/// A feed for one guide channel nothing in the lineup maps to.
const UNMAPPED_GUIDE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="nobody.watches"><display-name>Unmapped</display-name></channel>
  <programme start="20260912130000 +0000" stop="20260912140000 +0000" channel="nobody.watches">
    <title>Unmapped Programme</title>
  </programme>
</tv>
"#;

#[tokio::test]
async fn an_epg_refresh_that_changed_the_guide_drops_the_rendered_one() {
    let app = TestApp::new().await;
    let mapped = a_mapped_tvg_id(&app).await;
    let source = source_from_file(&app, &guide_xml(&mapped)).await;
    let handle = super::jobs::test_handle(&app.state, "epg_refresh:1");

    let (status, _, _) = app.raw("/output/epg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cached_outputs(&app, "epg-", "xml").len(), 1);

    // This feed carries nothing for the guide channels the fixture's lineup
    // maps, so the refresh clears their listings.
    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .unwrap();
    assert!(
        cached_outputs(&app, "epg-", "xml").is_empty(),
        "the refresh left the pre-refresh guide on disk"
    );
}

/// The guard on that invalidation is real, not decorative: a scheduled refresh
/// of a feed the provider has not republished promotes nothing, and making every
/// client rescan the programme table for identical bytes is the cost the cache
/// exists to avoid.
#[tokio::test]
async fn an_epg_refresh_that_promotes_nothing_leaves_the_rendered_guide_alone() {
    let app = TestApp::new().await;
    // A feed for a guide channel the lineup maps to nothing: refreshing it
    // stores guide data and promotes not one programme. `GUIDE` would not do —
    // it carries a programme for a channel this fixture does map, and rewriting
    // a row with its own contents is still a write this cannot tell from a
    // change.
    let source = source_from_file(&app, UNMAPPED_GUIDE).await;
    let handle = super::jobs::test_handle(&app.state, "epg_refresh:1");

    // The first run is the one that changes something — it clears the listings
    // this feed does not carry — and the guide is rendered after it, so the
    // second run has nothing left to promote or clear.
    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .unwrap();
    let (status, _, _) = app.raw("/output/epg").await;
    assert_eq!(status, StatusCode::OK);
    let cached = cached_outputs(&app, "epg-", "xml");
    assert_eq!(cached.len(), 1, "the guide was not cached: {cached:?}");

    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .unwrap();
    assert!(
        cached[0].exists(),
        "a refresh that changed nothing dropped the cached guide"
    );

    // And the downloaded feed beside it is nobody's to delete.
    assert!(
        app.state.config.cache_dir().join("feed.xml").exists(),
        "the provider feed was swept up with the outputs"
    );
}

#[tokio::test]
async fn xtream_authenticates_on_its_own_password_and_serves_the_live_actions() {
    let app = TestApp::new().await;
    let credentials = "username=fixtureadmin&password=fixturepass";

    let (status, account) = app.public(&format!("/player_api.php?{credentials}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(account["user_info"]["username"], "fixtureadmin");
    assert_eq!(account["user_info"]["auth"], 1);
    assert_eq!(account["server_info"]["url"], "ipx.test");
    // The advertised port, not the one off the Host header — read from there,
    // a portless request would report "80" while every URL beside it carries
    // :9191.
    assert_eq!(account["server_info"]["port"], "9191");

    let (_, streams) = app
        .public(&format!(
            "/player_api.php?{credentials}&action=get_live_streams"
        ))
        .await;
    let streams = streams.as_array().unwrap();
    assert_eq!(streams.len(), 16);
    assert_eq!(streams[0]["stream_id"], 171);
    assert_eq!(streams[0]["num"], 1);

    let (_, categories) = app
        .public(&format!(
            "/player_api.php?{credentials}&action=get_live_categories"
        ))
        .await;
    let categories = categories.as_array().unwrap();
    // Lineup order, and only groups that have channels.
    assert_eq!(
        categories[0]["category_name"],
        fixture_name(&app.state.db, "channel_group", 6).await
    );
    assert!(
        !categories
            .iter()
            .any(|c| c["category_name"] == "Default Group"),
        "an empty category was advertised"
    );

    let (_, filtered) = app
        .public(&format!(
            "/player_api.php?{credentials}&action=get_live_streams&category_id=3"
        ))
        .await;
    let filtered = filtered.as_array().unwrap();
    assert!(!filtered.is_empty() && filtered.len() < 16);
    assert!(filtered.iter().all(|s| s["category_id"] == "3"));

    // VOD and series are out of scope and answer empty rather than failing: a
    // client treats a failed catalogue call as a broken account.
    for action in ["get_vod_categories", "get_vod_streams", "get_series"] {
        let (status, body) = app
            .public(&format!("/player_api.php?{credentials}&action={action}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{action}");
        assert_eq!(body, serde_json::json!([]), "{action}");
    }

    let (status, _, panel) = app.raw(&format!("/panel_api.php?{credentials}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(panel.contains("\"user_info\""));

    let (status, headers, playlist) = app
        .raw(&format!("/get.php?{credentials}&type=m3u_plus"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "audio/x-mpegurl");
    assert_eq!(playlist.matches("#EXTINF").count(), 16);

    let (status, _, guide) = app.raw(&format!("/xmltv.php?{credentials}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(guide.starts_with("<?xml"));
}

#[tokio::test]
async fn xtream_refuses_the_login_password_and_anything_else() {
    let app = TestApp::new().await;

    // The account password must not work here: an Xtream URL carries the
    // secret in the query string, where it lands in every proxy log.
    for query in [
        "username=fixtureadmin&password=ipx-test-password",
        "username=fixtureadmin&password=",
        "username=nobody&password=fixturepass",
        "",
    ] {
        let (status, _, body) = app.raw(&format!("/player_api.php?{query}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{query}");
        assert!(body.contains("Not Found"), "{query}");
    }
}

#[tokio::test]
async fn the_stream_endpoint_resolves_a_channel_and_redirects_when_asked() {
    let app = TestApp::new().await;
    let uuid = "91af9876-76cc-4bd1-bb7e-0d16a5af10a1";

    let (status, _) = app
        .public("/proxy/ts/stream/00000000-0000-0000-0000-000000000000")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an unknown uuid must 404");

    // The locked `redirect` profile takes this server out of the data path
    // entirely, so no session is opened and no upstream is contacted.
    sqlx::query("UPDATE channel SET stream_profile_id = 4 WHERE uuid = ?")
        .bind(uuid)
        .execute(&app.state.db)
        .await
        .unwrap();

    let (status, headers, _) = app.raw(&format!("/proxy/ts/stream/{uuid}")).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert!(
        headers["location"]
            .to_str()
            .unwrap()
            .starts_with("https://provider.example/live/"),
        "{:?}",
        headers["location"]
    );

    // A channel with no streams is a client error, not a 500.
    sqlx::query("DELETE FROM channel_stream WHERE channel_id = 171")
        .execute(&app.state.db)
        .await
        .unwrap();
    let (status, _) = app.public(&format!("/proxy/ts/stream/{uuid}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// These endpoints carry no credentials — Plex fetches them with nothing but a
/// URL — so the allowlist is the only control in front of the whole channel
/// inventory and the stream URLs that play it.
#[tokio::test]
async fn the_unauthenticated_outputs_are_behind_the_network_allowlist() {
    let app = TestApp::new().await;
    let token = app.login().await;

    // A channel that does not exist, deliberately. The gate runs before the
    // lookup, so an allowed request answers 404 and a blocked one 403 — which
    // is the whole distinction under test, without opening a real session in
    // the process-global registry that another test would then count.
    let stream = "/proxy/ts/stream/aaaaaaaa-0000-4000-8000-0000000000ff";
    let xtream = "/player_api.php?username=fixtureadmin&password=fixturepass";

    // Every class, and every class checked against the other classes' paths
    // too: a test of one class alone passes just as well when that class is
    // enforced nowhere.
    let classes: [(&str, Vec<&str>); 3] = [
        (
            "M3U_EPG",
            vec![
                "/hdhr/lineup.json",
                "/output/m3u",
                "/output/epg",
                "/hdhr/device.xml",
            ],
        ),
        ("STREAMS", vec![stream]),
        ("XC_API", vec![xtream]),
    ];

    for (class, paths) in &classes {
        // Open to begin with.
        for path in paths {
            assert_ne!(
                app.raw(path).await.0,
                StatusCode::FORBIDDEN,
                "{class} {path} before any allowlist"
            );
        }

        let (status, _) = app
            .json(
                "PATCH",
                "/api/core/settings/network_access/",
                &token,
                // A range the test peer (127.0.0.1) is not in.
                Some(json!({ "value": { *class: "10.0.0.0/8" } })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);

        for path in paths {
            assert_eq!(
                app.raw(path).await.0,
                StatusCode::FORBIDDEN,
                "{class} does not gate {path}"
            );
        }

        // And every other class's paths still answer, so a restriction on one
        // class cannot be passing by locking everything.
        for (other, others) in &classes {
            if other == class {
                continue;
            }
            for path in others {
                assert_ne!(
                    app.raw(path).await.0,
                    StatusCode::FORBIDDEN,
                    "{class} restricted {other}'s {path}"
                );
            }
        }

        // Cleared before the next class, so each is tested alone.
        let (status, _) = app
            .json(
                "PATCH",
                "/api/core/settings/network_access/",
                &token,
                Some(json!({ "value": { *class: "" } })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
}

#[tokio::test]
async fn the_websocket_requires_an_access_token_in_the_subprotocol() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let handshake = |protocols: Option<&str>| {
        let mut builder = Request::builder()
            .method("GET")
            .uri("/ws")
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        if let Some(protocols) = protocols {
            builder = builder.header("sec-websocket-protocol", protocols);
        }
        let mut request = builder.body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(PEER.parse::<SocketAddr>().unwrap()));
        request
    };

    // No protocols at all, a marker with nothing after it, and a marker
    // followed by something that is not a token.
    for protocols in [None, Some("auth.jwt"), Some("auth.jwt, not-a-token")] {
        let response = app
            .router
            .clone()
            .oneshot(handshake(protocols))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{protocols:?} got past authentication"
        );
    }

    // A real handshake needs a real connection, which `oneshot` does not have:
    // the upgrade is refused for want of transport, *after* the token was
    // accepted. That boundary is the thing worth asserting here — the
    // subprotocol parsing itself is covered in `ws`'s own tests.
    let response = app
        .router
        .clone()
        .oneshot(handshake(Some(&format!("auth.jwt, {token}"))))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("no upgrade state"),
        "a valid token was rejected for some other reason"
    );
}

#[tokio::test]
async fn live_session_stats_are_admin_only() {
    let app = TestApp::new().await;
    let admin = app.login().await;

    let (status, stats) = app.json("GET", "/api/proxy/stats/", &admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats, serde_json::json!([]));

    assert_eq!(
        app.anonymous("GET", "/api/proxy/stats/").await.0,
        StatusCode::UNAUTHORIZED
    );
}

// --- Ingest -----------------------------------------------------------------

/// A provider, served locally, so the refresh path can be exercised end to end
/// without reaching the internet.
struct FakeProvider {
    server: wiremock::MockServer,
}

impl FakeProvider {
    async fn new() -> Self {
        Self {
            server: wiremock::MockServer::start().await,
        }
    }

    async fn serve(&self, path: &str, content_type: &str, body: &str) {
        use wiremock::matchers::{method, path as path_matcher};
        wiremock::Mock::given(method("GET"))
            .and(path_matcher(path))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw(body.to_owned().into_bytes(), content_type),
            )
            .mount(&self.server)
            .await;
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.server.uri())
    }
}

/// Provider fetches cannot reach loopback — which is also why the refresh tests
/// below drive the reconciler against a file rather than over HTTP.
///
/// `http::get` is what closes this, not `client()` alone: reqwest dials a bare
/// address without resolving it, so the guarded resolver never runs. Pinned so
/// nobody "simplifies" the call sites back to `client.get(..).send()`.
#[tokio::test]
async fn provider_fetches_cannot_reach_loopback() {
    let provider = FakeProvider::new().await;
    provider
        .serve("/epg.xml", "application/xml", "<tv></tv>")
        .await;
    let url = provider.url("/epg.xml");

    let client = dollet_core::http::client("dollet-test", false).unwrap();
    assert!(
        dollet_core::http::get(&client, &url, false).await.is_err(),
        "a provider fetch reached loopback"
    );

    // And the LAN is refused too, because an EPG or artwork URL is
    // configuration whose *content* the provider controls.
    assert!(dollet_core::http::check_url("http://192.168.1.50/epg.xml", false).is_err());
}

/// Point a source at a file on disk. A supported configuration, and the
/// only way to exercise the refresh without a reachable provider.
async fn source_from_file(app: &TestApp, body: &str) -> dollet_core::domain::EpgSource {
    let path = app.state.config.cache_dir().join("feed.xml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();

    let mut source = dollet_core::db::epg::get_source(&app.state.db, 1)
        .await
        .unwrap()
        .expect("the fixture has one source");
    source.file_path = Some(path.to_string_lossy().into_owned());
    source.url = None;
    dollet_core::db::epg::save_source(&app.state.db, &source)
        .await
        .unwrap()
}

/// The guide id a channel in the fixture actually maps to.
///
/// Read rather than written down: guide ids in the fixture are invented words,
/// so any literal here would be one more meaningless token that a hand edit to
/// the fixture could move. What the feed below needs is *a* mapped id, and the
/// database is the only thing that knows which.
async fn a_mapped_tvg_id(app: &TestApp) -> String {
    sqlx::query_scalar(
        "SELECT d.tvg_id FROM epg_data d JOIN channel c ON c.epg_data_id = d.id
         WHERE d.epg_source_id = 1 AND d.tvg_id IS NOT NULL
         ORDER BY c.id LIMIT 1",
    )
    .fetch_one(&app.state.db)
    .await
    .expect("the fixture maps at least one channel to guide data")
}

/// A feed with one mapped channel, one unmapped, and one of each programme.
///
/// `mapped` is the fixture's, so a refresh of this has something in scope.
/// The other two ids are invented and deliberately not real call signs —
/// nothing in this repository should ship a lineup that belongs to somebody.
fn guide_xml(mapped: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="{mapped}"><display-name>Mapped Guide</display-name><icon src="https://logos.example/mapped.png" /></channel>
  <channel id="second.test"><display-name>Second Guide</display-name></channel>
  <channel id="nobody.watches"><display-name>Unmapped</display-name></channel>
  <programme start="20260912120000 +0000" stop="20260912130000 +0000" channel="{mapped}">
    <title>Mapped Programme</title><desc>Kept</desc>
  </programme>
  <programme start="20260912130000 +0000" stop="20260912140000 +0000" channel="nobody.watches">
    <title>Unmapped Programme</title>
  </programme>
</tv>
"#
    )
}

#[tokio::test]
async fn an_epg_refresh_stores_every_guide_channel_but_only_mapped_listings() {
    let app = TestApp::new().await;
    let mapped = a_mapped_tvg_id(&app).await;
    let source = source_from_file(&app, &guide_xml(&mapped)).await;
    let handle = super::jobs::test_handle(&app.state, "epg_refresh:1");

    // The fixture ships listings of its own, and a refresh only rewrites the
    // guide channels in scope. Cleared first, so every count below is what
    // *this* refresh stored rather than what was already sitting there.
    sqlx::query("DELETE FROM program")
        .execute(&app.state.db)
        .await
        .unwrap();

    let summary = super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .expect("refresh ran");

    // Every declared channel is stored, because the picker offers all of them.
    for tvg_id in [mapped.as_str(), "second.test", "nobody.watches"] {
        let found: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM epg_data WHERE tvg_id = ?")
            .bind(tvg_id)
            .fetch_one(&app.state.db)
            .await
            .unwrap();
        assert_eq!(found, 1, "{tvg_id} was not stored");
    }

    let listings = async |tvg_id: &str| {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM program p JOIN epg_data d ON d.id = p.epg_data_id
             WHERE d.tvg_id = ? AND d.epg_source_id = 1",
        )
        .bind(tvg_id)
        .fetch_one(&app.state.db)
        .await
        .unwrap()
    };

    // One channel in the fixture maps to `mapped`, so that one is in scope.
    // Asserted, because the rest of this test passes just as well when nothing
    // is mapped and the refresh stored no listings at all — which is precisely
    // the failure it is here to catch.
    assert!(listings(&mapped).await > 0, "{summary}");

    // And only that one. A real feed carries thousands of channels against a
    // couple of dozen mapped; storing the rest is hundreds of thousands of
    // rows nobody reads.
    assert_eq!(
        listings("nobody.watches").await,
        0,
        "programmes were stored for an unmapped channel"
    );

    assert!(summary.starts_with("3 guide channels, 1 "), "{summary}");
}

#[tokio::test]
async fn an_epg_refresh_replaces_a_channels_listings_rather_than_appending() {
    let app = TestApp::new().await;
    let mapped = a_mapped_tvg_id(&app).await;
    let source = source_from_file(&app, &guide_xml(&mapped)).await;
    let handle = super::jobs::test_handle(&app.state, "epg_refresh:1");

    // Map a channel at the guide channel this feed declares, so its programmes
    // are the ones in scope.
    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .unwrap();
    let data_id: i64 = sqlx::query_scalar("SELECT id FROM epg_data WHERE tvg_id = ?")
        .bind(&mapped)
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE channel SET epg_data_id = ? WHERE id = 171")
        .bind(data_id)
        .execute(&app.state.db)
        .await
        .unwrap();

    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .unwrap();
    let first: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM program WHERE epg_data_id = ?")
        .bind(data_id)
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(first, 1);

    // A provider that drops a programme has dropped it; merging would leave the
    // old one showing forever.
    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .unwrap();
    let second: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM program WHERE epg_data_id = ?")
        .bind(data_id)
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(second, first, "listings accumulated across refreshes");
}

#[tokio::test]
async fn epg_auto_matching_never_overwrites_a_hand_assignment() {
    let app = TestApp::new().await;
    let mapped = a_mapped_tvg_id(&app).await;
    let source = source_from_file(&app, &guide_xml(&mapped)).await;
    let handle = super::jobs::test_handle(&app.state, "epg_refresh:1");
    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .unwrap();

    let wrong: i64 = sqlx::query_scalar("SELECT id FROM epg_data WHERE tvg_id = 'nobody.watches'")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE channel SET epg_data_id = ? WHERE id = 181")
        .bind(wrong)
        .execute(&app.state.db)
        .await
        .unwrap();

    super::ingest::epg::auto_match(&app.state, 1).await.unwrap();

    let after: i64 = sqlx::query_scalar("SELECT epg_data_id FROM channel WHERE id = 181")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(after, wrong, "a fuzzy score overwrote a hand assignment");
}

/// Point the sample seed's provider account at a playlist on disk, the way a
/// file-backed source works.
async fn account_from_file(app: &TestApp, body: &str) -> dollet_core::domain::M3uAccount {
    file_backed_account(app, 2, body).await
}

/// The same for a named account, which the synthetic seed needs: its provider
/// accounts start at 1001.
async fn file_backed_account(
    app: &TestApp,
    id: dollet_core::domain::Id,
    body: &str,
) -> dollet_core::domain::M3uAccount {
    let path = app.state.config.cache_dir().join("playlist.m3u");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();

    let mut account = dollet_core::db::m3u::get_account(&app.state.db, id)
        .await
        .unwrap()
        .expect("the seed has a provider account");
    account.file_path = Some(path.to_string_lossy().into_owned());
    account.server_url = None;
    dollet_core::db::m3u::save_account(&app.state.db, &account)
        .await
        .unwrap()
}

fn playlist(entries: &[(&str, &str, &str)]) -> String {
    let mut out = String::from("#EXTM3U\n");
    for (name, group, url) in entries {
        out.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"\" tvg-name=\"{name}\" group-title=\"{group}\",{name}\n{url}\n"
        ));
    }
    out
}

#[tokio::test]
async fn an_m3u_refresh_keeps_existing_streams_rather_than_rebuilding_them() {
    let app = TestApp::new().await;

    // The fixture's `last_seen` is a fixed date, so left alone it ages out of
    // the seven-day retention window and this test starts deleting what it
    // claims to keep. Pin it to yesterday.
    sqlx::query("UPDATE stream SET last_seen = ? WHERE m3u_account_id = 2")
        .bind(dollet_core::db::sql_timestamp(
            chrono::Utc::now() - chrono::Duration::days(1),
        ))
        .execute(&app.state.db)
        .await
        .unwrap();

    // Two of the fixture's own streams, verbatim. The hash keys on the URL, so
    // re-presenting them must resolve to the rows already stored — anything
    // else orphans the catalogue and empties every channel's failover list.
    let existing: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT s.name, COALESCE(g.name, 'Default Group'), s.url FROM stream s
         LEFT JOIN channel_group g ON g.id = s.channel_group_id
         WHERE s.m3u_account_id = 2 ORDER BY s.id LIMIT 2",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap();

    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE m3u_account_id = 2")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    let ids_before: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM stream WHERE m3u_account_id = 2 ORDER BY id LIMIT 2")
            .fetch_all(&app.state.db)
            .await
            .unwrap();

    let body = playlist(
        &existing
            .iter()
            .map(|(name, group, url)| (name.as_str(), group.as_str(), url.as_str()))
            .collect::<Vec<_>>(),
    );
    let account = account_from_file(&app, &body).await;
    let handle = super::jobs::test_handle(&app.state, "m3u_refresh:2");

    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect("refresh ran");

    let ids_after: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM stream WHERE m3u_account_id = 2 ORDER BY id LIMIT 2")
            .fetch_all(&app.state.db)
            .await
            .unwrap();
    assert_eq!(ids_after, ids_before, "the catalogue was rebuilt");
    assert!(summary.contains("0 new"), "{summary}");

    // The rest are missing from this feed but inside the retention window, so
    // they are stale rather than gone: a provider hiccup must not delete a
    // channel's failover list.
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE m3u_account_id = 2")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(
        after, before,
        "missing streams were deleted on the first miss"
    );

    let stale: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE is_stale = 1 AND m3u_account_id = 2")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(stale as usize, before as usize - existing.len());
}

#[tokio::test]
async fn an_m3u_refresh_adds_new_streams_and_their_groups() {
    let app = TestApp::new().await;
    let body = playlist(&[
        (
            "Brand New HD",
            "A Brand New Group",
            "https://provider.example/live/u/p/9001",
        ),
        (
            "Second One",
            "A Brand New Group",
            "https://provider.example/live/u/p/9002",
        ),
    ]);
    let account = account_from_file(&app, &body).await;
    let handle = super::jobs::test_handle(&app.state, "m3u_refresh:2");

    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .unwrap();
    assert!(summary.contains("2 new"), "{summary}");

    let group: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM channel_group WHERE name = 'A Brand New Group'")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(group, 1, "the group was not created");

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT s.name FROM stream s JOIN channel_group g ON g.id = s.channel_group_id
         WHERE g.name = 'A Brand New Group' ORDER BY s.name",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap();
    assert_eq!(stored, vec!["Brand New HD", "Second One"]);
}

#[tokio::test]
async fn a_filter_that_will_not_compile_is_refused_at_the_door() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, body) = app
        .json(
            "POST",
            "/api/m3u/accounts/2/filters/",
            &token,
            Some(json!({ "filter_type": "name", "regex_pattern": "(unclosed", "exclude": true })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Lookaround is PCRE-flavoured and rejected by Rust's `regex` crate, but
    // ingest compiles with `fancy_regex`, so it must be accepted here.
    let (status, body) = app
        .json(
            "POST",
            "/api/m3u/accounts/2/filters/",
            &token,
            Some(json!({
                "filter_type": "name",
                "regex_pattern": "^(?=.*HD)(.*)$",
                "exclude": false
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_group_filter_drops_streams_before_they_are_stored() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, _) = app
        .json(
            "POST",
            "/api/m3u/accounts/2/filters/",
            &token,
            Some(json!({
                "filter_type": "group",
                "regex_pattern": "Adult",
                "exclude": true
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let body = playlist(&[
        ("Wanted", "Sports", "https://provider.example/live/u/p/1"),
        (
            "Unwanted",
            "Adult Channels",
            "https://provider.example/live/u/p/2",
        ),
    ]);
    let account = account_from_file(&app, &body).await;
    let handle = super::jobs::test_handle(&app.state, "m3u_refresh:2");

    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .unwrap();
    assert!(summary.contains("1 filtered out"), "{summary}");

    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE name = 'Unwanted'")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(stored, 0);
}

/// The one notification a producer raises today, end to end.
///
/// A refresh with an uncompilable filter reports *success* — correctly, since
/// the streams it wrote are right — and the extra sentence in its summary is
/// gone the moment the job row is overwritten by the next run. The operator
/// meets the consequence weeks later as channels in Plex the filters were
/// supposed to remove, which is the failure this whole table exists to end.
#[tokio::test]
async fn a_refresh_whose_filter_will_not_compile_says_so_until_it_is_fixed() {
    let app = TestApp::synthetic().await;

    // Inserted rather than posted: `POST /filters/` refuses an uncompilable
    // pattern at the door, so the only ways one exists are an import — the
    // source's engine accepts patterns `fancy_regex` does not — and a rule that
    // stopped compiling under a newer engine. Both are how an
    // operator actually meets this, and neither goes through a handler.
    sqlx::query(
        "INSERT INTO m3u_filter (m3u_account_id, filter_type, regex_pattern, exclude, sort_order)
         VALUES (1001, 'name', '^(unclosed', 1, 1)",
    )
    .execute(&app.state.db)
    .await
    .unwrap();

    let body = playlist(&[(
        "Synth Kept",
        "Synth Sports",
        "https://provider.example/live/u/p/9001",
    )]);
    let account = file_backed_account(&app, 1001, &body).await;
    let handle = super::jobs::test_handle(&app.state, "m3u_refresh:1001");

    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect("a broken filter is a warning, not a failed refresh");
    assert!(summary.contains("would not compile"), "{summary}");

    let raised = open_notification(&app, "m3u.filter_broken", "account:1001")
        .await
        .expect("the refresh kept more streams than asked and said nothing");
    assert_eq!(
        raised.severity,
        dollet_core::db::notifications::Severity::Warning
    );
    assert_eq!(raised.occurrences, 1);
    // The pattern and the account, because "a filter is broken" sends the
    // operator to look through every account's rules to find out which.
    assert!(raised.title.contains(&account.name), "{}", raised.title);
    assert!(raised.message.contains("^(unclosed"), "{}", raised.message);
    assert_eq!(raised.detail["m3u_account_id"], 1001);

    // Still broken the next night: one row with a count on it, not two rows.
    super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .unwrap();
    let again = open_notification(&app, "m3u.filter_broken", "account:1001")
        .await
        .expect("the second refresh dropped the notification");
    assert_eq!(again.id, raised.id);
    assert_eq!(again.occurrences, 2);

    // Fixed. A notification the operator cannot make go away by fixing the
    // thing it names is one they learn to ignore.
    sqlx::query("DELETE FROM m3u_filter WHERE regex_pattern = '^(unclosed'")
        .execute(&app.state.db)
        .await
        .unwrap();
    super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .unwrap();
    assert!(
        open_notification(&app, "m3u.filter_broken", "account:1001")
            .await
            .is_none(),
        "the condition was fixed and the notification stayed"
    );
}

async fn open_notification(
    app: &TestApp,
    kind: &str,
    subject: &str,
) -> Option<dollet_core::db::notifications::Notification> {
    dollet_core::db::notifications::list(&app.state.db)
        .await
        .unwrap()
        .into_iter()
        .find(|notification| notification.kind == kind && notification.subject == subject)
}

#[tokio::test]
async fn notifications_list_with_what_nobody_has_seen_first() {
    let app = TestApp::synthetic().await;
    let token = match app.login_as(Principal::Admin).await {
        Credential::Bearer(token) => token,
        other => panic!("the admin did not get a bearer token: {other:?}"),
    };

    let (status, body) = app.json("GET", "/api/notifications/", &token, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Unacknowledged first, newest first inside each half. The acknowledged
    // row is the *most* recently updated of the three, so a plain
    // `ORDER BY updated_at DESC` would put it at the top.
    let ids: Vec<i64> = rows(&body)
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![1003, 1001, 1002], "{body}");

    let recurring = &rows(&body)[1];
    assert_eq!(recurring["occurrences"], 12);
    assert_eq!(recurring["severity"], "warning");
    assert!(recurring["acknowledged_at"].is_null());
    assert_eq!(recurring["detail"]["m3u_account_id"], 1003);

    let (status, count) = app
        .json("GET", "/api/notifications/count/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{count}");
    assert_eq!(count["unacknowledged"], 2, "{count}");
}

#[tokio::test]
async fn acknowledging_empties_the_badge_without_losing_the_row() {
    let app = TestApp::synthetic().await;
    let token = match app.login_as(Principal::Admin).await {
        Credential::Bearer(token) => token,
        other => panic!("the admin did not get a bearer token: {other:?}"),
    };

    let (status, one) = app
        .json("POST", "/api/notifications/1003/acknowledge/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert!(!one["acknowledged_at"].is_null(), "{one}");
    // The row survives: acknowledging is "I have seen this", and a condition
    // that is still true has to stay readable after the badge is empty.
    assert_eq!(one["id"], 1003);

    let (_, count) = app
        .json("GET", "/api/notifications/count/", &token, None)
        .await;
    assert_eq!(count["unacknowledged"], 1);

    let (status, all) = app
        .json("POST", "/api/notifications/acknowledge-all/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(
        all["acknowledged"], 1,
        "already-seen rows were counted again"
    );

    let (_, count) = app
        .json("GET", "/api/notifications/count/", &token, None)
        .await;
    assert_eq!(count["unacknowledged"], 0);

    let (status, body) = app
        .json("DELETE", "/api/notifications/1002/", &token, None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (_, listed) = app.json("GET", "/api/notifications/", &token, None).await;
    assert_eq!(listed["count"], 2, "{listed}");

    // An id that is gone is a 404, not a 200 answering about nothing.
    let (status, body) = app
        .json("POST", "/api/notifications/1002/acknowledge/", &token, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn auto_channel_sync_creates_channels_in_the_configured_range() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let body = playlist(&[
        (
            "Auto One",
            "Auto Group",
            "https://provider.example/live/u/p/1",
        ),
        (
            "Auto Two",
            "Auto Group",
            "https://provider.example/live/u/p/2",
        ),
    ]);
    let account = account_from_file(&app, &body).await;
    let handle = super::jobs::test_handle(&app.state, "m3u_refresh:2");

    // First pass creates the group; the toggle can only be set afterwards.
    super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .unwrap();
    let group: i64 = sqlx::query_scalar("SELECT id FROM channel_group WHERE name = 'Auto Group'")
        .fetch_one(&app.state.db)
        .await
        .unwrap();

    let (status, _) = app
        .json(
            "POST",
            "/api/m3u/accounts/2/groups/",
            &token,
            Some(json!({
                "channel_group": group,
                "enabled": true,
                "auto_channel_sync": true,
                // Both streams are on no channel, so switching auto-sync on
                // previews them first; `switching_auto_sync_on_previews_...`
                // covers that exchange.
                "confirm": true
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app
        .json(
            "PATCH",
            &format!("/api/channels/groups/{group}/"),
            &token,
            Some(json!({ "number_start": 500.0 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .unwrap();
    assert!(summary.contains("2 channels created"), "{summary}");

    let numbers: Vec<f64> = sqlx::query_scalar(
        "SELECT channel_number FROM channel WHERE auto_created = 1 ORDER BY channel_number",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap();
    assert_eq!(numbers, vec![500.0, 501.0]);

    // Each gets its stream attached, or the channel is unplayable.
    let attached: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM channel_stream cs JOIN channel c ON c.id = cs.channel_id
         WHERE c.auto_created = 1",
    )
    .fetch_one(&app.state.db)
    .await
    .unwrap();
    assert_eq!(attached, 2);

    // And a second pass does not create them again.
    super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .unwrap();
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel WHERE auto_created = 1")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(total, 2, "auto sync duplicated its own channels");
}

#[tokio::test]
async fn the_scheduler_registers_a_job_per_source_and_account() {
    let app = TestApp::new().await;
    super::jobs::sync_schedule(&app.state).await.unwrap();

    let keys: Vec<String> = dollet_core::db::jobs::list(&app.state.db)
        .await
        .unwrap()
        .into_iter()
        .map(|job| job.key)
        .collect();

    assert!(keys.contains(&"epg_refresh:1".to_owned()), "{keys:?}");
    assert!(keys.contains(&"m3u_refresh:2".to_owned()), "{keys:?}");
    // The built-in `custom` account holds hand-added streams and has no
    // provider to refresh from.
    assert!(!keys.contains(&"m3u_refresh:1".to_owned()), "{keys:?}");
}

/// Deactivating a provider or a guide source stops its schedule.
///
/// An account an operator turns off is usually one whose subscription lapsed
/// or whose credentials were suspended; a scheduler that keeps calling it does
/// so unattended, daily, and forever.
#[tokio::test]
async fn an_inactive_source_or_account_is_registered_without_a_schedule() {
    use dollet_core::db::jobs;

    let app = TestApp::synthetic().await;
    super::jobs::sync_schedule(&app.state).await.unwrap();

    let interval = async |key: &str| {
        jobs::by_key(&app.state.db, key)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("no job {key}"))
            .interval_seconds
    };

    // Active: scheduled at the interval its row asks for.
    // Two different intervals, so this reads the row rather than a constant.
    assert_eq!(interval("epg_refresh:1001").await, Some(24 * 3600));
    assert_eq!(interval("m3u_refresh:1001").await, Some(24 * 3600));
    assert_eq!(interval("m3u_refresh:1002").await, Some(12 * 3600));

    // Inactive: the row still exists, so the jobs page can show it and
    // re-activating has something to update — but it carries no interval, and
    // `due` filters on precisely that.
    assert_eq!(interval("epg_refresh:1003").await, None);
    assert_eq!(interval("m3u_refresh:1003").await, None);

    // A year out, so nothing here depends on the seed's timestamps.
    let due: Vec<String> = jobs::due(
        &app.state.db,
        chrono::Utc::now() + chrono::Duration::days(365),
    )
    .await
    .unwrap()
    .into_iter()
    .map(|job| job.key)
    .collect();
    assert!(!due.contains(&"epg_refresh:1003".to_owned()), "{due:?}");
    assert!(!due.contains(&"m3u_refresh:1003".to_owned()), "{due:?}");
    // And not vacuously: the active ones are due by then.
    assert!(due.contains(&"epg_refresh:1001".to_owned()), "{due:?}");
    assert!(due.contains(&"m3u_refresh:1001".to_owned()), "{due:?}");
}

#[tokio::test]
async fn deleting_a_source_takes_its_schedule_with_it() {
    let app = TestApp::new().await;
    let token = app.login().await;
    super::jobs::sync_schedule(&app.state).await.unwrap();

    let (status, _) = app
        .json("DELETE", "/api/epg/sources/1/", &token, None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert!(
        dollet_core::db::jobs::by_key(&app.state.db, "epg_refresh:1")
            .await
            .unwrap()
            .is_none(),
        "a job outlived the source it refreshes"
    );
}

#[tokio::test]
async fn the_epg_and_m3u_apis_never_echo_a_provider_password() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, sources) = app.json("GET", "/api/epg/sources/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!sources.to_string().contains("\"password\""), "{sources}");

    let (status, accounts) = app.json("GET", "/api/m3u/accounts/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!accounts.to_string().contains("\"password\""), "{accounts}");
    assert_eq!(rows(&accounts)[1]["has_password"], true);
}

#[tokio::test]
async fn a_manual_refresh_reports_whether_it_started() {
    let app = TestApp::new().await;
    let token = app.login().await;
    super::jobs::sync_schedule(&app.state).await.unwrap();

    let (status, body) = app.json("POST", "/api/epg/refresh/1/", &token, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["started"], true);

    let (status, _) = app
        .json("POST", "/api/epg/sources/404/refresh/", &token, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_artwork_proxy_serves_the_url_every_output_points_at() {
    let app = TestApp::new().await;

    // The provider CDN is unreachable from a test, so this exercises the
    // fallback: a broken image is worse than a slow one, so a fetch failure
    // hands the client the provider URL rather than an error.
    let (status, headers, _) = app.raw("/api/channels/logos/20/cache/").await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        headers["location"],
        fixture_logo_url(&app.state.db, 20).await
    );

    // Some clients strip the trailing slash from artwork URLs.
    assert_eq!(
        app.raw("/api/channels/logos/20/cache").await.0,
        StatusCode::TEMPORARY_REDIRECT
    );

    let (status, _, _) = app.raw("/api/channels/logos/99999/cache/").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Provider-controlled bytes served from this origin, where the SPA keeps its
/// tokens in `localStorage`. An SVG sniffed out of a logo URL would otherwise
/// be a script with access to them.
#[tokio::test]
async fn artwork_is_served_isolated_from_the_app_origin() {
    let app = TestApp::new().await;

    // The cache-hit path, primed the way a successful fetch leaves it. The
    // fetch path builds the same headers and cannot be driven from a test:
    // `allow_private` is off for artwork, so the guard refuses the loopback
    // server a mock would run on. `artwork`'s own unit test covers it.
    let path = super::artwork::cache_path(&app.state, "logo-20");
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&path, b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>")
        .await
        .unwrap();

    let (status, headers, _) = app.raw("/api/channels/logos/20/cache/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/svg+xml");
    assert_eq!(
        headers["content-security-policy"],
        "default-src 'none'; sandbox"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
}

/// Without these a wedged stream can only be cleared by restarting the
/// container.
#[tokio::test]
async fn the_stop_routes_are_admin_only_and_answer_honestly() {
    let app = TestApp::new().await;
    let token = app.login().await;
    let uuid = "91af9876-76cc-4bd1-bb7e-0d16a5af10a1";

    // They take a channel UUID, which is otherwise only exposed to admins and
    // plays that channel against the anonymous stream endpoint.
    for uri in [
        format!("/api/proxy/ts/stop/{uuid}"),
        format!("/api/proxy/ts/stop_client/{uuid}?client_id=whatever"),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri(&uri)
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.send(request).await.0, StatusCode::UNAUTHORIZED, "{uri}");
    }

    // Nothing is playing, so there is nothing to stop — but the channel exists,
    // so this is a truthful zero rather than a 404.
    let (status, body) = app
        .json("POST", &format!("/api/proxy/ts/stop/{uuid}"), &token, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stopped"], 0);

    let (status, _) = app
        .json(
            "POST",
            "/api/proxy/ts/stop/00000000-0000-0000-0000-000000000000",
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an unknown channel must 404");

    // A stale row on the Stats page must not report a silent success.
    let (status, _) = app
        .json(
            "POST",
            &format!(
                "/api/proxy/ts/stop_client/{uuid}?client_id=6f1c2e70-0000-4000-8000-000000000000"
            ),
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn job_state_is_pollable_for_a_page_that_opens_mid_refresh() {
    let app = TestApp::new().await;
    let token = app.login().await;
    super::jobs::sync_schedule(&app.state).await.unwrap();

    let (status, jobs) = app.json("GET", "/api/core/jobs/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    let jobs = rows(&jobs).clone();
    assert!(!jobs.is_empty());
    // The row says what the last run did; `running` says what this process is
    // doing now, and after a crash those disagree. Asserted as present and
    // typed rather than as `false`: the scheduler's map is process-global, so
    // whether anything is running depends on what else this binary is doing.
    assert!(jobs.iter().all(|job| job["running"].is_boolean()));
    assert!(jobs.iter().all(|job| job["progress"].is_number()));

    // A page that opens mid-refresh needs the current state before the next
    // `/ws` frame arrives.
    let (status, job) = app
        .json("GET", "/api/core/jobs/epg_refresh:1/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["kind"], "epg_refresh");
    assert!(job["interval_seconds"].is_number());

    let (status, _) = app
        .json("GET", "/api/core/jobs/no_such_job:9/", &token, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Cancel, both ways round, with a key of its own for each.
///
/// Keys unique to this test: `epg_refresh:1` is driven by several tests and
/// the running map is process-global, so a shared key would force accepting
/// either answer.
#[tokio::test]
async fn cancel_refuses_an_idle_job_and_actually_stops_a_running_one() {
    use dollet_core::db::jobs;

    let app = TestApp::new().await;
    let token = app.login().await;

    // Not running: a conflict, not a silent success. The UI puts a row into
    // "cancelling" on the strength of a 200, and a job that was never running
    // would sit there saying so until the page is reloaded.
    let idle = "cancel_idle:1";
    jobs::ensure(&app.state.db, idle, "never_runs", &json!({}), None)
        .await
        .unwrap();
    let (status, body) = app
        .json(
            "POST",
            &format!("/api/core/jobs/{idle}/cancel/"),
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Running: a 200 that has to mean the token was signalled rather than that
    // the endpoint says yes to everything.
    let busy = "cancel_busy:1";
    jobs::ensure(
        &app.state.db,
        busy,
        "blocks_until_cancelled",
        &json!({}),
        None,
    )
    .await
    .unwrap();
    super::jobs::register(std::collections::HashMap::from([(
        "blocks_until_cancelled",
        super::jobs::handler(|_, _, handle: super::jobs::JobHandle| async move {
            handle.cancel.cancelled().await;
            Err(dollet_core::Error::Other(anyhow::anyhow!("stopped")))
        }),
    )]));

    let job = jobs::by_key(&app.state.db, busy).await.unwrap().unwrap();
    assert!(super::jobs::trigger(&app.state, job));
    wait_until(|| super::jobs::is_running(busy)).await;

    let (status, body) = app
        .json(
            "POST",
            &format!("/api/core/jobs/{busy}/cancel/"),
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["cancelling"], true);

    wait_until(|| !super::jobs::is_running(busy)).await;
    let job = jobs::by_key(&app.state.db, busy).await.unwrap().unwrap();
    assert_eq!(
        job.state,
        jobs::State::Cancelled,
        "the row does not say it was cancelled"
    );
}

/// Poll a process-global the scheduler writes from another task. Two seconds,
/// which is an eternity for a `DashMap` entry and short enough that a hang
/// fails rather than stalling the suite.
async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..100 {
        if condition() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the condition never held");
}

/// The TV Guide's one call. A grid built on `/programs/` is either one query
/// per channel or a window far larger than the screen.
#[tokio::test]
async fn the_guide_grid_returns_every_channel_in_lineup_order() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, grid) = app.json("GET", "/api/epg/grid/", &token, None).await;
    assert_eq!(status, StatusCode::OK, "{grid}");

    let channels = grid["channels"].as_array().unwrap();
    // Every visible channel, hidden excluded — the same set and the same order
    // as the lineup Plex reads, because both come from `effective_channel`.
    assert_eq!(channels.len(), 16);
    assert_eq!(
        channels[0]["name"],
        fixture_name(&app.state.db, "channel", 171).await
    );
    assert_eq!(channels[0]["channel_number"], 1.0);
    assert!(grid["start"].is_string() && grid["end"].is_string());

    // A row with an empty strip says "this channel has no guide"; a missing row
    // looks like the channel is gone.
    assert!(
        channels.iter().all(|c| c["programs"].is_array()),
        "a channel came back without a programme list"
    );
    assert!(
        channels.iter().any(|c| c["epg_data_id"].is_null()),
        "the fixture should carry a channel with no guide at all"
    );
}

#[tokio::test]
async fn the_grid_includes_a_programme_that_began_before_the_window() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let data_id: i64 = sqlx::query_scalar("SELECT epg_data_id FROM channel WHERE id = 171")
        .fetch_one(&app.state.db)
        .await
        .unwrap();

    let now = chrono::Utc::now();
    let insert =
        |title: &str, start: chrono::DateTime<chrono::Utc>, end: chrono::DateTime<chrono::Utc>| {
            sqlx::query(
            "INSERT INTO program (epg_data_id, start_time, end_time, title) VALUES (?, ?, ?, ?)",
        )
        .bind(data_id)
        .bind(dollet_core::db::sql_timestamp(start))
        .bind(dollet_core::db::sql_timestamp(end))
        .bind(title.to_owned())
        };

    // A three-hour film that started before the window is the single most
    // visible thing a grid can get wrong.
    insert(
        "Long Film",
        now - chrono::Duration::hours(2),
        now + chrono::Duration::hours(1),
    )
    .execute(&app.state.db)
    .await
    .unwrap();
    insert(
        "Well Outside",
        now + chrono::Duration::days(9),
        now + chrono::Duration::days(9) + chrono::Duration::hours(1),
    )
    .execute(&app.state.db)
    .await
    .unwrap();

    let (_, grid) = app.json("GET", "/api/epg/grid/", &token, None).await;
    let row = grid["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == 171)
        .unwrap();

    let titles: Vec<&str> = row["programs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["title"].as_str().unwrap())
        .collect();
    assert!(titles.contains(&"Long Film"), "{titles:?}");
    assert!(!titles.contains(&"Well Outside"), "{titles:?}");
}

#[tokio::test]
async fn the_grid_window_is_bounded_and_validated() {
    let app = TestApp::new().await;
    let token = app.login().await;

    // A request for a decade is not a guide.
    let (status, grid) = app
        .json(
            "GET",
            "/api/epg/grid/?from=2026-01-01T00:00:00Z&to=2036-01-01T00:00:00Z",
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let start: chrono::DateTime<chrono::Utc> = grid["start"].as_str().unwrap().parse().unwrap();
    let end: chrono::DateTime<chrono::Utc> = grid["end"].as_str().unwrap().parse().unwrap();
    assert!(
        end - start <= chrono::Duration::days(14),
        "{start} -> {end}"
    );

    let (status, _) = app
        .json(
            "GET",
            "/api/epg/grid/?from=2026-09-12T00:00:00Z&to=2026-09-11T00:00:00Z",
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a backwards window");

    // `from + Duration` panics rather than saturating once the sum leaves the
    // representable range. RFC 3339's four-digit year keeps that out of reach
    // from the query string today, so this pins the guard rather than
    // reproducing a live panic — the arithmetic is what would have to change.
    let (status, _) = app
        .json(
            "GET",
            "/api/epg/grid/?from=9999-12-31T23:59:00Z",
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "a far-future window");
}

/// The third outcome `sync::epg` reports. Without somewhere to put it, the
/// ambiguous band degrades to "no guide" with no explanation — which would be
/// the real regression, not the missing model.
#[tokio::test]
async fn an_ambiguous_match_is_offered_in_the_grid_and_settled_once() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let data_id: i64 = sqlx::query_scalar("SELECT id FROM epg_data ORDER BY id LIMIT 1")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE channel SET epg_data_id = NULL WHERE id = 171")
        .execute(&app.state.db)
        .await
        .unwrap();
    dollet_core::db::epg::suggest_match(
        &app.state.db,
        &dollet_core::db::epg::MatchSuggestion {
            channel_id: 171,
            epg_data_id: data_id,
            score: 63.5,
        },
    )
    .await
    .unwrap();

    let row_for = |grid: &Value| -> Value {
        grid["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == 171)
            .unwrap()
            .clone()
    };

    let (_, grid) = app.json("GET", "/api/epg/grid/", &token, None).await;
    let row = row_for(&grid);
    assert_eq!(row["epg_suggestion"]["epg_data_id"], data_id);
    assert_eq!(row["epg_suggestion"]["score"], 63.5);
    assert!(
        row["epg_suggestion"]["name"].is_string(),
        "no candidate name"
    );

    let (status, accepted) = app
        .json("POST", "/api/epg/suggestions/171/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["epg_data_id"], data_id);

    let (_, grid) = app.json("GET", "/api/epg/grid/", &token, None).await;
    let row = row_for(&grid);
    assert!(row["epg_suggestion"].is_null(), "the question came back");
    assert_eq!(row["epg_data_id"], data_id);

    // Answering twice is a 404, not a second silent assignment.
    let (status, _) = app
        .json("POST", "/api/epg/suggestions/171/", &token, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dismissing_a_suggestion_leaves_the_channel_alone() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let before: Option<i64> = sqlx::query_scalar("SELECT epg_data_id FROM channel WHERE id = 173")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    let other: i64 = sqlx::query_scalar("SELECT id FROM epg_data ORDER BY id DESC LIMIT 1")
        .fetch_one(&app.state.db)
        .await
        .unwrap();

    dollet_core::db::epg::suggest_match(
        &app.state.db,
        &dollet_core::db::epg::MatchSuggestion {
            channel_id: 173,
            epg_data_id: other,
            score: 55.0,
        },
    )
    .await
    .unwrap();

    let (status, _) = app
        .json("DELETE", "/api/epg/suggestions/173/", &token, None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let after: Option<i64> = sqlx::query_scalar("SELECT epg_data_id FROM channel WHERE id = 173")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(after, before, "dismissing changed the channel's guide");
}

/// A cancelled refresh says so, rather than looking like one that never ran.
///
/// The account endpoints report the job's state in the job's own vocabulary;
/// collapsing everything but `running`/`failed` to `idle` loses `cancelled`.
/// The Sources page reads `running` straight off `/api/core/jobs/` for the
/// same account on the same screen.
#[tokio::test]
async fn a_cancelled_refresh_is_reported_as_cancelled_not_as_idle() {
    use dollet_core::db::jobs;

    let app = TestApp::new().await;
    let token = app.login().await;
    super::jobs::sync_schedule(&app.state).await.unwrap();

    jobs::claim(&app.state.db, "m3u_refresh:2").await.unwrap();
    jobs::finish(
        &app.state.db,
        "m3u_refresh:2",
        jobs::State::Cancelled,
        None,
        Some("stopped by the operator"),
    )
    .await
    .unwrap();

    let (status, accounts) = app.json("GET", "/api/m3u/accounts/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    let account = rows(&accounts)
        .iter()
        .find(|a| a["id"] == 2)
        .expect("the provider account")
        .clone();
    assert_eq!(account["status"], "cancelled", "{account}");
    assert_eq!(account["last_message"], "stopped by the operator");

    // The same word the jobs endpoint uses for the same row.
    let (_, job) = app
        .json("GET", "/api/core/jobs/m3u_refresh:2/", &token, None)
        .await;
    assert_eq!(job["state"], account["status"]);
}

/// `updated_at` is when the account last refreshed *successfully*: a claim
/// must not clear it and a failure must not stamp it, or a provider failing
/// for a week reports itself refreshed an hour ago.
#[tokio::test]
async fn only_a_successful_refresh_moves_the_accounts_timestamp() {
    use dollet_core::db::jobs;

    let app = TestApp::new().await;
    let token = app.login().await;
    super::jobs::sync_schedule(&app.state).await.unwrap();

    let account = |value: &Value| {
        rows(value)
            .iter()
            .find(|a| a["id"] == 2)
            .expect("the provider account")
            .clone()
    };

    jobs::claim(&app.state.db, "m3u_refresh:2").await.unwrap();
    jobs::finish(
        &app.state.db,
        "m3u_refresh:2",
        jobs::State::Success,
        None,
        None,
    )
    .await
    .unwrap();
    let (_, accounts) = app.json("GET", "/api/m3u/accounts/", &token, None).await;
    let refreshed = account(&accounts)["updated_at"].clone();
    assert!(refreshed.is_string(), "{refreshed}");

    jobs::claim(&app.state.db, "m3u_refresh:2").await.unwrap();
    let (status, accounts) = app.json("GET", "/api/m3u/accounts/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    let running = account(&accounts);
    assert_eq!(running["status"], "running", "{running}");
    assert_eq!(
        running["updated_at"], refreshed,
        "a running refresh reports the account as never refreshed"
    );

    // And the run it is in the middle of fails, which is not a refresh.
    jobs::finish(
        &app.state.db,
        "m3u_refresh:2",
        jobs::State::Failed,
        None,
        Some("connection refused"),
    )
    .await
    .unwrap();
    let (_, accounts) = app.json("GET", "/api/m3u/accounts/", &token, None).await;
    let failed = account(&accounts);
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(
        failed["updated_at"], refreshed,
        "a failed refresh passed itself off as the last good one"
    );
}

/// One spelling for an account type, and an unknown one is refused.
///
/// A parser that reads anything unrecognised as `standard` turns a typo into a
/// plain-playlist account pointed at an Xtream provider: wrong URL, wrong hash,
/// catalogue rebuilt, with a 200 on the way in.
#[tokio::test]
async fn an_account_type_is_one_spelling_and_an_unknown_one_is_refused() {
    let app = TestApp::synthetic().await;
    let Credential::Bearer(token) = app.login_as(Principal::Admin).await else {
        panic!("the admin did not get a bearer token");
    };

    let (status, account) = app
        .json("GET", "/api/m3u/accounts/1002/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(account["account_type"], "xtream_codes", "{account}");

    // What the SPA sent before 1.0 still works, for one release.
    for legacy in ["XC", "STD"] {
        let (status, _) = app
            .json(
                "PATCH",
                "/api/m3u/accounts/1002/",
                &token,
                Some(json!({ "account_type": legacy })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{legacy}");
    }

    for bad in ["xc", "Xtream", "xtreamcodes", ""] {
        let (status, body) = app
            .json(
                "PATCH",
                "/api/m3u/accounts/1002/",
                &token,
                Some(json!({ "account_type": bad })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?} was accepted");
        assert!(body.to_string().contains("xtream_codes"), "{body}");
    }

    // And the account is still what it was, not silently downgraded.
    let (_, account) = app
        .json("GET", "/api/m3u/accounts/1002/", &token, None)
        .await;
    assert_eq!(
        account["account_type"], "standard",
        "the last valid PATCH won"
    );
}

/// The same, for a guide source: an unknown type is not read as `xmltv`.
#[tokio::test]
async fn an_unknown_guide_source_type_is_refused_rather_than_read_as_xmltv() {
    let app = TestApp::synthetic().await;
    let Credential::Bearer(token) = app.login_as(Principal::Admin).await else {
        panic!("the admin did not get a bearer token");
    };

    let (status, body) = app
        .json(
            "PATCH",
            "/api/epg/sources/1002/",
            &token,
            Some(json!({ "source_type": "Dummy" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A dummy source has no URL, so reading it as `xmltv` would have made the
    // next refresh fetch nothing and report a source with neither.
    let (_, source) = app
        .json("GET", "/api/epg/sources/1002/", &token, None)
        .await;
    assert_eq!(source["source_type"], "dummy", "{source}");
}

/// The refresh interval carries its unit on the wire, as it does everywhere else.
#[tokio::test]
async fn the_refresh_interval_says_hours_and_still_takes_the_old_name() {
    let app = TestApp::synthetic().await;
    let Credential::Bearer(token) = app.login_as(Principal::Admin).await else {
        panic!("the admin did not get a bearer token");
    };

    let (status, account) = app
        .json(
            "PATCH",
            "/api/m3u/accounts/1002/",
            &token,
            Some(json!({ "refresh_interval_hours": 6 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(account["refresh_interval_hours"], 6, "{account}");
    assert!(account.get("refresh_interval").is_none(), "{account}");

    // `refresh_interval` was the name before 1.0; a client still sending it
    // is not a client that should stop working.
    let (status, account) = app
        .json(
            "PATCH",
            "/api/m3u/accounts/1002/",
            &token,
            Some(json!({ "refresh_interval": 9 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(account["refresh_interval_hours"], 9, "{account}");
}

/// A capped list reports the size of the collection, not the cap: `count: 5,
/// pages: 1` for `?limit=5` says "five exist", and a client that believes it
/// has everything deletes what it thinks is missing.
#[tokio::test]
async fn a_capped_list_counts_the_collection_rather_than_the_page() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, all) = app.json("GET", "/api/epg/epgdata/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    let total = all["count"].as_i64().expect("a count");
    assert!(total > 5, "the fixture has a guide to truncate: {total}");

    let (status, capped) = app
        .json("GET", "/api/epg/epgdata/?limit=5", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows(&capped).len(), 5, "the cap still applies");
    assert_eq!(capped["count"], total, "count is the collection");
    assert!(
        capped["pages"].as_i64().unwrap() > 1,
        "a truncated list claimed to be the only page: {capped}"
    );

    // Same shape on the other one. Events are written by the streaming path,
    // so this seeds a few rather than assuming the fixture has any.
    for n in 0..4 {
        dollet_core::db::events::record(
            &app.state.db,
            "test_event",
            None,
            None,
            &json!({ "n": n }),
        )
        .await
        .unwrap();
    }

    let (_, events) = app
        .json("GET", "/api/core/system-events/?limit=2", &token, None)
        .await;
    assert_eq!(rows(&events).len(), 2);
    assert_eq!(events["count"], 4);
    assert_eq!(events["pages"], 2);
}

/// The ambiguous list survives a guide bigger than one page of it.
///
/// `list_data` clamps at ten thousand rows ordered by name, so a lookup through
/// one page silently drops every candidate past it. The fixture has 96 guide
/// channels, so this plants a candidate that sorts last and fills the table
/// past the clamp.
#[tokio::test]
async fn the_ambiguous_list_does_not_lose_a_candidate_to_the_guide_size() {
    use dollet_core::domain::EpgData;

    let app = TestApp::new().await;
    let token = app.login().await;

    let filler: Vec<EpgData> = (0..10_500)
        .map(|n| EpgData {
            id: 0,
            epg_source_id: Some(1),
            // Sorts ahead of the candidate below, so the clamp's window is
            // full before it is reached.
            tvg_id: Some(format!("filler{n:05}.test")),
            name: format!("AAA Filler {n:05}"),
            icon_url: None,
        })
        .collect();
    dollet_core::db::epg::upsert_data(&app.state.db, &filler)
        .await
        .unwrap();

    let candidate = EpgData {
        id: 0,
        epg_source_id: Some(1),
        tvg_id: Some("zzz-last.test".into()),
        name: "ZZZ Sorts Last".into(),
        icon_url: None,
    };
    dollet_core::db::epg::upsert_data(&app.state.db, std::slice::from_ref(&candidate))
        .await
        .unwrap();
    let epg_data_id: i64 = sqlx::query_scalar("SELECT id FROM epg_data WHERE tvg_id = ?")
        .bind("zzz-last.test")
        .fetch_one(&app.state.db)
        .await
        .unwrap();

    dollet_core::db::epg::suggest_match(
        &app.state.db,
        &dollet_core::db::epg::MatchSuggestion {
            channel_id: 171,
            epg_data_id,
            score: 61.0,
        },
    )
    .await
    .unwrap();

    let (status, waiting) = app.json("GET", "/api/epg/ambiguous/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows(&waiting).len(),
        1,
        "the suggestion vanished: {waiting}"
    );
    assert_eq!(rows(&waiting)[0]["candidate_name"], "ZZZ Sorts Last");
    assert_eq!(rows(&waiting)[0]["candidate_tvg_id"], "zzz-last.test");
}

/// "3 channels need a guide decision" is only actionable if the user can ask
/// *which three*. A count tells them something is wrong without telling them
/// what, which is the failure the three-outcome model exists to avoid.
#[tokio::test]
async fn the_ambiguous_list_names_the_channels_and_their_candidates() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, empty) = app.json("GET", "/api/epg/ambiguous/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        rows(&empty).is_empty(),
        "nothing is waiting on a fresh fixture"
    );

    let candidates: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM epg_data ORDER BY id LIMIT 2")
            .fetch_all(&app.state.db)
            .await
            .unwrap();

    for (channel, (epg_data_id, _)) in [171, 173].iter().zip(&candidates) {
        dollet_core::db::epg::suggest_match(
            &app.state.db,
            &dollet_core::db::epg::MatchSuggestion {
                channel_id: *channel,
                epg_data_id: *epg_data_id,
                score: 61.0,
            },
        )
        .await
        .unwrap();
    }

    let (_, waiting) = app.json("GET", "/api/epg/ambiguous/", &token, None).await;
    let waiting = rows(&waiting);
    assert_eq!(waiting.len(), 2);

    let first = &waiting[0];
    assert_eq!(first["channel_id"], 171);
    assert_eq!(
        first["channel_name"],
        fixture_name(&app.state.db, "channel", 171).await
    );
    assert_eq!(first["epg_data_id"], candidates[0].0);
    assert_eq!(first["candidate_name"], candidates[0].1);
    assert_eq!(first["score"], 61.0);

    // Accepting one is a PATCH of the channel, which the editor already does —
    // and it has to clear the question, or the list never shrinks.
    let (status, _) = app
        .json("POST", "/api/epg/suggestions/171/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let (_, remaining) = app.json("GET", "/api/epg/ambiguous/", &token, None).await;
    assert_eq!(rows(&remaining).len(), 1);
    assert_eq!(rows(&remaining)[0]["channel_id"], 173);
}

// --- The server against the snapshots ---------------------------------------
//
// `golden.rs` reconstructs `EffectiveChannel` values *from* the snapshots and
// calls the serializers directly, so it proves "given the right channel list we
// emit these bytes" and says nothing about whether the queries produce that
// list, in that order, with `effective_*` resolved and hidden channels
// filtered. What follows boots the real router over `sample.sql` and diffs what
// the server actually serves, which is what Plex talks to.
//
// The snapshots describe the same 53 channels the seed carries, adversarial
// rows included, so the two sides are directly comparable.

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden")
}

fn snapshot(name: &str) -> String {
    let path = golden_dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The corpus as `(file, expected status, path)`, read from the manifest
/// beside it rather than transcribed — a path added to the corpus and not to
/// this suite would otherwise go unserved and unnoticed.
fn manifest() -> Vec<(String, u16, String)> {
    snapshot("manifest.txt")
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?.to_owned();
            let status = fields.next()?.parse().ok()?;
            Some((name, status, fields.next()?.to_owned()))
        })
        .collect()
}

/// Payloads whose *content* depends on which programmes fall inside a window,
/// so a snapshot taken at another instant describes a different set of
/// entries. `golden.rs` pins these against the serializer with the clock
/// substituted; there is nothing here to compare them to.
const WINDOW_DEPENDENT: &[&str] = &[
    "xc-get-short-epg.json",
    "xc-get-short-epg-limit-2.json",
    "xc-get-simple-data-table.json",
    "xc-get-simple-data-table-dummy.json",
];

/// Payloads that are fixed but for three readings of the clock.
///
/// Masking the three clock fields is enough to compare the rest whole — for
/// `panel_api.php` that is the entire embedded lineup.
const CLOCK_STAMPED: &[&str] = &["xc-account-info.json", "xc-panel-api.json"];

/// The three clock-derived fields, in both objects that carry them.
const CLOCK_FIELDS: &[(&str, &str)] = &[
    ("user_info", "exp_date"),
    ("server_info", "timestamp_now"),
    ("server_info", "time_now"),
];

/// Replace each clock reading with a marker, failing if one is not there: a
/// field that quietly disappeared would otherwise weaken this comparison
/// without anything saying so.
fn mask_the_clock(value: &mut Value, what: &str) {
    for (object, field) in CLOCK_FIELDS {
        let slot = value
            .get_mut(object)
            .and_then(|o| o.get_mut(field))
            .unwrap_or_else(|| panic!("{what}: no {object}.{field} to mask"));
        *slot = json!("@CLOCK@");
    }
}

/// The masked fields, checked for being a current clock rather than compared.
fn assert_the_clock_is_current(value: &Value, what: &str) {
    let now = chrono::Utc::now().timestamp();

    let stamp = value["server_info"]["timestamp_now"]
        .as_i64()
        .unwrap_or_else(|| panic!("{what}: timestamp_now is not a number"));
    assert!(
        (now - stamp).abs() < 300,
        "{what}: timestamp_now is {stamp}"
    );

    // A client reads an expiry in the past as a dead account and stops.
    let expiry: i64 = value["user_info"]["exp_date"]
        .as_str()
        .unwrap_or_else(|| panic!("{what}: exp_date is not a string"))
        .parse()
        .unwrap_or_else(|_| panic!("{what}: exp_date is not a timestamp"));
    assert!(expiry > now, "{what}: exp_date is in the past");

    // The two have to describe the same instant, or a client showing "server
    // time" shows something the rest of the response disagrees with.
    let formatted = chrono::DateTime::from_timestamp(stamp, 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    assert_eq!(value["server_info"]["time_now"], json!(formatted), "{what}");
}

/// Line by line, so a failure names the first line that differs rather than
/// printing two whole documents.
fn assert_lines_match(ours: &str, snapshot: &str, what: &str) {
    for (index, (mine, theirs)) in ours.lines().zip(snapshot.lines()).enumerate() {
        assert_eq!(
            mine,
            theirs,
            "{what}: line {} differs from the snapshot",
            index + 1
        );
    }
    assert_eq!(
        ours.lines().count(),
        snapshot.lines().count(),
        "{what}: line count differs from the snapshot"
    );
    assert_eq!(ours, snapshot, "{what}");
}

/// The channel block of a guide: everything before the first programme.
///
/// `sample.sql` samples the programme table and carries fixed timestamps, so
/// which programmes fall inside a served window depends on when the suite
/// runs; `golden.rs` compares those against the snapshot directly. The channel
/// elements are what the query layer decides — which ids, which names, which
/// icons, in which order — and that is what the server is checked on.
fn guide_channels(xml: &str) -> String {
    xml.lines()
        .take_while(|line| {
            let line = line.trim_start();
            !line.starts_with("<programme") && !line.starts_with("</tv>")
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

#[tokio::test]
async fn every_served_playlist_matches_its_snapshot() {
    let app = TestApp::new().await;

    for name in [
        "output-m3u.m3u",
        "output-m3u-tvg-id.m3u",
        "output-m3u-gracenote.m3u",
        "output-m3u-direct.m3u",
        "output-m3u-no-cached-logos.m3u",
        "output-m3u-output-profile.m3u",
        "xc-get-php.m3u",
    ] {
        let uri = manifest()
            .into_iter()
            .find(|(file, ..)| file == name)
            .unwrap_or_else(|| panic!("{name} is not in the manifest"))
            .2;

        let (status, headers, ours) = app.raw(&uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(headers["content-type"], "audio/x-mpegurl", "{uri}");
        assert_lines_match(&ours, &snapshot(name), &uri);
    }
}

#[tokio::test]
async fn the_served_guide_channels_match_their_snapshots() {
    let app = TestApp::new().await;

    for (name, uri) in [
        ("output-epg.xml", "/output/epg"),
        ("output-epg-tvg-id.xml", "/output/epg?tvg_id_source=tvg_id"),
        (
            "output-epg-gracenote.xml",
            "/output/epg?tvg_id_source=gracenote",
        ),
        ("output-epg-days-1.xml", "/output/epg?days=1"),
        (
            "xc-xmltv.xml",
            "/xmltv.php?username=fixtureadmin&password=fixturepass",
        ),
    ] {
        let (status, headers, ours) = app.raw(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(headers["content-type"], "application/xml", "{uri}");
        assert_lines_match(
            &guide_channels(&ours),
            &guide_channels(&snapshot(name)),
            uri,
        );
    }
}

#[tokio::test]
async fn every_served_json_payload_matches_its_snapshot() {
    let app = TestApp::new().await;

    // Compared as parsed values rather than bytes, so a snapshot's formatting
    // is free — which is what lets a re-pin write it pretty-printed.
    //
    // Everything in the manifest but the clock-dependent payloads is here, so
    // a new snapshot joins this suite by existing. Bad Xtream credentials
    // answer with a page rather than JSON; the status is what a client acts on,
    // and it is asserted for every entry.
    let mut checked = 0;
    for (name, status, uri) in manifest() {
        if !name.ends_with(".json") {
            continue;
        }

        let (served, mut ours) = app.public(&uri).await;
        assert_eq!(served.as_u16(), status, "{uri}");
        if status != 200 || WINDOW_DEPENDENT.contains(&name.as_str()) {
            continue;
        }

        let mut expected: Value = serde_json::from_str(&snapshot(&name)).unwrap();
        if CLOCK_STAMPED.contains(&name.as_str()) {
            assert_the_clock_is_current(&ours, &uri);
            mask_the_clock(&mut ours, &uri);
            mask_the_clock(&mut expected, &name);
        }
        assert_eq!(ours, expected, "{uri}");
        checked += 1;
    }

    // A manifest this failed to read would otherwise pass silently.
    assert!(checked >= 17, "only {checked} JSON snapshots were compared");
}

#[tokio::test]
async fn the_served_device_xml_matches_the_snapshot() {
    let app = TestApp::new().await;
    let (_, _, ours) = app.raw("/hdhr/device.xml").await;
    assert_lines_match(&ours, &snapshot("hdhr-device.xml"), "/hdhr/device.xml");
}

#[tokio::test]
async fn a_missing_output_profile_is_a_404() {
    let app = TestApp::new().await;

    // The status is the contract a client acts on; the body is pinned too, so
    // a change to it is at least seen.
    let (status, _, body) = app.raw("/output/m3u/NoSuchProfile").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, snapshot("output-m3u-missing-profile.m3u"));
}

/// Rewrite every snapshot the router can produce, and the checksum file.
///
/// The guide snapshots carry more programmes than `sample.sql` samples, so
/// only their channel block is rewritten here; the programme blocks are pinned
/// by the round-trip tests in `golden.rs`, and the dummy channels' are
/// rewritten by that file's `repin_the_dummy_guide_snapshots`. Clock-dependent
/// payloads are left alone. A JSON snapshot is rewritten only when its value
/// changed, so a re-pin does not reformat what it did not touch.
///
/// ```text
/// cargo test -p dollet-server repin -- --ignored
/// ```
#[tokio::test]
#[ignore = "rewrites the snapshots; run on purpose after a deliberate output change"]
async fn repin_the_served_snapshots() {
    let app = TestApp::new().await;

    for (name, expected_status, uri) in manifest() {
        let (status, _, body) = app.raw(&uri).await;
        assert_eq!(status.as_u16(), expected_status, "{uri}");

        let current = snapshot(&name);
        let next =
            if status != StatusCode::OK || name.ends_with(".m3u") || name == "hdhr-device.xml" {
                body
            } else if name.ends_with(".xml") {
                let programmes = current
                    .find("  <programme ")
                    .expect("a guide snapshot carries programmes");
                format!("{}{}", guide_channels(&body), &current[programmes..])
            } else if WINDOW_DEPENDENT.contains(&name.as_str()) {
                // Which programmes fall in the window depends on when this
                // runs, so there is nothing stable to write. `golden.rs` pins
                // these against the serializer instead.
                continue;
            } else if CLOCK_STAMPED.contains(&name.as_str()) {
                // Re-pinnable, but only by keeping the clock the file already
                // has: writing this run's would make the corpus differ from
                // itself on every re-pin, and `golden.rs` compares these
                // against a substituted clock rather than a current one.
                let mut ours: Value = serde_json::from_str(&body).unwrap();
                let existing: Value = serde_json::from_str(&current).unwrap();
                for (object, field) in CLOCK_FIELDS {
                    let pinned = existing[object][field].clone();
                    ours[object][field] = pinned;
                }
                if ours == existing {
                    continue;
                }
                serde_json::to_string_pretty(&ours).unwrap() + "\n"
            } else {
                let ours: Value = serde_json::from_str(&body).unwrap();
                if ours == serde_json::from_str::<Value>(&current).unwrap() {
                    continue;
                }
                serde_json::to_string_pretty(&ours).unwrap() + "\n"
            };
        if next != current {
            std::fs::write(golden_dir().join(&name), next).unwrap();
        }
    }

    // FNV-1a, the same tripwire `golden.rs` checks: a truncated or half-written
    // snapshot fails by name.
    let mut names: Vec<String> = manifest().into_iter().map(|(name, ..)| name).collect();
    names.sort();
    let mut out = String::new();
    for name in names {
        let bytes = std::fs::read(golden_dir().join(&name)).unwrap();
        let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
        for byte in &bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01B3);
        }
        out.push_str(&format!("{name}\t{}\t{hash:016x}\n", bytes.len()));
    }
    std::fs::write(golden_dir().join("checksums.tsv"), out).unwrap();
}

// --- Ingest safety ----------------------------------------------------------

#[tokio::test]
async fn a_hash_key_that_selects_nothing_refuses_to_write_anything() {
    let app = TestApp::new().await;

    // What an imported instance can carry. Every entry then hashes identically,
    // `reconcile` matches one stream and calls the other 60 missing, and the
    // retention window turns that into a delete — which cascades every
    // `channel_stream` row away with it.
    dollet_core::settings::patch_by_key(
        &app.state.db,
        "stream_settings",
        &json!({ "m3u_hash_key": "" }),
    )
    .await
    .unwrap();

    let existing: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT s.name, COALESCE(g.name, 'Default Group'), s.url FROM stream s
         LEFT JOIN channel_group g ON g.id = s.channel_group_id
         WHERE s.m3u_account_id = 2 ORDER BY s.id",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap();

    let body = playlist(
        &existing
            .iter()
            .map(|(name, group, url)| (name.as_str(), group.as_str(), url.as_str()))
            .collect::<Vec<_>>(),
    );
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE m3u_account_id = 2")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    let stale_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE m3u_account_id = 2 AND is_stale = 1")
            .fetch_one(&app.state.db)
            .await
            .unwrap();

    let account = account_from_file(&app, &body).await;
    let handle = super::jobs::test_handle(&app.state, "m3u_refresh:2");
    let error = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect_err("a refresh that would erase the catalogue must not report success");

    let message = error.to_string();
    assert!(message.contains("refusing to write"), "{message}");
    assert!(message.contains("m3u_hash_key"), "{message}");

    // Nothing was written: not a new row, not a stale flag.
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE m3u_account_id = 2")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    let stale_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE m3u_account_id = 2 AND is_stale = 1")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(after, before, "streams were written");
    assert_eq!(stale_after, stale_before, "streams were marked stale");
}

#[tokio::test]
async fn a_refresh_never_deletes_a_channels_only_stream() {
    let app = TestApp::new().await;

    // A channel with exactly one stream, and a playlist that omits it entirely
    // from a group that is about to be disabled. Both roads to deletion — the
    // group leaving the active set, and the entry leaving the feed — end here.
    let (channel_id, stream_id, group_id): (i64, i64, i64) = sqlx::query_as(
        "SELECT cs.channel_id, cs.stream_id, s.channel_group_id
           FROM channel_stream cs
           JOIN stream s ON s.id = cs.stream_id
          WHERE s.m3u_account_id = 2 AND s.channel_group_id IS NOT NULL
          GROUP BY cs.channel_id
         HAVING COUNT(*) = 1
          LIMIT 1",
    )
    .fetch_one(&app.state.db)
    .await
    .unwrap();

    dollet_core::db::m3u::upsert_group_link(
        &app.state.db,
        &dollet_core::db::m3u::GroupAccountLink {
            id: 0,
            channel_group_id: group_id,
            m3u_account_id: 2,
            enabled: false,
            auto_channel_sync: false,
            auto_sync_channel_start: None,
            auto_sync_channel_end: None,
            custom_properties: json!({}),
        },
    )
    .await
    .unwrap();

    let account = account_from_file(&app, &playlist(&[("Kept", "Other", "http://x/1")])).await;
    let handle = super::jobs::test_handle(&app.state, "m3u_refresh:2");
    super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect("refresh ran");

    let survives: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE id = ?")
        .bind(stream_id)
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(survives, 1, "the channel's only stream was deleted");

    let still_assigned: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM channel_stream WHERE channel_id = ?")
            .bind(channel_id)
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(still_assigned, 1, "the channel was left unplayable");
}

#[tokio::test]
async fn disabling_a_group_reports_what_it_would_cost_before_doing_it() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let group_id: i64 = sqlx::query_scalar(
        "SELECT channel_group_id FROM stream WHERE m3u_account_id = 2
          AND channel_group_id IS NOT NULL GROUP BY channel_group_id
         ORDER BY COUNT(*) DESC LIMIT 1",
    )
    .fetch_one(&app.state.db)
    .await
    .unwrap();

    let (status, body) = app
        .json(
            "POST",
            "/api/m3u/accounts/2/groups/",
            &token,
            Some(json!({ "channel_group": group_id, "enabled": false })),
        )
        .await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let cost = &body["cost"];
    assert!(
        cost["streams_deleted"].as_i64().unwrap_or(0) > 0,
        "the refusal carried no blast radius: {body}"
    );
    assert!(cost["channels_affected"].is_i64(), "{body}");
    assert!(cost["channels_left_unplayable"].is_i64(), "{body}");

    // Still enabled: the call was a question, not an action.
    let links = dollet_core::db::m3u::list_group_links(&app.state.db, Some(2))
        .await
        .unwrap();
    assert!(
        links
            .iter()
            .find(|link| link.channel_group_id == group_id)
            .is_none_or(|link| link.enabled),
        "the group was disabled without confirmation"
    );

    let (status, _) = app
        .json(
            "POST",
            "/api/m3u/accounts/2/groups/",
            &token,
            Some(json!({ "channel_group": group_id, "enabled": false, "confirm": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let links = dollet_core::db::m3u::list_group_links(&app.state.db, Some(2))
        .await
        .unwrap();
    assert!(
        !links
            .iter()
            .find(|link| link.channel_group_id == group_id)
            .expect("the link exists")
            .enabled,
        "a confirmed disable did not take effect"
    );
}

#[tokio::test]
async fn a_provider_url_never_reaches_an_error_with_its_password() {
    // Both halves of the leak: the one this module interpolates, and the one
    // `reqwest::Error` appends on its own. `job.last_error` is served as
    // `last_message`, pushed to `/ws` and printed to stdout.
    let raw = "https://provider.example/get.php?username=bob&password=hunter2&type=m3u_plus";
    let redacted = super::ingest::redact(raw);
    assert!(!redacted.contains("hunter2"), "{redacted}");
    assert!(redacted.contains("username=bob"), "{redacted}");
    assert!(redacted.contains("password=REDACTED"), "{redacted}");

    assert_eq!(
        super::ingest::redact("https://bob:hunter2@provider.example/list.m3u"),
        "https://provider.example/list.m3u"
    );

    let message = super::ingest::scrub(&format!(
        "error sending request for url ({raw}): connection refused"
    ));
    assert!(!message.contains("hunter2"), "{message}");
    assert!(message.contains("connection refused"), "{message}");

    // The sentence survives intact: punctuation that ended the clause rather
    // than the URL must not be swallowed into it and come back re-encoded.
    let message = super::ingest::scrub(&format!("fetching {raw}: timed out"));
    assert!(message.ends_with("&type=m3u_plus: timed out"), "{message}");

    // Not a URL at all, and a URL this cannot parse, both stay safe.
    assert_eq!(super::ingest::scrub("no urls here"), "no urls here");
    assert_eq!(super::ingest::redact("http://["), "<unparseable url>");

    // An Xtream *stream* URL carries the same password in a path segment
    // rather than a query parameter, which is a shape neither the userinfo
    // strip nor the query rewrite above can see. `ingest::xtream` builds every
    // one of them as `{base}/live/{user}/{pass}/{id}.ts`.
    let stream = "https://provider.example:8080/live/bob/hunter2/4211.ts";
    let redacted = super::ingest::redact(stream);
    assert!(!redacted.contains("hunter2"), "{redacted}");
    assert!(redacted.ends_with("/4211.ts"), "{redacted}");

    // A path that merely contains the word is left alone: redacting by
    // position would corrupt any provider whose playlist lives under a
    // similarly named directory.
    assert_eq!(
        super::ingest::redact("https://provider.example/live/stream.ts"),
        "https://provider.example/live/stream.ts"
    );
}

/// Creating a user with an Xtream password keeps it. No payload struct here
/// denies unknown fields, so a field the create handler ignores is accepted
/// with a 201 and silently dropped.
#[tokio::test]
async fn a_new_user_keeps_the_xtream_password_it_was_created_with() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, created) = app
        .json(
            "POST",
            "/api/accounts/users/",
            &token,
            Some(json!({
                "username": "xtream-viewer",
                "password": "login-password",
                "custom_properties": { "xc_password": "player-secret" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["custom_properties"]["xc_password"], "player-secret");

    // And it is the credential the Xtream API actually authenticates on, which
    // is the only reason the field exists.
    let (status, account) = app
        .public("/player_api.php?username=xtream-viewer&password=player-secret")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(account["user_info"]["auth"], 1, "{account}");

    // The login password must not work there. An Xtream URL carries its secret
    // in the query string, where it lands in every proxy log, which is why the
    // two are separate credentials at all.
    let (status, _, body) = app
        .raw("/player_api.php?username=xtream-viewer&password=login-password")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// A name the fixture decides, read from it rather than written down here.
///
/// Channel, group and guide names in `fixtures/sample.sql` are invented words,
/// so pinning one in this file would make a hand edit to the fixture edit the
/// test suite too, and would put a meaningless token in a place a reader
/// expects to learn something.
/// What these assertions are actually about is that the *right row* came
/// back, which its id says.
async fn fixture_name(db: &sqlx::SqlitePool, table: &str, id: i64) -> String {
    sqlx::query_scalar(&format!("SELECT name FROM {table} WHERE id = ?"))
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap_or_else(|e| panic!("no {table} {id}: {e}"))
}

/// A logo's URL, read from the fixture for the same reason as `fixture_name`.
async fn fixture_logo_url(db: &sqlx::SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT url FROM logo WHERE id = ?")
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap_or_else(|e| panic!("no logo {id}: {e}"))
}

/// A `SessionStats` with nothing interesting in it, for tests that care about
/// one field. Built here rather than per test: the struct has sixteen fields
/// and a test that spells all of them out to set one is unreadable.
fn synthetic_session(uuid: &str) -> dollet_stream::SessionStats {
    dollet_stream::SessionStats {
        channel: uuid.parse().unwrap(),
        output: dollet_stream::OutputKey::Raw,
        phase: dollet_stream::Phase::Streaming,
        healthy: true,
        source_index: 0,
        source_id: None,
        url: None,
        switches: 0,
        last_error: None,
        started_at: chrono::Utc::now(),
        total_bytes: 0,
        buffer: dollet_stream::RingStats {
            chunks: 0,
            bytes: 0,
            head: 0,
            oldest: None,
            seconds: 0.0,
        },
        media: Default::default(),
        progress: Default::default(),
        clients: Vec::new(),
    }
}

/// The live-stats payload, which is the fourth place the same secret can go.
///
/// `SessionStats.url` is the URL the proxy is actually pulling, serialized
/// whole into `GET /api/proxy/stats/` and pushed on every `channel_stats`
/// frame. For an Xtream provider that string contains the account password,
/// and the Stats page renders it — including into a `title` attribute.
///
/// Admin-only, so not a cross-role leak, but it is the one credential this
/// project otherwise refuses to hand back — `has_password` and `redact` exist
/// for it — and `url` is frozen as a payload key at 1.0.
#[tokio::test]
async fn the_live_stats_payload_never_carries_a_provider_password() {
    let app = TestApp::new().await;

    let session = dollet_stream::SessionStats {
        url: Some("https://provider.example:8080/live/bob/hunter2/4211.ts".into()),
        ..synthetic_session("bbbbbbbb-0000-4000-8000-000000000001")
    };

    let payload = super::stream::with_now_playing(&app.state, vec![session])
        .await
        .expect("stats serialized");
    let body = serde_json::to_string(&payload).unwrap();

    assert!(!body.contains("hunter2"), "{body}");
    // Still useful to an operator: which provider, and which stream.
    assert!(body.contains("provider.example"), "{body}");
    assert!(body.contains("4211.ts"), "{body}");
}

#[tokio::test]
async fn a_panicking_job_releases_its_key_and_is_recorded_as_failed() {
    let app = TestApp::new().await;

    let key = "panics:1";
    dollet_core::db::jobs::ensure(&app.state.db, key, "panics", &json!({}), None)
        .await
        .unwrap();

    super::jobs::register(std::collections::HashMap::from([(
        "panics",
        super::jobs::handler(|_, _, _| async { panic!("handler exploded") }),
    )]));

    let job = dollet_core::db::jobs::by_key(&app.state.db, key)
        .await
        .unwrap()
        .unwrap();
    assert!(super::jobs::trigger(&app.state, job));

    // The guard is an in-memory map entry; the state is a row. A panic that
    // skips the statement clearing the first and never reaches the write
    // setting the second leaves the key held and `due` skipping the row until
    // the process restarts.
    for _ in 0..100 {
        if !super::jobs::is_running(key) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!super::jobs::is_running(key), "the key is still held");

    let job = dollet_core::db::jobs::by_key(&app.state.db, key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.state, dollet_core::db::jobs::State::Failed);
    assert!(
        job.last_error.unwrap_or_default().contains("panicked"),
        "the panic was not recorded"
    );
}

#[tokio::test]
async fn a_cancelled_epg_refresh_leaves_the_previous_guide_whole() {
    let app = TestApp::new().await;
    let mapped = a_mapped_tvg_id(&app).await;
    let source = source_from_file(&app, &guide_xml(&mapped)).await;
    let handle = super::jobs::test_handle(&app.state, "epg_refresh:1");

    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .unwrap();
    let data_id: i64 = sqlx::query_scalar("SELECT id FROM epg_data WHERE tvg_id = ?")
        .bind(&mapped)
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE channel SET epg_data_id = ? WHERE id = 171")
        .bind(data_id)
        .execute(&app.state.db)
        .await
        .unwrap();
    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .unwrap();

    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM program WHERE epg_data_id = ?")
        .bind(data_id)
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(before, 1, "the guide was not populated to begin with");

    // Cancelled before the refresh starts, which stands in for a cancel or a
    // SIGTERM arriving mid-parse: a deletion up front and outside a
    // transaction would leave the user with whatever had been read so far.
    // Nothing visible is written until the parse completes.
    let cancelled = super::jobs::test_handle(&app.state, "epg_refresh:1");
    cancelled.cancel.cancel();
    assert!(
        super::ingest::epg::refresh(&app.state, &source, &cancelled)
            .await
            .is_err()
    );

    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM program WHERE epg_data_id = ?")
        .bind(data_id)
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(after, before, "a cancelled refresh truncated the guide");

    // And nothing was left staged for the next one to promote as if complete.
    let staged: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM program_incoming WHERE epg_source_id = 1")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(staged, 0, "staged rows outlived the refusal");
}

#[tokio::test]
async fn a_scheduled_refresh_does_not_reassign_guide_data_on_its_own() {
    let app = TestApp::new().await;
    let mapped = a_mapped_tvg_id(&app).await;
    let source = source_from_file(&app, &guide_xml(&mapped)).await;
    let handle = super::jobs::test_handle(&app.state, "epg_refresh:1");

    sqlx::query("UPDATE channel SET epg_data_id = NULL")
        .execute(&app.state.db)
        .await
        .unwrap();

    // Matching runs only when a user asks. Running it on every scheduled
    // refresh would rewrite `epg_data_id` across the catalogue every interval
    // with a count in a job row as the only record.
    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .unwrap();
    let assigned: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM channel WHERE epg_data_id IS NOT NULL")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(assigned, 0, "a timer reassigned guide data unasked");

    // The capability is a button.
    let token = app.login().await;
    let (status, body) = app.json("POST", "/api/epg/match/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["matched"].is_i64(), "{body}");

    let assigned: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM channel WHERE epg_data_id IS NOT NULL")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert!(assigned > 0, "the manual match assigned nothing");
}

/// What is on, attached to what is streaming.
///
/// The Stats page shows sessions; the guide is joined to channels. Without this
/// the page can say a channel is up and not what is playing on it.
#[tokio::test]
async fn live_sessions_carry_what_is_on_the_channel() {
    let app = TestApp::new().await;

    let session = synthetic_session;

    // Channel 171 is mapped in the fixture; 900 is one of the adversarial rows
    // and has no guide at all.
    let (mapped, unmapped): (String, String) = sqlx::query_as(
        "SELECT (SELECT uuid FROM channel WHERE id = 171),
                (SELECT uuid FROM channel WHERE id = 900)",
    )
    .fetch_one(&app.state.db)
    .await
    .unwrap();

    // A programme covering this instant, so the bar has something to draw.
    let data_id: i64 = sqlx::query_scalar("SELECT epg_data_id FROM channel WHERE id = 171")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    let now = chrono::Utc::now();
    dollet_core::db::epg::insert_programs(
        &app.state.db,
        &[dollet_core::domain::Program {
            id: 0,
            epg_data_id: data_id,
            tvg_id: None,
            start_time: now - chrono::Duration::minutes(10),
            end_time: now + chrono::Duration::minutes(20),
            title: "The Thing That Is On".into(),
            sub_title: None,
            description: Some("Half an hour of it".into()),
            custom_properties: json!({}),
        }],
    )
    .await
    .unwrap();

    let enriched = super::stream::with_now_playing(
        &app.state,
        vec![
            session(&mapped),
            session(&unmapped),
            // A session whose channel has been deleted since it started.
            session("aaaaaaaa-0000-4000-8000-0000000000ff"),
        ],
    )
    .await
    .unwrap();

    assert_eq!(enriched.len(), 3);
    // The session's own fields survive being wrapped.
    assert_eq!(enriched[0]["channel"], mapped);
    assert_eq!(enriched[0]["phase"], "streaming");

    let playing = &enriched[0]["now_playing"];
    assert_eq!(playing["state"], "programme");
    assert_eq!(playing["title"], "The Thing That Is On");
    assert_eq!(playing["duration_seconds"], 1800);
    // Elapsed and remaining are computed here rather than in the browser: a
    // client's clock can be minutes off the server's, which on a half-hour
    // programme is a visibly wrong position.
    let elapsed = playing["elapsed_seconds"].as_i64().unwrap();
    assert!((595..=605).contains(&elapsed), "{elapsed}");
    assert_eq!(
        playing["remaining_seconds"].as_i64().unwrap(),
        1800 - elapsed
    );

    // Never null and never absent, because the page has to tell "this channel
    // has no listings" from "the answer has not arrived yet".
    assert_eq!(enriched[1]["now_playing"]["state"], "unmapped");
    assert_eq!(enriched[2]["now_playing"]["state"], "unknown");
}

#[tokio::test]
async fn a_channel_on_a_dummy_source_is_never_reported_as_having_nothing_on() {
    let app = TestApp::new().await;

    // Dummy sources generate their listings per request, so there are no stored
    // rows to find — and "no rows" must not reach the page as "nothing is on".
    sqlx::query("UPDATE epg_source SET source_type = 'dummy' WHERE id = 1")
        .execute(&app.state.db)
        .await
        .unwrap();
    let uuid: String = sqlx::query_scalar("SELECT uuid FROM channel WHERE id = 171")
        .fetch_one(&app.state.db)
        .await
        .unwrap();

    let enriched = super::stream::with_now_playing(
        &app.state,
        vec![dollet_stream::SessionStats {
            channel: uuid.parse().unwrap(),
            output: dollet_stream::OutputKey::Raw,
            phase: dollet_stream::Phase::Streaming,
            healthy: true,
            source_index: 0,
            source_id: None,
            url: None,
            switches: 0,
            last_error: None,
            started_at: chrono::Utc::now(),
            total_bytes: 0,
            buffer: dollet_stream::RingStats {
                chunks: 0,
                bytes: 0,
                head: 0,
                oldest: None,
                seconds: 0.0,
            },
            media: Default::default(),
            progress: Default::default(),
            clients: Vec::new(),
        }],
    )
    .await
    .unwrap();

    let playing = &enriched[0]["now_playing"];
    assert_eq!(playing["state"], "programme");
    assert_eq!(playing["generated"], true);
    assert!(playing["title"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(playing["remaining_seconds"].as_i64().unwrap() > 0);
}

/// The manual source switch, which is an operator's alternative to stopping a
/// channel that is technically alive and visibly bad.
#[tokio::test]
async fn the_switch_routes_answer_honestly_and_are_admin_only() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let uuid: String = sqlx::query_scalar("SELECT uuid FROM channel WHERE id = 171")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    let first_source: i64 =
        sqlx::query_scalar("SELECT stream_id FROM channel_stream WHERE channel_id = 171 LIMIT 1")
            .fetch_one(&app.state.db)
            .await
            .unwrap();

    // Nothing is playing, so there is no session to move. 404 rather than a
    // cheerful 200: the Stats page offers this against a row it is showing, and
    // a row that has gone must say so.
    for uri in [
        format!("/api/proxy/ts/next_stream/{uuid}"),
        format!("/api/proxy/ts/change_stream/{uuid}?source_id={first_source}"),
    ] {
        let (status, _) = app.json("POST", &uri, &token, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");

        // Same reason `/api/proxy/stats/` is admin-only: the UUID in the path
        // plays this channel against the anonymous stream endpoint.
        assert_eq!(
            app.anonymous("POST", &uri).await.0,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }

    // Which source to move to is not optional, and saying so beats guessing.
    let (status, body) = app
        .json(
            "POST",
            &format!("/api/proxy/ts/change_stream/{uuid}"),
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("source_id"),
        "{body}"
    );

    // A channel that does not exist is a 404 before the registry is consulted.
    let (status, _) = app
        .json(
            "POST",
            "/api/proxy/ts/next_stream/aaaaaaaa-0000-4000-8000-0000000000ff",
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Setup must stay closed once an instance has an owner, including when that
/// owner has been deactivated.
///
/// The guard is `create_first_admin`'s own count, made inside the insert
/// transaction and regardless of `is_active`. `admin_count` counts only
/// *active* admins, so guarding on it would have `setup_status` say the
/// instance is set up while `POST` still hands it to whoever asks.
#[tokio::test]
async fn setup_cannot_be_reopened_by_deactivating_the_only_admin() {
    let app = TestApp::new().await;

    let (status, body) = app
        .anonymous("GET", "/api/accounts/initialize-superuser/")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["superuser_exists"], true);

    sqlx::query("UPDATE user SET is_active = 0")
        .execute(&app.state.db)
        .await
        .unwrap();

    let (status, body) = app
        .anonymous("GET", "/api/accounts/initialize-superuser/")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["superuser_exists"], true,
        "a disabled admin is still an owner"
    );

    let (status, _) = app
        .post_json(
            "/api/accounts/initialize-superuser/",
            json!({ "username": "attacker", "password": "whatever" }),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the instance was handed to an anonymous caller"
    );

    let admins: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user WHERE user_level >= 10")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(admins, 1, "a second admin was created");
}

/// And it must refuse *before* it hashes. `create_first_admin` is still the
/// authority, but hashing before checking would spend 1.2M PBKDF2 iterations
/// on a refused request — on an endpoint that is unauthenticated, unthrottled,
/// and refuses every caller for the rest of the instance's life. Counted
/// rather than timed, for the same reason as
/// `a_missing_account_and_a_wrong_password_cost_the_same`.
#[tokio::test]
#[ignore = "counts a process-global; needs --test-threads=1 to be meaningful"]
async fn a_closed_setup_endpoint_costs_no_key_derivation() {
    let app = TestApp::new().await;

    let before = dollet_core::auth::password::derivations();
    let (status, _) = app
        .post_json(
            "/api/accounts/initialize-superuser/",
            json!({ "username": "intruder", "password": "intruder-pass" }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(dollet_core::auth::password::derivations(), before);
}

/// Switching by position picks a different stream than the one the operator
/// clicked, whenever the channel has a stream with no URL.
///
/// The engine's source list comes from `sources_for`, which drops those; the
/// UI's menu comes from `/channels/{id}/streams/`, which returns all of them.
/// So for `[A(url), B(no url), C(url)]` position 1 means B to the operator and
/// C to the engine. `stream.url` is nullable and the API accepts an explicit
/// null, so this list is reachable rather than hypothetical.
#[tokio::test]
async fn a_switch_names_the_stream_rather_than_its_position() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let uuid: String = sqlx::query_scalar("SELECT uuid FROM channel WHERE id = 171")
        .fetch_one(&app.state.db)
        .await
        .unwrap();

    // Give the channel three streams, the middle one unplayable.
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM stream ORDER BY id LIMIT 3")
        .fetch_all(&app.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE stream SET url = NULL WHERE id = ?")
        .bind(ids[1])
        .execute(&app.state.db)
        .await
        .unwrap();
    dollet_core::db::channels::set_streams(&app.state.db, 171, &ids)
        .await
        .unwrap();

    let channel =
        dollet_core::db::channels::get_effective_by_uuid(&app.state.db, uuid.parse().unwrap())
            .await
            .unwrap()
            .unwrap();
    let sources = super::stream::sources_for(&app.state, &channel)
        .await
        .unwrap();

    // Two playable of three, so every position past the first is off by one.
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].id, ids[0]);
    assert_eq!(sources[1].id, ids[2]);

    // The unplayable one is addressable and refused by name — not silently
    // resolved to whatever sits at its old position.
    let (status, _) = app
        .json(
            "POST",
            &format!("/api/proxy/ts/change_stream/{uuid}?source_id={}", ids[1]),
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And one that is playable gets as far as the registry, which has no
    // session for it.
    let (status, _) = app
        .json(
            "POST",
            &format!("/api/proxy/ts/change_stream/{uuid}?source_id={}", ids[2]),
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The engine dials a stream URL exactly as written — it cannot depend on
/// `dollet-core`, by design — and the guarded resolver only runs for a hostname.
/// So a provider playlist carrying a bare loopback or metadata address is
/// judged in `sources_for` or nowhere.
#[tokio::test]
async fn a_stream_url_in_blocked_address_space_never_reaches_the_engine() {
    let app = TestApp::new().await;

    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM stream ORDER BY id LIMIT 3")
        .fetch_all(&app.state.db)
        .await
        .unwrap();
    for (id, url) in ids.iter().zip([
        "http://127.0.0.1:9191/proxy/ts/stream/x",
        "http://169.254.169.254/latest/meta-data/",
        // The LAN stays reachable: a tuner on it is a supported source, so
        // `allow_private` is on and only always-blocked space is refused.
        "http://192.168.1.50:5004/auto/v1",
    ]) {
        sqlx::query("UPDATE stream SET url = ? WHERE id = ?")
            .bind(url)
            .bind(id)
            .execute(&app.state.db)
            .await
            .unwrap();
    }
    dollet_core::db::channels::set_streams(&app.state.db, 171, &ids)
        .await
        .unwrap();

    let uuid: String = sqlx::query_scalar("SELECT uuid FROM channel WHERE id = 171")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    let channel =
        dollet_core::db::channels::get_effective_by_uuid(&app.state.db, uuid.parse().unwrap())
            .await
            .unwrap()
            .unwrap();

    let sources = super::stream::sources_for(&app.state, &channel)
        .await
        .unwrap();
    assert_eq!(sources.len(), 1, "{sources:?}");
    assert_eq!(sources[0].url, "http://192.168.1.50:5004/auto/v1");
}

/// The output cache is keyed partly by the `Host` header, which an
/// unauthenticated client chooses, so it has to be swept rather than only
/// expired.
#[tokio::test]
async fn stale_output_cache_files_are_deleted_and_provider_feeds_are_not() {
    let app = TestApp::new().await;
    let dir = app.state.config.cache_dir();
    tokio::fs::create_dir_all(&dir).await.unwrap();

    // One of each: a stale cached output, a fresh one, and a downloaded
    // provider feed that lives in the same directory and must survive.
    let stale = dir.join("m3u-deadbeefdeadbeef.m3u");
    let fresh = dir.join("epg-cafecafecafecafe.xml");
    let feed = dir.join("m3u-2.feed");
    for path in [&stale, &fresh, &feed] {
        tokio::fs::write(path, b"x").await.unwrap();
    }

    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    let file = std::fs::File::options().write(true).open(&stale).unwrap();
    file.set_modified(old).unwrap();
    let file = std::fs::File::options().write(true).open(&feed).unwrap();
    file.set_modified(old).unwrap();

    // Any output request that misses the cache sweeps.
    let (status, _, _) = app.raw("/output/m3u").await;
    assert_eq!(status, StatusCode::OK);

    assert!(!stale.exists(), "a stale cached output was kept");
    assert!(fresh.exists(), "a fresh cached output was deleted");
    assert!(
        feed.exists(),
        "a downloaded provider feed was deleted; re-fetching it is a provider \
         round trip, and it is not this cache's to remove"
    );
}

/// `?days=` is client-supplied and unauthenticated, and `chrono` panics rather
/// than saturating when a duration overflows the date.
#[tokio::test]
async fn an_absurd_guide_window_is_clamped_rather_than_fatal() {
    let app = TestApp::new().await;

    for days in ["1", "14", "4000000000", "4294967295"] {
        let (status, _, body) = app.raw(&format!("/output/epg?days={days}")).await;
        assert_eq!(status, StatusCode::OK, "days={days}");
        assert!(body.contains("<tv "), "days={days}");
    }
}

/// And on the *generated* half of the guide: `dummy_epg::generate` walks
/// `start + Duration::days(day)` once per day, so an unclamped
/// `?days=4000000000` spends seconds of CPU and then panics on the date
/// overflow. The test above never reaches the generator, because the fixture
/// has no dummy source.
#[tokio::test]
async fn an_absurd_guide_window_is_clamped_on_the_generated_half_too() {
    let app = TestApp::new().await;
    sqlx::query("UPDATE epg_source SET source_type = 'dummy' WHERE id = 1")
        .execute(&app.state.db)
        .await
        .unwrap();

    let (status, _, absurd) = app.raw("/output/epg?days=4000000000").await;
    assert_eq!(status, StatusCode::OK);

    let (_, _, capped) = app.raw("/output/epg?days=14").await;
    assert!(
        capped.contains("<programme"),
        "no generated listings at all"
    );
    assert_eq!(
        absurd.matches("<programme").count(),
        capped.matches("<programme").count(),
        "the generated guide did not stop at the same ceiling"
    );

    // Fourteen days of four-hour blocks, so the last one ends on the cutoff
    // itself rather than merely somewhere finite.
    let last = absurd
        .match_indices("stop=\"")
        .filter_map(|(at, prefix)| absurd.get(at + prefix.len()..at + prefix.len() + 14))
        .filter_map(|raw| chrono::NaiveDateTime::parse_from_str(raw, "%Y%m%d%H%M%S").ok())
        .max()
        .expect("the generated guide has programmes");
    let ahead = last.and_utc() - chrono::Utc::now();
    assert!(
        ahead > chrono::Duration::days(13) && ahead <= chrono::Duration::days(14),
        "the generated guide reaches {ahead} ahead"
    );
}

// --- Response shapes --------------------------------------------------------
//
// Nothing else in either half of this project pins the *structure* of a
// payload. The server tests read values back out of their own output and the
// web tests read hand-written fixtures, so both halves verify themselves and
// neither notices when they stop agreeing.
//
// So: snapshot the key paths and leaf types of every response the SPA
// consumes, and nothing else. Not the values — those move with the fixture on
// every regeneration, and a test that has to be re-approved for a reason
// nobody reads is a test nobody reads. Renaming a field, dropping one,
// changing a string to a number or an enum from a bare string to a tagged
// object all show up here; a channel gaining a different name does not.

/// One line per distinct key path, with the set of leaf types seen at it.
///
/// Arrays collapse to `[]` and their elements are unioned, so a list of 52
/// channels yields one description rather than 52 — and a field that is null
/// in some rows and a string in others is recorded as `null|string`, which is
/// exactly what a client has to handle.
fn shape(value: &Value) -> String {
    fn kind(value: &Value) -> &'static str {
        match value {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    fn walk(value: &Value, path: &str, out: &mut BTreeMap<String, BTreeSet<&'static str>>) {
        match value {
            Value::Object(fields) => {
                if fields.is_empty() {
                    out.entry(path.to_owned()).or_default().insert("object");
                }
                for (key, child) in fields {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(child, &child_path, out);
                }
            }
            Value::Array(items) => {
                // An empty array describes nothing, and saying so is better
                // than an absent line that reads as "this field is gone".
                if items.is_empty() {
                    out.entry(format!("{path}[]")).or_default().insert("empty");
                }
                for item in items {
                    walk(item, &format!("{path}[]"), out);
                }
            }
            leaf => {
                out.entry(path.to_owned()).or_default().insert(kind(leaf));
            }
        }
    }

    let mut paths: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    walk(value, "", &mut paths);

    paths
        .into_iter()
        .map(|(path, kinds)| {
            let kinds = kinds.into_iter().collect::<Vec<_>>().join("|");
            format!(
                "{}: {kinds}\n",
                if path.is_empty() { "<root>" } else { &path }
            )
        })
        .collect()
}

/// Key paths whose only observed type is `null`, or arrays with no elements.
///
/// A snapshot of a shape with its nullable fields all null pins far less than
/// it appears to: `last_message: null` says nothing about whether the field is
/// a string, and a client written against it is written against a guess. So
/// these are counted, and the count has to be zero except for the paths named
/// below — each of which is a field that *cannot* carry a value in a response,
/// rather than one the seed happens not to fill.
fn unpinned(shape: &str) -> Vec<String> {
    let paths: Vec<&str> = shape
        .lines()
        .filter_map(|line| line.rsplit_once(": ").map(|(path, _)| path))
        .collect();

    shape
        .lines()
        .filter(|line| line.ends_with(": null") || line.ends_with(": empty"))
        .filter(|line| {
            // `override: null` beside `override.name: string` is a nullable
            // object that some row filled, not an unpinned field — the walk
            // records a path's own type only for a leaf or an empty container.
            // Same for an array of objects where one element happened to be an
            // empty list.
            let path = line.rsplit_once(": ").map(|(path, _)| path).unwrap_or(line);
            !paths.iter().any(|other| {
                other.starts_with(&format!("{path}.")) || other.starts_with(&format!("{path}["))
            })
        })
        .map(str::to_owned)
        .collect()
}

/// The only shapes allowed to stay unpinned, with the reason each one is.
fn allowed_unpinned(name: &str, path: &str) -> bool {
    match (name, path) {
        // No session is running, which is the whole reason `shape-session`
        // exists beside this.
        ("proxy-stats", "[]") => true,
        // A provider password is never echoed: the serializer replaces it with
        // `has_password`, and the field itself is always absent or null.
        (_, "results[].password") | (_, "password") => true,
        // `epg_match_ignore_*` are operator-authored lists and the shipped
        // default is empty. Seeding one would pin a list of strings; leaving
        // them empty is what a fresh install actually serves.
        (_, path) if path.starts_with("results[].value.epg_match_ignore") => true,
        _ => false,
    }
}

#[tokio::test]
async fn the_shapes_the_spa_reads_are_pinned() {
    // The synthetic seed, because a shape is only pinned by a row that fills
    // it: on `sample.sql` most nullable paths are null in every row, and a
    // snapshot of `null` pins nothing.
    let app = TestApp::synthetic().await;
    let token = match app.login_as(Principal::Admin).await {
        Credential::Bearer(token) => token,
        other => panic!("the admin did not get a bearer token: {other:?}"),
    };

    // Every endpoint the SPA's resource layer calls with a body worth
    // decoding. Writes and actions are covered by their own tests; what is
    // here is what a component destructures.
    //
    // The detail endpoints point at the rows chosen to be *complete*: channel
    // 1001 is the one with an override on every column, account 1001 is the one
    // with a server group and a filter, source 1001 is the active XMLTV one.
    let endpoints = [
        ("users", "/api/accounts/users/"),
        ("users-me", "/api/accounts/users/me/"),
        ("channels", "/api/channels/channels/"),
        ("channel-detail", "/api/channels/channels/1001/"),
        ("channel-streams", "/api/channels/channels/1000/streams/"),
        ("groups", "/api/channels/groups/"),
        ("logos", "/api/channels/logos/"),
        ("channel-profiles", "/api/channels/profiles/"),
        ("streams", "/api/channels/streams/"),
        ("jobs", "/api/core/jobs/"),
        ("backups", "/api/core/backups/"),
        ("settings", "/api/core/settings/"),
        ("settings-detail", "/api/core/settings/proxy_settings/"),
        ("stream-profiles", "/api/core/streamprofiles/"),
        ("system-events", "/api/core/system-events/"),
        ("version", "/api/core/version/"),
        ("epg-grid", "/api/epg/grid/"),
        ("epg-sources", "/api/epg/sources/"),
        ("epg-source-detail", "/api/epg/sources/1001/"),
        ("epg-data", "/api/epg/epgdata/"),
        ("m3u-accounts", "/api/m3u/accounts/"),
        ("m3u-account-detail", "/api/m3u/accounts/1001/"),
        ("m3u-account-groups", "/api/m3u/accounts/1001/groups/"),
        ("m3u-account-profiles", "/api/m3u/accounts/1001/profiles/"),
        ("m3u-account-filters", "/api/m3u/accounts/1001/filters/"),
        ("notifications", "/api/notifications/"),
        ("notifications-count", "/api/notifications/count/"),
        ("proxy-stats", "/api/proxy/stats/"),
    ];

    // Backups live on disk rather than in the seed, and an empty list pins
    // nothing.
    dollet_core::backup::take(
        &app.state.db,
        &app.state.config.backups_dir(),
        dollet_core::backup::Trigger::Manual,
    )
    .await
    .expect("a backup to list");

    let mut unpinned_paths: Vec<String> = Vec::new();
    // `users/me/` answers about whoever asked, and the admin is the one user
    // with no channel-profile restriction — so it is taken as the standard
    // user, whose row fills `channel_profiles`.
    let standard = match app.login_as(Principal::Standard).await {
        Credential::Bearer(token) => token,
        other => panic!("the standard user did not get a bearer token: {other:?}"),
    };

    for (name, uri) in endpoints {
        let token = if name == "users-me" {
            &standard
        } else {
            &token
        };
        let (status, body) = app.json("GET", uri, token, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let shape = shape(&body);
        for line in unpinned(&shape) {
            let path = line.rsplit_once(':').map(|(path, _)| path).unwrap_or(&line);
            if !allowed_unpinned(name, path) {
                unpinned_paths.push(format!("{uri}: {line}"));
            }
        }
        insta::assert_snapshot!(format!("shape-{name}"), shape, uri);
    }

    // Writes that answer with the thing they wrote. Reads alone left every one
    // of these unpinned, which is how a `PUT` returning a bare array survived
    // beside a `GET` on the same path returning an envelope.
    let writes: Vec<(&str, &str, &str, Value)> = vec![
        (
            "put-channel-streams",
            "PUT",
            "/api/channels/channels/1000/streams/",
            // The channel's own three, re-presented: the write has to answer in
            // the read's shape, which it cannot do from an empty list.
            json!({ "ids": [1001, 1002, 1000] }),
        ),
        (
            "patch-channel",
            "PATCH",
            "/api/channels/channels/1001/",
            json!({ "name": "Renamed By A Test" }),
        ),
        (
            "patch-settings",
            "PATCH",
            "/api/core/settings/proxy_settings/",
            json!({ "value": { "ring_seconds": 20 } }),
        ),
    ];

    for (name, method, uri, body) in writes {
        let (status, written) = app.json(method, uri, &token, Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {written}");
        let shape = shape(&written);
        for line in unpinned(&shape) {
            let path = line.rsplit_once(':').map(|(path, _)| path).unwrap_or(&line);
            if !allowed_unpinned(name, path) {
                unpinned_paths.push(format!("{method} {uri}: {line}"));
            }
        }
        insta::assert_snapshot!(format!("shape-{name}"), shape, uri);
    }

    assert!(
        unpinned_paths.is_empty(),
        "{} key path(s) are pinned as `null` or an empty array, which pins \
         nothing. Point the test at a row that fills them, or add the row to \
         the seed:\n  {}",
        unpinned_paths.len(),
        unpinned_paths.join("\n  ")
    );
}

/// `/api/proxy/stats/` is empty without a live session, which would pin
/// nothing.
///
/// Every optional field is filled, for the same reason the read snapshots are
/// taken on the synthetic seed: a session whose `media` is all null pins a
/// dozen key paths as "null" and tells the Stats page nothing about what it
/// will actually receive. The values are what a real ffmpeg session reports —
/// this is the one payload in the suite that no request can produce, because
/// producing it needs a provider.
#[tokio::test]
async fn the_live_session_shape_is_pinned() {
    // The synthetic seed, so the channel resolves to a programme that is
    // genuinely on now: `now_playing` has four shapes and the richest of them
    // is the one the page draws a progress bar from.
    let app = TestApp::synthetic().await;

    let session = dollet_stream::SessionStats {
        channel: synthetic::CHANNEL_UUID.parse().unwrap(),
        output: dollet_stream::OutputKey::Profile(1001),
        phase: dollet_stream::Phase::Streaming,
        healthy: true,
        source_index: 1,
        source_id: Some(1002),
        url: Some("https://provider.example/live/1.ts".into()),
        switches: 1,
        last_error: Some("input stalled for 15s".into()),
        started_at: chrono::Utc::now(),
        total_bytes: 4_194_304,
        buffer: dollet_stream::RingStats {
            chunks: 128,
            bytes: 3_145_728,
            head: 4096,
            oldest: Some(3968),
            seconds: 14.8,
        },
        media: dollet_stream::MediaInfo {
            input_format: Some("mpegts".into()),
            video_codec: Some("h264".into()),
            width: Some(1920),
            height: Some(1080),
            source_fps: Some(59.94),
            pixel_format: Some("yuv420p".into()),
            video_bitrate_kbps: Some(7800.0),
            audio_codec: Some("ac3".into()),
            sample_rate: Some(48000),
            audio_channels: Some("5.1".into()),
            audio_bitrate_kbps: Some(384.0),
            quality: Some("1080p".into()),
        },
        progress: dollet_stream::Progress {
            speed: Some(1.0),
            fps: Some(59.9),
            actual_fps: Some(59.9),
            bitrate_kbps: Some(8100.0),
        },
        clients: vec![
            dollet_stream::ClientStats {
                id: "cccccccc-0000-4000-8000-000000000001".parse().unwrap(),
                ip: Some("203.0.113.9".parse().unwrap()),
                user_agent: Some("Plex/1.40".into()),
                connected_at: chrono::Utc::now(),
                bytes_sent: 1_048_576,
                internal: false,
            },
            // The transcode behind an output profile: a consumer inside this
            // process, with no address and no way to disconnect it. The page
            // renders it as the reason a channel with no viewers is still up,
            // so its shape is part of the contract too.
            dollet_stream::ClientStats {
                id: "cccccccc-0000-4000-8000-000000000002".parse().unwrap(),
                ip: None,
                user_agent: None,
                connected_at: chrono::Utc::now(),
                bytes_sent: 2_097_152,
                internal: true,
            },
        ],
    };

    let enriched = super::stream::with_now_playing(&app.state, vec![session])
        .await
        .unwrap();
    let shape = shape(&json!(enriched));
    assert_eq!(
        unpinned(&shape),
        Vec::<String>::new(),
        "a live session still has fields nothing has ever filled"
    );
    insta::assert_snapshot!("shape-session", shape);
}

/// A bad CIDR is reported against the endpoint class it was typed into.
///
/// `detail` is unchanged and still carries the sentence, so a client that
/// knows nothing about `fields` keeps working; the map is additive.
#[tokio::test]
async fn an_invalid_cidr_is_reported_per_endpoint_class() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, body) = app
        .json(
            "PATCH",
            "/api/core/settings/network_access/",
            &token,
            Some(json!({ "value": { "UI": "10.0.0.0/8,nonsense", "STREAMS": "also-bad" } })),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"].as_str().is_some_and(|d| d.contains("CIDR")),
        "{body}"
    );
    assert_eq!(body["fields"]["UI"], "not a CIDR range: nonsense", "{body}");
    assert_eq!(
        body["fields"]["STREAMS"], "not a CIDR range: also-bad",
        "{body}"
    );
    // The valid half of a rejected patch is not reported, and not saved.
    assert!(body["fields"].get("XC_API").is_none(), "{body}");

    let (_, saved) = app
        .json("GET", "/api/core/settings/network_access/", &token, None)
        .await;
    assert!(
        saved["value"]
            .get("UI")
            .is_none_or(|ui| ui != "10.0.0.0/8,nonsense"),
        "a patch that was refused was saved anyway: {saved}"
    );
}

/// A write that answers with the thing it wrote answers in the shape a read of
/// that thing would.
///
/// A write whose response a client uses *instead of* re-fetching is exactly as
/// much a contract as the fetch it replaces, and the shape snapshots cover
/// reads only.
///
/// Asserted as agreement between the pair rather than against a snapshot of
/// each. The property is that the two cannot differ; two snapshots would catch
/// a drift only if someone read both files side by side, which is the thing
/// nobody does.
#[tokio::test]
async fn a_write_answers_in_the_same_shape_as_the_read_it_replaces() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let streams: Vec<i64> = sqlx::query_scalar("SELECT id FROM stream ORDER BY id LIMIT 2")
        .fetch_all(&app.state.db)
        .await
        .unwrap();

    let pairs: Vec<(&str, &str, Value, &str)> = vec![
        (
            "channel streams",
            "PUT",
            json!({ "ids": streams }),
            "/api/channels/channels/171/streams/",
        ),
        (
            "channel",
            "PATCH",
            json!({ "name": "Renamed By A Test" }),
            "/api/channels/channels/171/",
        ),
        (
            "settings group",
            "PATCH",
            json!({ "value": { "ring_seconds": 20 } }),
            "/api/core/settings/proxy_settings/",
        ),
        (
            "epg source",
            "PATCH",
            json!({ "priority": 3 }),
            "/api/epg/sources/1/",
        ),
        (
            "m3u account",
            "PATCH",
            json!({ "priority": 2 }),
            "/api/m3u/accounts/2/",
        ),
    ];

    for (name, method, body, uri) in pairs {
        // Written first, then read. The other order compares two different
        // states: a PATCH to an EPG source reschedules it, so `next_run_at`
        // goes from null to a timestamp between the calls and the types differ
        // for a reason that has nothing to do with the shape.
        let (write_status, written) = app.json(method, uri, &token, Some(body)).await;
        assert_eq!(write_status, StatusCode::OK, "{name} {method}: {written}");

        let (read_status, read) = app.json("GET", uri, &token, None).await;
        assert_eq!(read_status, StatusCode::OK, "{name} GET: {read}");

        assert_eq!(
            shape(&written),
            shape(&read),
            "{method} {uri} answers in a different shape than GET does"
        );
    }
}

// --- The authorization matrix -----------------------------------------------
//
// `contract.rs` proves every path the SPA calls is *served*, and that every
// path the client authenticates answers 401 to a request carrying nothing.
// Neither says anything about who a route lets in once a credential is
// present, and "present a credential" is the interesting half: a streamer with
// a valid token is not an anonymous request, and an API key is a second
// credential for the same person that has to reach exactly as far as their
// token and no further.
//
// So: every route the manifest lists, plus the public surface it deliberately
// omits, crossed with six principals. One table, one loop, and a guard that
// fails when the manifest grows a route this table does not name — because the
// failure mode of a hand-written matrix is not a wrong row, it is a missing
// one.

/// What a route requires of its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    /// No credential at all: Plex and every M3U client fetch these with
    /// nothing but a URL. The `network_access` allowlist is the only control
    /// in front of them and it is asserted separately.
    Public,
    /// Any active user, at any level.
    Authenticated,
    /// Admin, which is almost all of `/api/`.
    Admin,
    /// Unauthenticated and closed anyway: the instance already has an owner,
    /// so `initialize-superuser` refuses everyone for the rest of its life.
    Closed,
    /// Authenticates from the query string or the path, never from a header,
    /// and answers a refused caller with a 404 page rather than a
    /// 401 — a client that gets a 401 prompts for a password it was never
    /// given. No HTTP credential reaches these at all, so every principal gets
    /// the same answer and the assertion is that none of them gets a
    /// catalogue.
    XtreamQuery,
}

/// The answer a principal must get. "Admitted" is deliberately loose — a 400
/// for an empty body is a route that let the caller in and then disliked what
/// they sent, which is the same side of the line as a 200.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Admitted,
    Unauthorized,
    Forbidden,
    /// The answer to bad Xtream credentials.
    NotFound,
}

struct Route {
    method: &'static str,
    path: String,
    access: Access,
    /// The WebSocket is the one route whose credential does not travel in a
    /// header: a browser cannot set `Authorization` on an upgrade, so the
    /// token rides in `Sec-WebSocket-Protocol`. An API key has no such
    /// spelling, which is why the key is refused here and admitted everywhere
    /// else the standard user is.
    websocket: bool,
    /// Gets a private instance, because calling it invalidates the credential
    /// the rest of the pass is made with.
    solo: bool,
    /// What to send on a write. `{}` everywhere except where an empty object
    /// would be rejected by the body extractor *before* the gate under test.
    body: Option<Value>,
}

impl Route {
    fn new(method: &'static str, path: impl Into<String>, access: Access) -> Self {
        Self {
            method,
            path: path.into(),
            access,
            websocket: false,
            solo: false,
            body: None,
        }
    }

    fn websocket(mut self) -> Self {
        self.websocket = true;
        self
    }

    fn solo(mut self) -> Self {
        self.solo = true;
        self
    }

    fn with_body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    /// Reads, then writes, then deletes — within one principal, so an admin's
    /// DELETE cannot decide what that same admin's GET sees.
    fn order(&self) -> u8 {
        match self.method {
            "GET" => 0,
            "DELETE" => 2,
            _ => 1,
        }
    }

    fn expected(&self, who: Principal) -> Expect {
        // A deactivated account is not a level, it is a closed door: neither
        // of its credentials resolves to a user, so it is refused exactly
        // where an anonymous request is.
        let anonymous = matches!(who, Principal::Anonymous | Principal::InactiveAdmin);

        match self.access {
            Access::Public => Expect::Admitted,
            Access::Closed => Expect::Forbidden,
            Access::XtreamQuery => Expect::NotFound,
            Access::Authenticated if anonymous => Expect::Unauthorized,
            Access::Authenticated if self.websocket && who == Principal::StandardApiKey => {
                Expect::Unauthorized
            }
            Access::Authenticated => Expect::Admitted,
            Access::Admin if anonymous => Expect::Unauthorized,
            Access::Admin if who == Principal::Admin => Expect::Admitted,
            Access::Admin => Expect::Forbidden,
        }
    }
}

/// Every route, with ids that exist in the synthetic seed.
///
/// The ids are chosen so the admin's pass is survivable: it deletes the
/// streamer, the orphan logo, the `Kids` profile, the empty group, the
/// inactive guide source and the inactive provider account — rows nothing else
/// in the table reads.
fn authorization_matrix() -> Vec<Route> {
    use Access::{Admin, Authenticated, Closed, Public, XtreamQuery};
    // Not the manifest's name, as the ids here are not its `1`: the two meet
    // through `route_template`.
    const BACKUP: &str = "dollet-backup-20260101-000000-scheduled.zip";

    let mut routes = vec![
        // --- accounts
        Route::new("GET", "/api/accounts/initialize-superuser/", Public),
        // A *complete* body, unlike every other write here. The body extractor
        // runs before the "does this instance already have an owner" check, so
        // an empty object is refused as unparseable and says nothing about the
        // gate — which is the only thing standing between an anonymous caller
        // and a second admin.
        Route::new("POST", "/api/accounts/initialize-superuser/", Closed)
            .with_body(json!({ "username": "usurper", "password": "usurper-pass" })),
        Route::new("POST", "/api/accounts/token/", Public),
        Route::new("POST", "/api/accounts/token/refresh/", Public),
        Route::new("POST", "/api/accounts/auth/login/", Public),
        Route::new("POST", "/api/accounts/auth/logout/", Authenticated),
        Route::new("GET", "/api/accounts/api-keys/", Authenticated),
        // Both of these replace or destroy the caller's own API key, so they
        // get a private instance: run in line they would make every later row
        // of the API-key principal read 401, which looks exactly like a route
        // refusing someone it should admit.
        Route::new("POST", "/api/accounts/api-keys/generate/", Authenticated).solo(),
        Route::new("POST", "/api/accounts/api-keys/revoke/", Authenticated).solo(),
        Route::new("GET", "/api/accounts/users/", Admin),
        Route::new("POST", "/api/accounts/users/", Admin),
        Route::new("GET", "/api/accounts/users/me/", Authenticated),
        Route::new("PATCH", "/api/accounts/users/me/", Authenticated),
        Route::new("GET", "/api/accounts/users/1002/", Admin),
        Route::new("PATCH", "/api/accounts/users/1002/", Admin),
        Route::new("PUT", "/api/accounts/users/1002/", Admin),
        Route::new("DELETE", "/api/accounts/users/1002/", Admin),
        // --- channels
        Route::new("GET", "/api/channels/groups/", Admin),
        Route::new("POST", "/api/channels/groups/", Admin),
        Route::new("GET", "/api/channels/groups/1004/", Admin),
        Route::new("PATCH", "/api/channels/groups/1004/", Admin),
        Route::new("PUT", "/api/channels/groups/1004/", Admin),
        Route::new("DELETE", "/api/channels/groups/1004/", Admin),
        Route::new("POST", "/api/channels/groups/1004/renumber/", Admin),
        Route::new("POST", "/api/channels/groups/plan-ranges/", Admin)
            .with_body(json!({ "order": [] })),
        Route::new("POST", "/api/channels/groups/assign-ranges/", Admin)
            .with_body(json!({ "ranges": [] })),
        Route::new("POST", "/api/channels/groups/plan-renumber/", Admin),
        Route::new("POST", "/api/channels/groups/renumber-all/", Admin),
        Route::new("GET", "/api/channels/channels/", Admin),
        Route::new("POST", "/api/channels/channels/", Admin),
        Route::new("POST", "/api/channels/channels/bulk-delete/", Admin),
        Route::new("GET", "/api/channels/channels/1005/", Admin),
        Route::new("PATCH", "/api/channels/channels/1005/", Admin),
        Route::new("PUT", "/api/channels/channels/1005/", Admin),
        Route::new("DELETE", "/api/channels/channels/1005/", Admin),
        Route::new("GET", "/api/channels/channels/1005/streams/", Admin),
        Route::new("PUT", "/api/channels/channels/1005/streams/", Admin),
        Route::new("POST", "/api/channels/channels/1005/move/", Admin).with_body(json!({})),
        Route::new("GET", "/api/channels/profiles/", Admin),
        Route::new("POST", "/api/channels/profiles/", Admin),
        Route::new("GET", "/api/channels/profiles/1002/", Admin),
        Route::new("PATCH", "/api/channels/profiles/1002/", Admin),
        Route::new("PUT", "/api/channels/profiles/1002/", Admin),
        Route::new("DELETE", "/api/channels/profiles/1002/", Admin),
        Route::new(
            "POST",
            "/api/channels/profiles/1002/channels/bulk-update/",
            Admin,
        ),
        Route::new("PATCH", "/api/channels/profiles/1002/channels/1005/", Admin),
        Route::new("GET", "/api/channels/streams/", Admin),
        Route::new("POST", "/api/channels/streams/", Admin),
        Route::new("POST", "/api/channels/streams/bulk-delete/", Admin),
        Route::new("GET", "/api/channels/streams/1019/", Admin),
        Route::new("PATCH", "/api/channels/streams/1019/", Admin),
        Route::new("PUT", "/api/channels/streams/1019/", Admin),
        Route::new("DELETE", "/api/channels/streams/1019/", Admin),
        Route::new("GET", "/api/channels/logos/", Admin),
        Route::new("POST", "/api/channels/logos/", Admin),
        Route::new("POST", "/api/channels/logos/bulk-delete/", Admin),
        Route::new("POST", "/api/channels/logos/cleanup/", Admin),
        Route::new("GET", "/api/channels/logos/1003/", Admin),
        Route::new("PATCH", "/api/channels/logos/1003/", Admin),
        Route::new("PUT", "/api/channels/logos/1003/", Admin),
        Route::new("DELETE", "/api/channels/logos/1003/", Admin),
        // Deliberately unauthenticated: every output emits this URL as the
        // `tvg-logo`, the guide `<icon>` and Xtream's `stream_icon`, and the
        // clients fetching those carry no credential. It leaks a logo.
        Route::new("GET", "/api/channels/logos/1003/cache/", Public),
        Route::new("GET", "/api/channels/logos/1003/cache", Public),
        // --- core
        Route::new("GET", "/api/core/version/", Public),
        Route::new("GET", "/api/core/settings/", Admin),
        Route::new("GET", "/api/core/settings/env/", Admin),
        Route::new("GET", "/api/core/origins/", Admin),
        Route::new("GET", "/api/core/settings/proxy_settings/", Admin),
        Route::new("PATCH", "/api/core/settings/proxy_settings/", Admin),
        Route::new("PUT", "/api/core/settings/proxy_settings/", Admin),
        Route::new("GET", "/api/core/system-events/", Admin),
        Route::new("GET", "/api/core/jobs/", Admin),
        Route::new("GET", "/api/core/jobs/m3u_refresh%3A1001/", Admin),
        Route::new("POST", "/api/core/jobs/m3u_refresh%3A1001/cancel/", Admin),
        Route::new("GET", "/api/core/useragents/", Admin),
        Route::new("POST", "/api/core/useragents/", Admin),
        Route::new("GET", "/api/core/useragents/1001/", Admin),
        Route::new("PATCH", "/api/core/useragents/1001/", Admin),
        Route::new("PUT", "/api/core/useragents/1001/", Admin),
        Route::new("DELETE", "/api/core/useragents/1001/", Admin),
        Route::new("GET", "/api/core/streamprofiles/", Admin),
        Route::new("POST", "/api/core/streamprofiles/", Admin),
        Route::new("GET", "/api/core/streamprofiles/1001/", Admin),
        Route::new("PATCH", "/api/core/streamprofiles/1001/", Admin),
        Route::new("PUT", "/api/core/streamprofiles/1001/", Admin),
        Route::new("DELETE", "/api/core/streamprofiles/1001/", Admin),
        Route::new("GET", "/api/core/outputprofiles/", Admin),
        Route::new("POST", "/api/core/outputprofiles/", Admin),
        Route::new("GET", "/api/core/outputprofiles/1001/", Admin),
        Route::new("PATCH", "/api/core/outputprofiles/1001/", Admin),
        Route::new("PUT", "/api/core/outputprofiles/1001/", Admin),
        Route::new("DELETE", "/api/core/outputprofiles/1001/", Admin),
        // A backup is every password hash, provider credential and the session
        // signing key. The named one does not exist, so the admin's pass is a
        // 404 rather than a restart.
        Route::new("GET", "/api/core/backups/", Admin),
        Route::new("POST", "/api/core/backups/", Admin),
        Route::new("POST", "/api/core/backups/upload/", Admin),
        Route::new(
            "GET",
            format!("/api/core/backups/{BACKUP}/download/"),
            Admin,
        ),
        Route::new(
            "POST",
            format!("/api/core/backups/{BACKUP}/restore/"),
            Admin,
        ),
        Route::new("DELETE", format!("/api/core/backups/{BACKUP}/"), Admin),
        // --- epg
        Route::new("GET", "/api/epg/sources/", Admin),
        Route::new("POST", "/api/epg/sources/", Admin),
        Route::new("GET", "/api/epg/sources/1003/", Admin),
        Route::new("PATCH", "/api/epg/sources/1003/", Admin),
        Route::new("PUT", "/api/epg/sources/1003/", Admin),
        Route::new("DELETE", "/api/epg/sources/1003/", Admin),
        Route::new("GET", "/api/epg/grid/", Admin),
        Route::new("GET", "/api/epg/ambiguous/", Admin),
        Route::new("GET", "/api/epg/epgdata/", Admin),
        Route::new("POST", "/api/epg/match/", Admin),
        Route::new("POST", "/api/epg/suggestions/1002/", Admin),
        Route::new("DELETE", "/api/epg/suggestions/1002/", Admin),
        Route::new("POST", "/api/epg/refresh/1001/", Admin),
        Route::new("POST", "/api/epg/refresh/", Admin),
        // --- m3u
        Route::new("GET", "/api/m3u/accounts/", Admin),
        Route::new("POST", "/api/m3u/accounts/", Admin),
        Route::new("GET", "/api/m3u/accounts/1003/", Admin),
        Route::new("PATCH", "/api/m3u/accounts/1003/", Admin),
        Route::new("PUT", "/api/m3u/accounts/1003/", Admin),
        Route::new("DELETE", "/api/m3u/accounts/1003/", Admin),
        Route::new("GET", "/api/m3u/accounts/1001/profiles/", Admin),
        Route::new("POST", "/api/m3u/accounts/1001/profiles/", Admin),
        Route::new("PATCH", "/api/m3u/accounts/1001/profiles/1002/", Admin),
        Route::new("DELETE", "/api/m3u/accounts/1001/profiles/1002/", Admin),
        Route::new("GET", "/api/m3u/accounts/1001/filters/", Admin),
        Route::new("POST", "/api/m3u/accounts/1001/filters/", Admin),
        Route::new("DELETE", "/api/m3u/accounts/1001/filters/1001/", Admin),
        Route::new("GET", "/api/m3u/accounts/1001/groups/", Admin),
        Route::new("POST", "/api/m3u/accounts/1001/groups/", Admin),
        Route::new("GET", "/api/m3u/server-groups/", Admin),
        Route::new("POST", "/api/m3u/refresh/1003/", Admin),
        Route::new("POST", "/api/m3u/refresh/", Admin),
        // --- notifications
        //
        // Admin-only throughout: a notification names a provider account and
        // the pattern that would not compile, which are deployment facts.
        // 1001 is the recurring unacknowledged row, so acknowledging it and
        // then deleting it — in that order, which `Route::order` guarantees —
        // leaves the other two for the reads.
        Route::new("GET", "/api/notifications/", Admin),
        Route::new("GET", "/api/notifications/count/", Admin),
        Route::new("POST", "/api/notifications/1001/acknowledge/", Admin),
        Route::new("POST", "/api/notifications/acknowledge-all/", Admin),
        Route::new("DELETE", "/api/notifications/1001/", Admin),
        // --- the surface the SPA never calls
        Route::new("GET", "/health", Public),
        Route::new("GET", "/output/m3u", Public),
        Route::new("GET", "/output/m3u/Living%20Room", Public),
        Route::new("GET", "/output/epg", Public),
        Route::new("GET", "/output/epg/Living%20Room", Public),
        // The streamless channel, so an admitted caller gets a 400 rather than
        // this test opening sixteen sessions against a provider that is not
        // there.
        Route::new(
            "GET",
            format!("/proxy/ts/stream/{}", synthetic::STREAMLESS_UUID),
            Public,
        ),
        Route::new("GET", "/api/proxy/stats/", Admin),
        Route::new(
            "POST",
            format!("/api/proxy/ts/stop/{}", synthetic::CHANNEL_UUID),
            Admin,
        ),
        Route::new(
            "POST",
            format!("/api/proxy/ts/stop_client/{}", synthetic::CHANNEL_UUID),
            Admin,
        ),
        Route::new(
            "POST",
            format!("/api/proxy/ts/next_stream/{}", synthetic::CHANNEL_UUID),
            Admin,
        ),
        Route::new(
            "POST",
            format!("/api/proxy/ts/change_stream/{}", synthetic::CHANNEL_UUID),
            Admin,
        ),
        Route::new("GET", "/ws", Authenticated).websocket(),
        // --- Xtream Codes
        Route::new("GET", "/player_api.php", XtreamQuery),
        Route::new("POST", "/player_api.php", XtreamQuery),
        Route::new("GET", "/panel_api.php", XtreamQuery),
        Route::new("POST", "/panel_api.php", XtreamQuery),
        Route::new("GET", "/get.php", XtreamQuery),
        Route::new("GET", "/xmltv.php", XtreamQuery),
        Route::new("GET", "/live/nobody/nothing/1000", XtreamQuery),
        Route::new("GET", "/nobody/nothing/1000", XtreamQuery),
    ];

    // Thirteen HDHomeRun paths: a channel profile and an output profile each
    // make a distinct tuner and Plex addresses them by URL prefix.
    routes.push(Route::new("GET", "/hdhr/device.xml", Public));
    for prefix in [
        "/hdhr",
        "/hdhr/Living%20Room",
        "/hdhr/output_profile/1001",
        "/hdhr/Living%20Room/output_profile/1001",
    ] {
        for leaf in ["discover.json", "lineup.json", "lineup_status.json"] {
            routes.push(Route::new("GET", format!("{prefix}/{leaf}"), Public));
        }
    }

    routes
}

/// A path with its ids replaced, so a route in the manifest and the same route
/// in the table above compare equal despite pointing at different rows.
fn route_template(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            let job_key = segment.replace("%3A", ":");
            if segment.is_empty() {
                segment.to_owned()
            } else if segment.chars().all(|c| c.is_ascii_digit()) {
                "{id}".to_owned()
            } else if uuid::Uuid::parse_str(segment).is_ok() {
                "{uuid}".to_owned()
            } else if dollet_core::backup::BackupName::parse(segment).is_some() {
                "{backup}".to_owned()
            } else if job_key
                .rsplit_once(':')
                .is_some_and(|(kind, id)| !kind.is_empty() && id.parse::<i64>().is_ok())
            {
                "{job}".to_owned()
            } else {
                segment.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The manifest is generated from the SPA's own resource layer, so a route
/// added to the client and not to the table above is a route the matrix
/// silently stops covering. This is what stops that being silent.
#[test]
fn the_matrix_covers_every_route_the_manifest_lists() {
    #[derive(serde::Deserialize)]
    struct Manifest {
        routes: Vec<ManifestRoute>,
    }
    #[derive(serde::Deserialize)]
    struct ManifestRoute {
        name: String,
        method: String,
        path: String,
    }

    let raw = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/route-manifest.json"),
    )
    .expect("route-manifest.json");
    let manifest: Manifest = serde_json::from_str(&raw).expect("route-manifest.json");

    let covered: BTreeSet<String> = authorization_matrix()
        .iter()
        .map(|route| format!("{} {}", route.method, route_template(&route.path)))
        .collect();

    let missing: Vec<String> = manifest
        .routes
        .iter()
        .filter(|route| {
            !covered.contains(&format!("{} {}", route.method, route_template(&route.path)))
        })
        .map(|route| format!("{} {} ({})", route.method, route.path, route.name))
        .collect();

    assert!(
        missing.is_empty(),
        "{} route(s) the SPA calls are outside the authorization matrix:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

/// Every route, every principal, one expected answer each.
///
/// A fresh instance per principal: the admin's pass deletes rows, and sharing
/// one database would make the streamer's answers depend on whether the admin
/// had run yet.
#[tokio::test]
async fn every_route_admits_exactly_the_principals_it_should() {
    let mut wrong: Vec<String> = Vec::new();

    for who in Principal::ALL {
        let app = TestApp::synthetic().await;
        let credential = app.login_as(who).await;

        let mut routes = authorization_matrix();
        routes.sort_by_key(Route::order);

        for route in &routes {
            let expected = route.expected(who);
            // The admin's press of "Back up now" holds the process-wide
            // single-flight, which a backup test running alongside would read
            // as its own 409.
            let _serial = match route.path.starts_with("/api/core/backups/") {
                true => Some(backups::SERIAL.lock().await),
                false => None,
            };
            let status = match route.solo {
                true => {
                    let app = TestApp::synthetic().await;
                    let credential = app.login_as(who).await;
                    app.answer_for(route, &credential).await
                }
                false => app.answer_for(route, &credential).await,
            };

            let matched = match expected {
                Expect::Admitted => {
                    status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN
                }
                Expect::Unauthorized => status == StatusCode::UNAUTHORIZED,
                Expect::Forbidden => status == StatusCode::FORBIDDEN,
                Expect::NotFound => status == StatusCode::NOT_FOUND,
            };

            if !matched {
                wrong.push(format!(
                    "{} {} as {}: expected {expected:?}, got {status}",
                    route.method,
                    route.path,
                    who.name()
                ));
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "{} route/principal pair(s) answered the wrong side of the line:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
}

impl TestApp {
    /// One matrix cell: the status this route gives this credential.
    ///
    /// A body is sent on every write, so a route that would otherwise answer
    /// 415 for a missing content type answers about the *caller* instead —
    /// which is the only thing this test is asking about.
    async fn answer_for(&self, route: &Route, credential: &Credential) -> StatusCode {
        if route.websocket {
            let mut builder = Request::builder().method(route.method).uri(&route.path);
            if let Credential::Bearer(token) = credential {
                builder = builder.header("sec-websocket-protocol", format!("auth.jwt, {token}"));
            }
            return self.send(builder.body(Body::empty()).unwrap()).await.0;
        }

        let body = (route.method != "GET").then(|| route.body.clone().unwrap_or_else(|| json!({})));
        self.request_as(credential, route.method, &route.path, body)
            .await
            .0
    }
}

// --- The outputs, on the second seed ----------------------------------------
//
// The golden corpus pins our bytes for `sample.sql`'s 49 lineup channels, every
// one of which has an integer number, a group, a stream with a URL and no
// override, so it says nothing about a fractional number, a channel with no
// streams, a guide channel with no programmes, or a dummy source.
//
// These assert content: which channels appear, in what order, with which
// numbers and URLs and programmes. Our own output is read back through
// `dollet_core::parse`, so a change to the serializer that a string search
// would not notice fails here instead.

/// The visible lineup of the synthetic seed, in the order every output must
/// use: numbered channels ascending, then the unnumbered one, never first.
const SYNTHETIC_LINEUP: [(&str, &str); 16] = [
    ("1", "Synth One"),
    ("2.5", "Synth Two & A Half"),
    ("3", "Synth \"Quoted\" Channel"),
    ("4", "Synth <Angle> & Ampersand"),
    ("5", "Synth Ünïcøde Ñoise"),
    ("6", "  Synth Padded  "),
    ("8", "Synth Adult"),
    ("9", "Synth Admin Only"),
    ("10", "Synth Standard Only"),
    ("11", "Synth Dummy Guide"),
    ("12", "Synth Gap Guide"),
    ("13", "Synth Redirect"),
    ("14", "Synth Streamless"),
    ("15", "Synth Catchup Agent"),
    ("16", "Synth Null Url"),
    ("", "Synth Unnumbered"),
];

fn lineup_entries(body: &Value) -> Vec<(String, String, String)> {
    body.as_array()
        .expect("lineup is an array")
        .iter()
        .map(|entry| {
            (
                entry["GuideNumber"].as_str().unwrap_or_default().to_owned(),
                entry["GuideName"].as_str().unwrap_or_default().to_owned(),
                entry["URL"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_hdhr_lineup_carries_the_same_channels_in_every_scope_that_holds_them() {
    let app = TestApp::synthetic().await;

    // A channel with no formattable number is skipped outright: `GuideNumber`
    // is how Plex addresses a channel and an empty one is unplayable. That is
    // the one place the lineup differs from the playlist.
    let numbered: Vec<(String, String)> = SYNTHETIC_LINEUP
        .iter()
        .filter(|(number, _)| !number.is_empty())
        .map(|(number, name)| ((*number).to_owned(), (*name).to_owned()))
        .collect();

    let (status, body) = app.public("/hdhr/lineup.json").await;
    assert_eq!(status, StatusCode::OK);
    let bare = lineup_entries(&body);
    assert_eq!(
        bare.iter()
            .map(|(n, name, _)| (n.clone(), name.clone()))
            .collect::<Vec<_>>(),
        numbered
    );

    // The channel-profile scope is the same lineup narrowed, in the same
    // order — not a re-sort.
    let (status, body) = app.public("/hdhr/Living%20Room/lineup.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        lineup_entries(&body)
            .iter()
            .map(|(n, name, _)| (n.clone(), name.clone()))
            .collect::<Vec<_>>(),
        numbered[..3],
        "the profile scope is a different lineup, not a subset of the same one"
    );

    // The output-profile scope changes the URLs and nothing else.
    let (status, body) = app.public("/hdhr/output_profile/1001/lineup.json").await;
    assert_eq!(status, StatusCode::OK);
    let scoped = lineup_entries(&body);
    assert_eq!(scoped.len(), bare.len());
    for ((number, name, url), (bare_number, bare_name, bare_url)) in scoped.iter().zip(&bare) {
        assert_eq!((number, name), (bare_number, bare_name));
        assert_eq!(
            *url,
            bare_url.replace("?output_profile=1", "?output_profile=1001")
        );
    }

    // Both scopes at once.
    let (status, body) = app
        .public("/hdhr/Living%20Room/output_profile/1001/lineup.json")
        .await;
    assert_eq!(status, StatusCode::OK);
    let both = lineup_entries(&body);
    assert_eq!(both.len(), 3);
    assert!(
        both.iter()
            .all(|(_, _, url)| url.ends_with("?output_profile=1001")),
        "{both:?}"
    );

    // Every URL is absolute against the `Host` the request arrived on. Getting
    // this wrong fails as "discovery works, playback doesn't".
    assert!(
        bare.iter()
            .all(|(_, _, url)| url.starts_with("http://ipx.test:9191/proxy/ts/stream/")),
        "{bare:?}"
    );

    // A profile name nobody has is an empty lineup, not a 404: Plex treats a
    // 404 as a dead tuner and stops asking.
    let (status, body) = app.public("/hdhr/Nowhere/lineup.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));

    // Each scope is a distinct tuner, and advertises the prefix it arrived on.
    let mut devices = BTreeSet::new();
    for prefix in [
        "/hdhr",
        "/hdhr/Living%20Room",
        "/hdhr/output_profile/1001",
        "/hdhr/Living%20Room/output_profile/1001",
    ] {
        let (status, discover) = app.public(&format!("{prefix}/discover.json")).await;
        assert_eq!(status, StatusCode::OK, "{prefix}");
        assert_eq!(
            discover["BaseURL"],
            format!("http://ipx.test:9191{prefix}"),
            "{prefix}"
        );
        // Four active provider profiles allowing 2 + 1 + 1 streams, plus the
        // one hand-added stream on the locked `custom` account. The inactive
        // account contributes nothing.
        assert_eq!(discover["TunerCount"], 5, "{prefix}");
        devices.insert(discover["DeviceID"].as_str().unwrap().to_owned());
    }
    assert_eq!(devices.len(), 4, "two scopes share a DeviceID: {devices:?}");
}

/// The setting exists so an operator who wants Plex transcoded does not have to
/// re-add the tuner under a longer URL. No channel in the sample uses an output
/// profile, so this runs on the synthetic seed.
#[tokio::test]
async fn the_lineup_falls_back_to_the_configured_output_profile_and_the_url_still_wins() {
    let app = TestApp::synthetic().await;

    let (_, body) = app.public("/hdhr/lineup.json").await;
    assert!(
        lineup_entries(&body)
            .iter()
            .all(|(_, _, url)| url.ends_with("?output_profile=1")),
        "the seeded `hdhr_output_profile_id` did not reach the lineup URLs"
    );

    // The URL scope is the more specific request and the only thing a second
    // tuner can differ by, so it wins.
    let (_, body) = app.public("/hdhr/output_profile/1001/lineup.json").await;
    assert!(
        lineup_entries(&body)
            .iter()
            .all(|(_, _, url)| url.ends_with("?output_profile=1001")),
        "the setting overrode the URL"
    );

    // An output profile the operator deleted is dropped rather than carried
    // through, so the lineup falls back to no transcoding instead of pointing
    // every entry at something the stream endpoint will ignore.
    sqlx::query("UPDATE core_setting SET value = json_set(value, '$.hdhr_output_profile_id', 424242) WHERE key = 'stream_settings'")
        .execute(&app.state.db)
        .await
        .unwrap();
    let (_, body) = app.public("/hdhr/lineup.json").await;
    assert!(
        lineup_entries(&body)
            .iter()
            .all(|(_, _, url)| !url.contains("output_profile")),
        "a deleted output profile reached the lineup URLs"
    );
}

/// `hidden_from_output` is the one exclusion a user cannot undo after the fact:
/// a channel that leaks into a public playlist has leaked.
///
/// `user_level` is a different story and this pins it deliberately. The
/// credential-less outputs have no principal to compare a level against, so an
/// admin-only channel appearing there is the only thing they *can* do. The
/// Xtream API does have one and still does not filter on it — a documented gap
/// rather than a silent one, and the assertion below is what makes it fail
/// loudly if anyone changes their mind.
#[tokio::test]
async fn a_hidden_channel_reaches_no_output_and_user_level_gates_none_of_them() {
    let app = TestApp::synthetic().await;

    let (_, lineup) = app.public("/hdhr/lineup.json").await;
    let (_, _, playlist) = app.raw("/output/m3u").await;
    let (_, _, guide) = app.raw("/output/epg").await;
    let (_, live) = app
        .public(&format!(
            "/player_api.php?username={}&password={}&action=get_live_streams",
            synthetic::ADMIN.0,
            synthetic::ADMIN_XC_PASSWORD
        ))
        .await;

    for (surface, text) in [
        ("hdhr", lineup.to_string()),
        ("m3u", playlist.clone()),
        ("epg", guide.clone()),
        ("xtream", live.to_string()),
    ] {
        assert!(
            !text.contains("Synth Hidden"),
            "the hidden channel reached the {surface} output"
        );
        assert!(
            !text.contains(synthetic::HIDDEN_UUID),
            "the hidden channel's stream URL reached the {surface} output"
        );
        assert!(
            text.contains("Synth Admin Only"),
            "the {surface} output started filtering on channel user_level; if that \
             is intended, this test is the place to say so"
        );
    }
}

#[tokio::test]
async fn fractional_and_absent_channel_numbers_reach_each_output_in_its_own_form() {
    let app = TestApp::synthetic().await;

    // HDHomeRun: the fraction survives as text, and the numberless channel is
    // absent because `GuideNumber` is how Plex addresses a channel.
    let (_, body) = app.public("/hdhr/lineup.json").await;
    let numbers: Vec<String> = lineup_entries(&body)
        .into_iter()
        .map(|(number, _, _)| number)
        .collect();
    assert!(numbers.contains(&"2.5".to_owned()), "{numbers:?}");
    assert!(numbers.contains(&"1".to_owned()), "{numbers:?}");
    assert!(!numbers.iter().any(|n| n == "1.0"), "{numbers:?}");

    // M3U: `tvg-chno` carries the fraction, and the numberless channel gets an
    // empty one plus its row id as `tvg-id`, because the default id source is
    // the channel number and it has none.
    let (_, _, text) = app.raw("/output/m3u").await;
    let playlist = dollet_core::parse::m3u::parse_str(&text);
    assert_eq!(playlist.entries.len(), SYNTHETIC_LINEUP.len());

    let fractional = playlist
        .entries
        .iter()
        .find(|e| e.display_name == "Synth Two & A Half")
        .expect("the fractional channel");
    assert_eq!(fractional.attr("tvg-chno"), Some("2.5"));
    assert_eq!(fractional.attr("tvg-id"), Some("2.5"));
    // And the override reached it: the base row says `RAWSTATION`.
    assert_eq!(
        fractional.attr("tvc-guide-stationid"),
        Some("SYNTHOVERRIDE")
    );

    let unnumbered = playlist
        .entries
        .iter()
        .find(|e| e.display_name == "Synth Unnumbered")
        .expect("the unnumbered channel");
    assert_eq!(unnumbered.attr("tvg-chno"), Some(""));
    assert_eq!(unnumbered.attr("tvg-id"), Some("1002"));
    assert_eq!(
        playlist.entries.last().map(|e| e.display_name.as_str()),
        Some("Synth Unnumbered"),
        "the unnumbered channel led the playlist, which is what SQLite's NULL \
         ordering does when nothing corrects it"
    );

    // Xtream addresses a channel by a bare integer, so both of these have to be
    // flattened without colliding with a channel that legitimately holds the
    // integer. 2.5 takes 2, which is free; the numberless one takes the first
    // integer above every claimed number.
    let (_, live) = app
        .public(&format!(
            "/player_api.php?username={}&password={}&action=get_live_streams",
            synthetic::ADMIN.0,
            synthetic::ADMIN_XC_PASSWORD
        ))
        .await;
    let by_name: BTreeMap<&str, &Value> = live
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| (entry["name"].as_str().unwrap(), entry))
        .collect();
    assert_eq!(by_name["Synth Two & A Half"]["num"], 2);
    // 7 rather than 17: the hidden channel is dropped before the numbers are
    // assigned, so the integer it held is free. The allocation is stable
    // rather than clever — `num` is what an Xtream client stores as a
    // channel's identity, so a
    // different answer renumbers every channel in every client at cutover.
    assert_eq!(by_name["Synth Unnumbered"]["num"], 7);
    // The guide is keyed by that same number, or an Xtream client joins the two
    // payloads on nothing.
    assert_eq!(by_name["Synth Unnumbered"]["epg_channel_id"], "7");
}

#[tokio::test]
async fn the_playlist_honours_direct_cached_logos_and_a_named_profile() {
    let app = TestApp::synthetic().await;

    let (status, headers, text) = app.raw("/output/m3u").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "audio/x-mpegurl");
    let default = dollet_core::parse::m3u::parse_str(&text);

    let one = default
        .entries
        .iter()
        .find(|e| e.display_name == "Synth One")
        .expect("channel 1000");
    assert_eq!(
        one.url,
        format!(
            "http://ipx.test:9191/proxy/ts/stream/{}",
            synthetic::CHANNEL_UUID
        )
    );
    // Cached logos are the default: the provider's CDN is
    // slow, rate-limited, and sometimes gone.
    assert_eq!(
        one.attr("tvg-logo"),
        Some("http://ipx.test:9191/api/channels/logos/1001/cache/")
    );

    // `?direct=true` hands the client the provider instead of proxying it, and
    // it is the *first* source in the failover order that gets handed over.
    let (_, _, text) = app.raw("/output/m3u?direct=true").await;
    let direct = dollet_core::parse::m3u::parse_str(&text);
    assert_eq!(
        direct
            .entries
            .iter()
            .find(|e| e.display_name == "Synth One")
            .unwrap()
            .url,
        "https://provider.example/live/synthuser/synthpass/1001.ts"
    );
    // A channel whose only stream has no URL has nothing to hand over, so it
    // keeps the proxy URL rather than emitting an empty line.
    assert_eq!(
        direct
            .entries
            .iter()
            .find(|e| e.display_name == "Synth Null Url")
            .unwrap()
            .url,
        format!(
            "http://ipx.test:9191/proxy/ts/stream/{}",
            synthetic::NULL_URL_UUID
        )
    );

    // `?cachedlogos=false` points the client back at the provider's own URL.
    let (_, _, text) = app.raw("/output/m3u?cachedlogos=false").await;
    let raw_logos = dollet_core::parse::m3u::parse_str(&text);
    assert_eq!(
        raw_logos
            .entries
            .iter()
            .find(|e| e.display_name == "Synth One")
            .unwrap()
            .attr("tvg-logo"),
        Some("https://logos.example/synth-sports.png")
    );
    // And the guide URL the playlist advertises carries the same choice, or a
    // client following it gets the other kind of artwork.
    assert!(
        raw_logos.header["x-tvg-url"].ends_with("/output/epg?cachedlogos=false"),
        "{:?}",
        raw_logos.header
    );

    // A named profile narrows the playlist to that profile's enabled members.
    let (status, _, text) = app.raw("/output/m3u/Living%20Room").await;
    assert_eq!(status, StatusCode::OK);
    let scoped = dollet_core::parse::m3u::parse_str(&text);
    assert_eq!(
        scoped
            .entries
            .iter()
            .map(|e| e.display_name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Synth One",
            "Synth Two & A Half",
            "Synth \"Quoted\" Channel",
            "Synth Unnumbered",
        ]
    );

    // Unlike the HDHR lineup, a name nobody has is a 404 here: a client asking
    // for a named playlist has been misconfigured, and an empty playlist reads
    // as "the provider has no channels today".
    assert_eq!(
        app.raw("/output/m3u/Nowhere").await.0,
        StatusCode::NOT_FOUND
    );

    // An output profile reaches every stream URL in the playlist, which is the
    // other way a client asks for a transcode.
    let (_, _, text) = app.raw("/output/m3u?output_profile=1001").await;
    let transcoded = dollet_core::parse::m3u::parse_str(&text);
    assert!(
        transcoded
            .entries
            .iter()
            .all(|e| e.url.ends_with("?output_profile=1001")),
        "{:?}",
        transcoded.entries.first().map(|e| &e.url)
    );
}

/// The artwork base a split deployment needs: reachable from a browser,
/// unlike the docker address the Plex server fetches everything else on.
const ARTWORK: &str = "https://tv.example.com";

/// Each channel's `<icon src>`, keyed by the guide id — the half of the guide
/// `guide_of` throws away.
async fn guide_icons(app: &TestApp, uri: &str) -> BTreeMap<String, String> {
    use dollet_core::parse::xmltv::XmltvItem;

    let (status, _, text) = app.raw(uri).await;
    assert_eq!(status, StatusCode::OK, "{uri}");

    let mut reader = dollet_core::parse::xmltv::from_bytes(text.as_bytes()).expect("our own guide");
    let mut icons = BTreeMap::new();
    while let Some(item) = reader.next_item().expect("our own guide parses") {
        if let XmltvItem::Channel(channel) = item {
            icons.insert(channel.tvg_id, channel.icon_url.unwrap_or_default());
        }
    }
    icons
}

#[tokio::test]
async fn the_playlist_moves_artwork_to_the_configured_base_and_leaves_the_streams_alone() {
    let mut app = TestApp::synthetic().await;

    // The pairing is the whole point. Plex fetches the playlist and the streams
    // itself over whatever network it reaches this server on; the logos are
    // rendered by a browser somewhere else entirely.
    app.set_artwork_base_url(Some(format!("{ARTWORK}/")));

    let (status, _, text) = app.raw("/output/m3u").await;
    assert_eq!(status, StatusCode::OK);
    let playlist = dollet_core::parse::m3u::parse_str(&text);
    let one = playlist
        .entries
        .iter()
        .find(|e| e.display_name == "Synth One")
        .expect("channel 1000");

    assert_eq!(
        one.attr("tvg-logo"),
        Some("https://tv.example.com/api/channels/logos/1001/cache/")
    );
    assert_eq!(
        one.url,
        format!(
            "http://ipx.test:9191/proxy/ts/stream/{}",
            synthetic::CHANNEL_UUID
        )
    );
    // And the guide this playlist points a player at is fetched by the player,
    // not by a browser, so it stays on the request's own address too.
    assert_eq!(
        playlist.header["x-tvg-url"],
        "http://ipx.test:9191/output/epg"
    );

    // Unset, both halves are the request origin, exactly as before.
    app.set_artwork_base_url(None);
    let (_, _, text) = app.raw("/output/m3u").await;
    let plain = dollet_core::parse::m3u::parse_str(&text);
    let one = plain
        .entries
        .iter()
        .find(|e| e.display_name == "Synth One")
        .unwrap();
    assert_eq!(
        one.attr("tvg-logo"),
        Some("http://ipx.test:9191/api/channels/logos/1001/cache/")
    );
    assert_eq!(
        one.url,
        format!(
            "http://ipx.test:9191/proxy/ts/stream/{}",
            synthetic::CHANNEL_UUID
        )
    );
}

#[tokio::test]
async fn the_guide_icons_move_to_the_configured_base_and_nothing_else_in_it_does() {
    let mut app = TestApp::synthetic().await;

    let before = guide_icons(&app, "/output/epg").await;
    assert_eq!(
        before["1"],
        "http://ipx.test:9191/api/channels/logos/1001/cache/"
    );

    app.set_artwork_base_url(Some(ARTWORK.into()));

    // Freshly rendered, not read back from the file the request above wrote:
    // the artwork base is in the cache key precisely so that setting it is not
    // followed by five minutes of the old answer.
    let after = guide_icons(&app, "/output/epg").await;
    assert_eq!(
        after["1"],
        "https://tv.example.com/api/channels/logos/1001/cache/"
    );
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>()
    );

    // A guide carries no stream URLs, so the pair to assert is the other way
    // round: the request's own address must now appear nowhere in it.
    let (_, _, text) = app.raw("/output/epg").await;
    assert!(
        !text.contains("ipx.test"),
        "the request origin survived in the guide"
    );
}

#[tokio::test]
async fn the_xtream_catalogue_moves_artwork_but_not_the_streams_it_hands_out() {
    let mut app = TestApp::synthetic().await;
    app.set_artwork_base_url(Some(ARTWORK.into()));

    let credentials = format!(
        "username={}&password={}",
        synthetic::ADMIN.0,
        synthetic::ADMIN_XC_PASSWORD
    );

    let (_, live) = app
        .public(&format!(
            "/player_api.php?{credentials}&action=get_live_streams"
        ))
        .await;
    let one = live
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "Synth One")
        .expect("channel 1000");
    assert_eq!(
        one["stream_icon"],
        "https://tv.example.com/api/channels/logos/1001/cache/"
    );

    // `get.php` is the same catalogue as a playlist, and it is where both URL
    // classes appear side by side.
    let (status, _, text) = app.raw(&format!("/get.php?{credentials}")).await;
    assert_eq!(status, StatusCode::OK);
    let playlist = dollet_core::parse::m3u::parse_str(&text);
    let one = playlist
        .entries
        .iter()
        .find(|e| e.display_name == "Synth One")
        .unwrap();
    assert_eq!(
        one.attr("tvg-logo"),
        Some("https://tv.example.com/api/channels/logos/1001/cache/")
    );
    assert!(
        one.url.starts_with("http://ipx.test:9191/live/"),
        "{}",
        one.url
    );

    // Unset, the catalogue is unchanged from what every Xtream client already
    // stores.
    app.set_artwork_base_url(None);
    let (_, live) = app
        .public(&format!(
            "/player_api.php?{credentials}&action=get_live_streams"
        ))
        .await;
    assert_eq!(
        live.as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "Synth One")
            .unwrap()["stream_icon"],
        "http://ipx.test:9191/api/channels/logos/1001/cache/"
    );
}

/// Every guide channel and programme our own writer produced, read back through
/// our own parser.
async fn guide_of(app: &TestApp, uri: &str) -> (BTreeSet<String>, Vec<(String, String, String)>) {
    use dollet_core::parse::xmltv::XmltvItem;

    let (status, headers, text) = app.raw(uri).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    assert_eq!(headers["content-type"], "application/xml");

    let mut reader = dollet_core::parse::xmltv::from_bytes(text.as_bytes()).expect("our own guide");
    let mut channels = BTreeSet::new();
    let mut programmes = Vec::new();
    while let Some(item) = reader.next_item().expect("our own guide parses") {
        match item {
            XmltvItem::Channel(channel) => {
                channels.insert(channel.tvg_id);
            }
            XmltvItem::Programme(programme) => programmes.push((
                programme.tvg_id,
                programme.title,
                programme.start_time.to_rfc3339(),
            )),
        }
    }
    (channels, programmes)
}

#[tokio::test]
async fn the_guide_generates_for_the_dummy_channel_and_publishes_nothing_for_the_gap() {
    let app = TestApp::synthetic().await;
    let (channels, programmes) = guide_of(&app, "/output/epg").await;

    // Every visible channel is declared, including the ones with no listings:
    // a `<channel>` with no `<programme>` is a channel Plex can still tune.
    assert_eq!(channels.len(), SYNTHETIC_LINEUP.len());
    assert!(channels.contains("11"), "the dummy channel: {channels:?}");
    assert!(channels.contains("12"), "the gap channel: {channels:?}");
    assert!(!channels.contains("7"), "the hidden channel was declared");

    let for_channel = |id: &str| -> Vec<&(String, String, String)> {
        programmes.iter().filter(|(c, _, _)| c == id).collect()
    };

    // Channel 11 is on the dummy source, so it has no `program` rows at all and
    // its listings are generated: three days of four-hour blocks.
    let dummy = for_channel("11");
    assert_eq!(
        dummy.len(),
        18,
        "the dummy channel's generated listings changed shape"
    );
    assert!(
        dummy
            .iter()
            .all(|(_, title, _)| title == "Synth Dummy Guide"),
        "a generated block is not titled after its channel"
    );

    // Channel 12 maps to guide data the feed carried nothing for. That is
    // genuinely nothing on, and it must not be confused with the dummy case.
    assert!(for_channel("12").is_empty(), "{:?}", for_channel("12"));

    // Channel 1, on stored programmes, gets the four around now.
    let titles: Vec<&str> = for_channel("1")
        .iter()
        .map(|(_, title, _)| title.as_str())
        .collect();
    assert_eq!(
        titles,
        vec![
            "Synth Just Ended",
            "Synth On Now",
            "Synth Overlapping",
            "Synth After The Gap",
        ]
    );

    // A channel with no guide data at all publishes no programmes, rather than
    // silently borrowing another channel's.
    assert!(for_channel("3").is_empty());
}

#[tokio::test]
async fn the_guide_window_is_clamped_and_its_timestamps_stay_utc() {
    let app = TestApp::synthetic().await;

    // The default window reaches a year out, so the programme a month away is
    // published.
    let (_, programmes) = guide_of(&app, "/output/epg").await;
    assert!(
        programmes
            .iter()
            .any(|(_, title, _)| title == "Synth Far Future"),
        "the default window dropped a programme a month out"
    );

    // `?days=1` bounds it, which is the whole point of the parameter.
    let (_, programmes) = guide_of(&app, "/output/epg?days=1").await;
    assert!(
        !programmes
            .iter()
            .any(|(_, title, _)| title == "Synth Far Future"),
        "?days=1 published a programme a month out"
    );
    assert!(
        programmes
            .iter()
            .any(|(_, title, _)| title == "Synth On Now"),
        "?days=1 dropped what is on now"
    );

    // `?days=` is client-supplied and unauthenticated, and `chrono` panics
    // rather than saturating when a duration overflows the date — on the
    // stored half *and* on the dummy generator, which walks one step per day.
    // `sample.sql` has no dummy source, so only this seed reaches that half.
    let (_, _, text) = app.raw("/output/epg?days=4000000000").await;
    let (_, programmes) = guide_of(&app, "/output/epg?days=4000000000").await;
    assert!(!text.is_empty());
    let horizon = chrono::Utc::now() + chrono::Duration::days(15);
    for (channel, title, start) in &programmes {
        let start: chrono::DateTime<chrono::Utc> = start.parse().unwrap();
        assert!(
            start < horizon,
            "{title} on {channel} starts past the fourteen-day clamp"
        );
    }

    // Ours are UTC. XMLTV timestamps carry their own offset, so a client
    // renders them in whatever zone it likes and there is nothing for this
    // instance to have an opinion about — which is why there is no time-zone
    // setting. Pinned so that emitting local times becomes a decision rather
    // than a drift.
    let (_, _, text) = app.raw("/output/epg").await;
    assert!(text.contains("+0000"), "no UTC offsets in the guide at all");
    assert!(
        !text.contains("-0400") && !text.contains("-0500"),
        "the guide started emitting local times"
    );
}

#[tokio::test]
async fn the_xtream_api_narrows_to_the_callers_profile_and_answers_on_its_own_password() {
    let app = TestApp::synthetic().await;

    let url = |user: &str, password: &str, action: &str| {
        format!("/player_api.php?username={user}&password={password}&action={action}")
    };

    // Categories are the groups that still hold a visible channel, ranked by
    // the lowest channel number in each — the order a client lists them in.
    let (status, categories) = app
        .public(&url(
            synthetic::ADMIN.0,
            synthetic::ADMIN_XC_PASSWORD,
            "get_live_categories",
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        categories
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["category_name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["Synth Sports", "Synth News", "Synth Adults", "Synth Empty"],
        "an empty group appeared, or the ranking is not by channel number"
    );

    // The standard user is pinned to one channel profile and sees that and
    // nothing else — both the catalogue and the categories derived from it.
    let (_, live) = app
        .public(&url(
            synthetic::STANDARD.0,
            synthetic::STANDARD_XC_PASSWORD,
            "get_live_streams",
        ))
        .await;
    assert_eq!(
        live.as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "Synth One",
            "Synth Two & A Half",
            "Synth \"Quoted\" Channel",
            "Synth Unnumbered",
        ]
    );
    let (_, categories) = app
        .public(&url(
            synthetic::STANDARD.0,
            synthetic::STANDARD_XC_PASSWORD,
            "get_live_categories",
        ))
        .await;
    assert_eq!(
        categories
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["category_name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["Synth Sports", "Synth News"]
    );

    // The login password is not the Xtream password, and offering it gets
    // a 404 page rather than a 401 — a client that gets a 401 prompts
    // for a password it was never given.
    let (status, _, body) = app
        .raw(&url(
            synthetic::STANDARD.0,
            synthetic::STANDARD.1,
            "get_live_streams",
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Not Found"), "{body}");

    // A stream the caller's profile does not contain is not theirs to play,
    // and the answer is the same 404 — not a 403, which would confirm it
    // exists.
    let (status, _, _) = app
        .raw(&format!(
            "/live/{}/{}/1009",
            synthetic::STANDARD.0,
            synthetic::STANDARD_XC_PASSWORD
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a profile-restricted user played a channel outside their profile"
    );

    // `server_info` reports the port the URLs in the same response actually
    // use. Read off the `Host` header, a request without an explicit port would
    // report "80" while every URL beside it carries :9191.
    let (_, account) = app
        .public(&url(
            synthetic::STANDARD.0,
            synthetic::STANDARD_XC_PASSWORD,
            "get_account_info",
        ))
        .await;
    assert_eq!(account["server_info"]["port"], "9191");
    assert_eq!(account["server_info"]["url"], "ipx.test");
    assert_eq!(account["user_info"]["username"], synthetic::STANDARD.0);
    // A personal cap wins over what the providers allow; without one the answer
    // is the providers' total.
    assert_eq!(account["user_info"]["max_connections"], "1");
    let (_, account) = app
        .public(&url(
            synthetic::ADMIN.0,
            synthetic::ADMIN_XC_PASSWORD,
            "get_account_info",
        ))
        .await;
    assert_eq!(account["user_info"]["max_connections"], "5");

    // `get.php` addresses channels by row id under the caller's credentials,
    // and its guide URL carries them too.
    let (status, _, text) = app
        .raw(&format!(
            "/get.php?username={}&password={}&type=m3u_plus",
            synthetic::STANDARD.0,
            synthetic::STANDARD_XC_PASSWORD
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let playlist = dollet_core::parse::m3u::parse_str(&text);
    assert_eq!(playlist.entries.len(), 4);
    assert_eq!(
        playlist.entries[0].url,
        format!(
            "http://ipx.test:9191/live/{}/{}/1000",
            synthetic::STANDARD.0,
            synthetic::STANDARD_XC_PASSWORD
        )
    );
}

#[tokio::test]
async fn the_stream_endpoint_resolves_the_failover_order_across_both_accounts() {
    let app = TestApp::synthetic().await;

    let channel = dollet_core::db::channels::get_effective_by_uuid(
        &app.state.db,
        synthetic::CHANNEL_UUID.parse().unwrap(),
    )
    .await
    .unwrap()
    .expect("channel 1000");

    let sources = super::stream::sources_for(&app.state, &channel)
        .await
        .unwrap();

    // Position 0 is tried first, and the order is the operator's, not the
    // accounts' or the ids'.
    assert_eq!(
        sources.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![1001, 1002, 1000]
    );
    assert_eq!(
        sources.iter().map(|s| s.url.as_str()).collect::<Vec<_>>(),
        vec![
            "https://provider.example/live/synthuser/synthpass/1001.ts",
            "https://xtream.example:8080/live/synthxc/synthxcpass/2002.ts",
            "https://provider.example/custom/hand-added.ts",
        ]
    );

    // Two accounts means two budgets. `max_streams = 0` means
    // "unlimited" and must not become a limit of zero, which would refuse every
    // connection instead.
    let limits: Vec<Option<(i64, u32)>> = sources
        .iter()
        .map(|s| s.limit.as_ref().map(|l| (l.key, l.max_streams)))
        .collect();
    assert_eq!(limits, vec![Some((1001, 2)), Some((1002, 1)), None]);

    // The user agent resolves through the channel's stream profile, which is
    // the only thing in the seed naming this one.
    let agent = dollet_core::db::channels::get_effective(&app.state.db, 1015)
        .await
        .unwrap()
        .unwrap();
    let agent_sources = super::stream::sources_for(&app.state, &agent)
        .await
        .unwrap();
    assert_eq!(agent_sources[0].user_agent, "SynthPlayer/1.0 (dollet-test)");

    // The `redirect` profile takes this server out of the data path: a 302 to
    // the provider, no ring allocated and no bytes proxied.
    let (status, headers, _) = app
        .raw(&format!("/proxy/ts/stream/{}", synthetic::REDIRECT_UUID))
        .await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        headers["location"],
        "https://provider.example/live/synthuser/synthpass/1015.ts"
    );

    // A channel with no streams is a client error, not a 500.
    let (status, _) = app
        .public(&format!("/proxy/ts/stream/{}", synthetic::STREAMLESS_UUID))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // And a channel whose only stream has no URL is the same answer by a
    // different route: `sources_for` drops it, so the channel is reachable and
    // unplayable at once.
    assert!(
        super::stream::sources_for(
            &app.state,
            &dollet_core::db::channels::get_effective(&app.state.db, 1016)
                .await
                .unwrap()
                .unwrap(),
        )
        .await
        .unwrap()
        .is_empty()
    );
    let (status, _) = app
        .public(&format!("/proxy/ts/stream/{}", synthetic::NULL_URL_UUID))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// --- Provider ingest, on hand-written feeds ---------------------------------
//
// `fixtures/ingest/` pins the reconciliation decisions over one hand-written
// feed, at the level of the pure `sync` functions. It says nothing about the
// job handlers around them, and nothing about the shapes it does not carry:
// `#EXTGRP`, a group a filter excludes, an XMLTV served as xz, a file that is
// UTF-16, a download that stopped half-way.
//
// So these drive the real job handlers over hand-written feeds on the
// synthetic seed.
//
// **They read from a file rather than over HTTP, and that is not laziness.**
// `allow_private` is false for every provider fetch — an EPG or M3U URL is
// configuration whose *content* the provider controls — and `http::get` judges
// a bare address before dialling it, so the loopback a mock server runs on is
// refused at hop zero. A file-backed source is a real, supported configuration
// and reaches the same reconciler; what it cannot exercise is the transport,
// and `provider_fetches_cannot_reach_loopback` pins why.

fn provider_fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/synthetic/providers")
        .join(name)
        .canonicalize()
        .unwrap_or_else(|e| panic!("fixtures/synthetic/providers/{name}: {e}"))
        .to_string_lossy()
        .into_owned()
}

/// Point a seeded provider account at one of the hand-written playlists.
async fn account_reading(app: &TestApp, id: i64, fixture: &str) -> dollet_core::domain::M3uAccount {
    let mut account = dollet_core::db::m3u::get_account(&app.state.db, id)
        .await
        .unwrap()
        .expect("a seeded account");
    account.file_path = Some(provider_fixture(fixture));
    dollet_core::db::m3u::save_account(&app.state.db, &account)
        .await
        .unwrap()
}

/// The same for a guide source.
async fn source_reading(app: &TestApp, id: i64, fixture: &str) -> dollet_core::domain::EpgSource {
    let mut source = dollet_core::db::epg::get_source(&app.state.db, id)
        .await
        .unwrap()
        .expect("a seeded source");
    source.file_path = Some(provider_fixture(fixture));
    dollet_core::db::epg::save_source(&app.state.db, &source)
        .await
        .unwrap()
}

/// A URL in `playlist.m3u`, which is also a URL the seed already stores for
/// the first four.
fn feed_url(id: i64) -> String {
    format!("https://provider.example/live/synthuser/synthpass/{id}.ts")
}

async fn stream_by_url(app: &TestApp, url: &str) -> Option<dollet_core::domain::Stream> {
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM stream WHERE url = ?")
        .bind(url)
        .fetch_optional(&app.state.db)
        .await
        .unwrap();
    match id {
        Some(id) => dollet_core::db::streams::get(&app.state.db, id)
            .await
            .unwrap(),
        None => None,
    }
}

#[tokio::test]
async fn an_m3u_refresh_reads_every_attribute_form_the_parser_supports() {
    let app = TestApp::synthetic().await;
    let account = account_reading(&app, 1001, "playlist.m3u").await;
    let handle = super::jobs::test_handle(&app.state, &super::ingest::m3u::job_key(1001));

    let before: BTreeMap<String, i64> = sqlx::query_as(
        "SELECT url, id FROM stream WHERE m3u_account_id = 1001 AND url IS NOT NULL",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap()
    .into_iter()
    .collect();

    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect("the refresh ran");

    // Re-presenting a URL already stored has to resolve to the row that is
    // already there. Anything else orphans the catalogue and empties every
    // channel's failover list, which is the failure this whole hash exists to
    // prevent — and the seed carries real hashes, so the match is the
    // reconciler's rather than a coincidence.
    for id in [1001, 1003, 1004, 1005] {
        let url = feed_url(id);
        let stream = stream_by_url(&app, &url).await.expect(&url);
        assert_eq!(Some(stream.id), before.get(&url).copied(), "{url}");
    }

    // Quoted attributes, the full set.
    let quoted = stream_by_url(&app, &feed_url(1001)).await.unwrap();
    assert_eq!(quoted.name, "Synth One HD");
    assert_eq!(quoted.tvg_id.as_deref(), Some("synth.sports"));
    assert_eq!(
        quoted.logo_url.as_deref(),
        Some("https://logos.example/synth-sports.png")
    );
    assert_eq!(quoted.stream_chno, Some(1.0));

    // Unquoted values, which providers emit as often as not.
    let unquoted = stream_by_url(&app, &feed_url(1003)).await.unwrap();
    assert_eq!(unquoted.tvg_id.as_deref(), Some("synth.news"));
    assert_eq!(unquoted.stream_chno, Some(2.5));

    // `#EXTGRP` where there is no `group-title`, trimmed.
    let extgrp = stream_by_url(&app, &feed_url(1004)).await.unwrap();
    assert_eq!(
        group_name(&app, extgrp.channel_group_id).await,
        "Synth News"
    );

    // Player directives between the `#EXTINF` and its URL must not detach the
    // two.
    let with_directives = stream_by_url(&app, &feed_url(1005)).await.unwrap();
    assert_eq!(with_directives.name, "Synth Quoted Feed");

    // No attributes at all: the entry still lands, in the default group.
    let bare = stream_by_url(&app, &feed_url(2001))
        .await
        .expect("the attribute-less entry");
    assert_eq!(bare.name, "Synth Bare Entry");
    assert_eq!(
        group_name(&app, bare.channel_group_id).await,
        "Default Group"
    );

    // `channel-number` is the other spelling of `tvg-chno`.
    let other_spelling = stream_by_url(&app, &feed_url(2002)).await.unwrap();
    assert_eq!(other_spelling.stream_chno, Some(42.0));

    // Catch-up is carried into the columns even though 1.0 advertises none of
    // it, so adding the feature later is not a backfill out of JSON.
    let catchup = stream_by_url(&app, &feed_url(2003)).await.unwrap();
    assert!(catchup.is_catchup);
    assert_eq!(catchup.catchup_days, 7);

    // A quote and non-ASCII in a display name reach the row intact. The hash
    // escapes them as `\uXXXX` because Python's `json.dumps` does; the column
    // does not.
    let unicode = stream_by_url(&app, &feed_url(2005)).await.unwrap();
    assert_eq!(unicode.name, "Synth \"Ünïcøde\" Feed");

    // VLC's multicast `@` is stripped on the way in: ffmpeg does not
    // understand it, and the engine dials the URL exactly as stored.
    assert!(
        stream_by_url(&app, "udp://239.255.0.1:1234")
            .await
            .is_some(),
        "the multicast entry was stored with VLC's `@` or not at all"
    );

    // The group the account's filter excludes is gone before anything is
    // written — not filtered out of the output afterwards.
    assert!(
        stream_by_url(&app, &feed_url(2004)).await.is_none(),
        "the filtered group's entry was stored"
    );
    assert!(summary.contains("1 filtered out"), "{summary}");

    // A provider listing one channel under two categories is one row.
    assert!(summary.contains("1 duplicate entries"), "{summary}");

    // An `#EXTINF` with no URL is counted and must not swallow the entry below
    // it.
    assert!(
        stream_by_url(&app, &feed_url(2006)).await.is_some(),
        "the orphaned #EXTINF swallowed the entry that followed it"
    );

    // Auto channel sync, on the one group configured for it: a new stream gets
    // a channel inside the group's range, numbered from the provider's own
    // `tvg-chno` and renamed by the group's pattern. One channel, not one per
    // new stream — the other new entries are in groups that do not sync.
    let created: Vec<(f64, String)> = sqlx::query_as(
        "SELECT channel_number, name FROM channel WHERE auto_created = 1 AND id > 1016",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap();
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(created[0].0, 210.0);
    assert_eq!(
        created[0].1, "Auto Created Feed",
        "the group's rename did not apply"
    );

    // Twelve of the account's streams are absent from this playlist and long
    // past `stale_stream_days`, so the plan deletes them — and every one is the
    // only stream on its channel, so every one is kept and held stale instead.
    // Deleting them cascades the assignment away and leaves a channel that
    // still appears in every output and plays nothing.
    let spared: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM stream WHERE m3u_account_id = 1001 AND is_stale = 1
         AND id IN (SELECT stream_id FROM channel_stream) ORDER BY id",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap();
    assert_eq!(spared, (1006..=1017).collect::<Vec<i64>>());

    // The one with no channel to protect it does go.
    assert!(
        stream_by_url(&app, &feed_url(1018)).await.is_none(),
        "a stream in a disabled group with no channel on it survived"
    );
}

async fn group_name(app: &TestApp, id: Option<i64>) -> String {
    match id {
        Some(id) => sqlx::query_scalar("SELECT name FROM channel_group WHERE id = ?")
            .bind(id)
            .fetch_one(&app.state.db)
            .await
            .unwrap(),
        None => String::new(),
    }
}

/// The credential rotation, on rows rather than on fields.
///
/// An Xtream provider rotates the username and password embedded in every
/// stream URL, so `sync::hash` substitutes the provider's own `stream_id` for
/// the URL on an Xtream account. `a_rotation_of_the_synthetic_credentials_moves_no_hash`
/// proves the hashes hold across that; this proves the catalogue does — the
/// rows keep their ids, their channel assignments, and nothing is reported new.
///
/// The catalogue arrives from a file, for the reason at the top of this
/// section: `catalogue` fetches, and a provider fetch cannot reach a mock
/// server. The reconciler sees `StreamFields` whichever way a catalogue
/// arrives, and the file carries the `stream_id` attribute
/// `StreamFields::from_entry` reads into `provider_stream_id`, so what is under
/// test — Xtream-shaped fields whose URL moved and whose id did not — is the
/// same either way.
#[tokio::test]
async fn an_xtream_credential_rotation_keeps_the_rows_it_matched() {
    let app = TestApp::synthetic().await;
    let handle = super::jobs::test_handle(&app.state, &super::ingest::m3u::job_key(1002));

    let account = account_reading(&app, 1002, "xtream-playlist.m3u").await;
    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect("the first refresh ran");
    assert!(summary.starts_with("3 streams: 2 new"), "{summary}");

    // The seed's own Xtream stream, matched rather than rebuilt.
    let before = xtream_rows(&app).await;
    assert_eq!(before.len(), 3);
    assert!(
        before.iter().any(|(id, ..)| *id == 1002),
        "the seeded Xtream stream was replaced rather than matched: {before:?}"
    );

    // The provider rotates. Every URL changes; nothing else does.
    let rotated = app.state.config.cache_dir().join("xtream-rotated.m3u");
    std::fs::create_dir_all(rotated.parent().unwrap()).unwrap();
    let catalogue = std::fs::read_to_string(provider_fixture("xtream-playlist.m3u")).unwrap();
    std::fs::write(
        &rotated,
        catalogue.replace("synthxc/synthxcpass", "synthxc2/rotated-in-the-night"),
    )
    .unwrap();

    let mut account = account.clone();
    account.file_path = Some(rotated.to_string_lossy().into_owned());
    let account = dollet_core::db::m3u::save_account(&app.state.db, &account)
        .await
        .unwrap();

    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect("the second refresh ran");
    assert!(summary.starts_with("3 streams: 0 new"), "{summary}");

    let after = xtream_rows(&app).await;
    for ((id, hash, url), (rotated_id, rotated_hash, rotated_url)) in before.iter().zip(&after) {
        assert_eq!(id, rotated_id, "a row moved when the credentials rotated");
        assert_eq!(
            hash, rotated_hash,
            "a hash moved when the credentials rotated"
        );
        assert_ne!(url, rotated_url, "the rotation changed no URL");
    }
    assert!(
        after
            .iter()
            .all(|(_, _, url)| url.contains("rotated-in-the-night")),
        "{after:?}"
    );

    // And channel 1000's failover list still names the row it named before,
    // which is the thing a rebuilt catalogue actually costs the user.
    let assigned: Vec<i64> = sqlx::query_scalar(
        "SELECT stream_id FROM channel_stream WHERE channel_id = 1000 ORDER BY sort_order",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap();
    assert_eq!(assigned, vec![1001, 1002, 1000]);

    // The substitution is a choice, not an accident: the same file under a
    // *standard* account keys on the URL, so the same rotation orphans
    // everything it just matched.
    let mut standard = account.clone();
    standard.account_type = dollet_core::domain::M3uAccountType::Standard;
    let standard = dollet_core::db::m3u::save_account(&app.state.db, &standard)
        .await
        .unwrap();
    let summary = super::ingest::m3u::refresh(&app.state, &standard, &handle)
        .await
        .unwrap();
    assert!(summary.starts_with("3 streams: 3 new"), "{summary}");
}

/// Every stream on the Xtream account, in a stable order.
async fn xtream_rows(app: &TestApp) -> Vec<(i64, String, String)> {
    sqlx::query_as(
        "SELECT id, stream_hash, url FROM stream
         WHERE m3u_account_id = 1002 AND is_stale = 0 ORDER BY id",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap()
}

/// Every `stream_hash` the seed carries is the one the reconciler would compute
/// for that row.
///
/// A hand-written seed can hold a hash that matches nothing, and the failure is
/// invisible: a refresh test still passes, it just proves the *rebuild* path
/// rather than the match it claims to. Four of these are pinned by
/// `an_m3u_refresh_reads_every_attribute_form_the_parser_supports` re-presenting
/// their URLs; this covers the rest, including the Xtream row whose key is a
/// stream id rather than a URL.
#[tokio::test]
async fn the_seeded_hashes_are_the_ones_the_reconciler_computes() {
    use dollet_core::sync::hash;

    #[derive(sqlx::FromRow)]
    struct Row {
        id: i64,
        name: String,
        url: Option<String>,
        tvg_id: Option<String>,
        group: String,
        m3u_account_id: i64,
        account_type: String,
        stream_id: Option<i64>,
        stream_hash: Option<String>,
    }

    let app = TestApp::synthetic().await;
    let keys = hash::parse_keys("url");

    let rows: Vec<Row> = sqlx::query_as(
        "SELECT s.id, s.name, s.url, s.tvg_id, COALESCE(g.name, 'Default Group') AS 'group',
                s.m3u_account_id, a.account_type, s.stream_id, s.stream_hash
         FROM stream s
         JOIN m3u_account a ON a.id = s.m3u_account_id
         LEFT JOIN channel_group g ON g.id = s.channel_group_id
         ORDER BY s.id",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 21);

    for row in rows {
        // The hand-added stream is on the locked built-in account, which no
        // refresh ever reconciles, so it has no dedup key at all. That is the
        // shape, not an omission.
        if row.m3u_account_id == 1 {
            assert_eq!(row.stream_hash, None, "the hand-added stream grew a hash");
            continue;
        }

        let account_type = match row.account_type.as_str() {
            "xtream_codes" => dollet_core::domain::M3uAccountType::XtreamCodes,
            _ => dollet_core::domain::M3uAccountType::Standard,
        };
        let expected = hash::stream_hash(
            &hash::StreamIdentity {
                name: &row.name,
                url: row.url.as_deref().unwrap_or_default(),
                tvg_id: row.tvg_id.as_deref().unwrap_or_default(),
                group: &row.group,
                m3u_account_id: row.m3u_account_id,
                account_type,
                provider_stream_id: row.stream_id,
            },
            &keys,
        );
        assert_eq!(
            row.stream_hash.as_deref(),
            Some(expected.as_str()),
            "stream {} ({}) carries a made-up hash",
            row.id,
            row.name
        );
    }
}

/// The property the whole hash exists for: running the same feed twice changes
/// nothing.
#[tokio::test]
async fn the_same_playlist_twice_writes_nothing_the_second_time() {
    let app = TestApp::synthetic().await;
    let account = account_reading(&app, 1001, "playlist.m3u").await;
    let handle = super::jobs::test_handle(&app.state, &super::ingest::m3u::job_key(1001));

    super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .unwrap();
    let after_first: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, stream_hash FROM stream ORDER BY id")
            .fetch_all(&app.state.db)
            .await
            .unwrap();

    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .unwrap();
    let after_second: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, stream_hash FROM stream ORDER BY id")
            .fetch_all(&app.state.db)
            .await
            .unwrap();

    assert_eq!(
        after_first, after_second,
        "a second identical refresh moved rows"
    );
    assert!(summary.contains("0 new"), "{summary}");
}

/// Three encodings of the same document have to produce the same guide.
///
/// Providers serve XMLTV as plain text, gzip and xz interchangeably, and
/// neither the URL extension nor the `Content-Type` says which — so the
/// encoding is sniffed from the leading bytes, and a test that only ever sees
/// plain text never reaches the sniffing.
#[tokio::test]
async fn a_guide_reads_the_same_whether_it_is_plain_gzip_or_xz() {
    let mut rows: Vec<Vec<(String, String, String)>> = Vec::new();

    for fixture in ["guide.xml", "guide.xml.gz", "guide.xml.xz"] {
        let app = TestApp::synthetic().await;
        let source = source_reading(&app, 1001, fixture).await;
        let handle = super::jobs::test_handle(&app.state, &super::ingest::epg::job_key(1001));

        let summary = super::ingest::epg::refresh(&app.state, &source, &handle)
            .await
            .unwrap_or_else(|e| panic!("{fixture}: {e}"));
        assert!(
            summary.starts_with("5 guide channels"),
            "{fixture}: {summary}"
        );

        rows.push(
            sqlx::query_as(
                "SELECT d.tvg_id, p.title, p.start_time FROM program p
                 JOIN epg_data d ON d.id = p.epg_data_id
                 ORDER BY p.start_time, p.title",
            )
            .fetch_all(&app.state.db)
            .await
            .unwrap(),
        );
    }

    assert_eq!(rows[0], rows[1], "gzip and plain disagree");
    assert_eq!(rows[0], rows[2], "xz and plain disagree");

    // And the guide is the one the file describes: programmes only for the
    // guide channels a channel maps to, entities decoded, and both awkward
    // timestamp forms normalised to UTC.
    let titles: Vec<&str> = rows[0].iter().map(|(_, title, _)| title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["Rock & Roll <Live>", "No Offset At All", "Offset Not UTC"],
        "a programme for an unmapped guide channel was stored, or an entity survived"
    );
    // `20260915140000 -0400` is 18:00 UTC, and a timestamp with no zone at all
    // is read as UTC rather than as the server's local time.
    assert_eq!(rows[0][2].2, "2026-09-15 18:00:00.000+00:00");
    assert_eq!(rows[0][1].2, "2026-09-15 18:00:00.000+00:00");
}

/// UTF-16 is a documented gap, not a silent one: both parsers assume UTF-8 and
/// a UTF-16 document parses as an *empty* guide, which reads as "the provider
/// published nothing today" and replaces a working guide with nothing.
#[tokio::test]
async fn a_utf16_feed_is_refused_by_name_and_leaves_the_guide_alone() {
    let app = TestApp::synthetic().await;
    let handle = super::jobs::test_handle(&app.state, &super::ingest::epg::job_key(1001));

    let good = source_reading(&app, 1001, "guide.xml").await;
    super::ingest::epg::refresh(&app.state, &good, &handle)
        .await
        .unwrap();
    let before = programme_titles(&app).await;
    assert!(!before.is_empty(), "the first refresh stored nothing");

    let bad = source_reading(&app, 1001, "guide-utf16.xml").await;
    let error = super::ingest::epg::refresh(&app.state, &bad, &handle)
        .await
        .expect_err("a UTF-16 feed was accepted");
    assert!(
        error.to_string().contains("UTF-16"),
        "the error does not say what is wrong: {error}"
    );

    assert_eq!(programme_titles(&app).await, before);
}

/// A download that stopped part-way must leave the previous guide whole: a
/// partial replacement plus a job row saying "failed" reads as "nothing
/// happened".
#[tokio::test]
async fn a_truncated_feed_leaves_the_previous_guide_whole() {
    let app = TestApp::synthetic().await;
    let handle = super::jobs::test_handle(&app.state, &super::ingest::epg::job_key(1001));

    let good = source_reading(&app, 1001, "guide.xml").await;
    super::ingest::epg::refresh(&app.state, &good, &handle)
        .await
        .unwrap();
    let before = programme_titles(&app).await;
    assert_eq!(before.len(), 3);

    let truncated = source_reading(&app, 1001, "guide-truncated.xml").await;
    let outcome = super::ingest::epg::refresh(&app.state, &truncated, &handle).await;
    assert!(
        outcome.is_err(),
        "a feed cut off mid-element was reported as a complete guide: {outcome:?}"
    );

    // Whole, not a prefix.
    assert_eq!(
        programme_titles(&app).await,
        before,
        "the truncated feed replaced the guide with however much of it had arrived"
    );

    // Two guards stand between the user and half a guide, and a feed cut this
    // early stops at the first: the `<channel>` pass reads the whole document
    // before a single programme is staged, so this refresh failed having
    // written nothing at all. The second guard — staged rows discarded rather
    // than promoted — catches a failure after that point and is exercised by
    // `a_cancelled_epg_refresh_leaves_the_previous_guide_whole`.
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM program_incoming")
            .fetch_one(&app.state.db)
            .await
            .unwrap(),
        0,
        "the failed refresh left staged rows behind"
    );

    // The committed fixture is cut *inside* a tag, which is a parse error. A
    // feed cut at an element boundary is a different story and this pins it: the
    // document is still well-formed as far as the reader is concerned — only the
    // unclosed `<tv>` gives it away, and `quick-xml` does not check end names —
    // so it is accepted as a complete, shorter guide and replaces the stored
    // one.
    //
    // A documented gap rather than a silent one. Closing it means turning on
    // end-name checking in the reader, which belongs with the parser; this
    // assertion is what turns that from a change into a decision.
    let boundary = app
        .state
        .config
        .cache_dir()
        .join("guide-cut-at-boundary.xml");
    std::fs::create_dir_all(boundary.parent().unwrap()).unwrap();
    let whole = std::fs::read_to_string(provider_fixture("guide.xml")).unwrap();
    let cut = whole.find("<programme").unwrap();
    std::fs::write(&boundary, &whole[..cut]).unwrap();

    let mut source = dollet_core::db::epg::get_source(&app.state.db, 1001)
        .await
        .unwrap()
        .unwrap();
    source.file_path = Some(boundary.to_string_lossy().into_owned());
    let source = dollet_core::db::epg::save_source(&app.state.db, &source)
        .await
        .unwrap();

    super::ingest::epg::refresh(&app.state, &source, &handle)
        .await
        .expect("a guide cut at an element boundary parses");
    assert!(
        programme_titles(&app).await.is_empty(),
        "a truncated guide must store nothing"
    );
}

async fn programme_titles(app: &TestApp) -> Vec<String> {
    sqlx::query_scalar("SELECT title FROM program ORDER BY start_time, title")
        .fetch_all(&app.state.db)
        .await
        .unwrap()
}

/// A provider that fails mid-refresh is reported, and applied to nothing.
///
/// The mock server answers 500 and this test never reaches it: `allow_private`
/// is false for provider feeds, loopback is blocked regardless of that flag,
/// and `http::get` judges a bare address before dialling it. It is stood up
/// anyway so that the target is a real listening port — the refusal below is
/// then demonstrably the guard's rather than a connection that was going to
/// fail on its own.
///
/// What is pinned is the half that does not depend on the transport: a fetch
/// that returns no feed fails the refresh, names the URL with the provider
/// password removed, and leaves every row where it was. `download`'s own
/// `!status.is_success()` branch stays a documented gap — reaching it needs a
/// mock a provider fetch is allowed to dial, and
/// `provider_fetches_cannot_reach_loopback` is why there cannot be one.
#[tokio::test]
async fn a_provider_that_fails_mid_refresh_is_reported_and_applied_to_nothing() {
    let app = TestApp::synthetic().await;
    let provider = FakeProvider::new().await;
    use wiremock::matchers::{method, path as path_matcher};
    wiremock::Mock::given(method("GET"))
        .and(path_matcher("/get.php"))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .mount(&provider.server)
        .await;

    let mut account = dollet_core::db::m3u::get_account(&app.state.db, 1001)
        .await
        .unwrap()
        .unwrap();
    account.file_path = None;
    account.server_url = Some(format!(
        "{}/get.php?username=synthuser&password=synthpass",
        provider.server.uri()
    ));
    let account = dollet_core::db::m3u::save_account(&app.state.db, &account)
        .await
        .unwrap();

    let before: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM stream ORDER BY id")
        .fetch_all(&app.state.db)
        .await
        .unwrap();

    let handle = super::jobs::test_handle(&app.state, &super::ingest::m3u::job_key(1001));
    let error = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect_err("a failing provider was reported as a successful refresh");

    // The message is persisted in `job.last_error`, served as `last_message`,
    // and pushed to every `/ws` subscriber, so the account password must not be
    // in it.
    let message = error.to_string();
    assert!(
        !message.contains("synthpass"),
        "the provider password reached the error: {message}"
    );
    assert!(message.contains("REDACTED"), "{message}");

    let after: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM stream ORDER BY id")
        .fetch_all(&app.state.db)
        .await
        .unwrap();
    assert_eq!(before, after, "a failed refresh changed the catalogue");
}

/// An ingest that changed the lineup must drop the rendered outputs, or they
/// keep serving what it replaced.
#[tokio::test]
async fn an_m3u_refresh_that_changed_the_catalogue_drops_both_rendered_outputs() {
    let app = TestApp::synthetic().await;
    let account = account_reading(&app, 1001, "playlist.m3u").await;
    let handle = super::jobs::test_handle(&app.state, &super::ingest::m3u::job_key(1001));

    let (status, _, _) = app.raw("/output/m3u").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = app.raw("/output/epg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cached_outputs(&app, "m3u-", "m3u").len(), 1);
    assert_eq!(cached_outputs(&app, "epg-", "xml").len(), 1);

    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect("the refresh ran");
    assert!(
        summary.contains("7 new") && summary.contains("1 channels created"),
        "the refresh changed nothing, so the guard below proves nothing: {summary}"
    );

    // Both, because a channel this refresh created is a line in each, and a
    // stream that came or went moves the playlist's `?direct=true` URLs.
    assert!(
        cached_outputs(&app, "m3u-", "m3u").is_empty(),
        "the refresh left the pre-refresh playlist on disk"
    );
    assert!(
        cached_outputs(&app, "epg-", "xml").is_empty(),
        "the refresh left the pre-refresh guide on disk"
    );
}

/// Every write that supersedes a rendered output, with the outputs it
/// supersedes.
///
/// One instance per case, each rendering both outputs first: "the cached file is
/// gone" only means anything if it was there to begin with. The pairing is the
/// point — a rename moves the playlist and the guide together, while a stream
/// URL moves only the playlist's `?direct=true` links, and invalidating more
/// than a write changed is a rescan nobody asked for.
#[tokio::test]
async fn every_write_that_changes_an_output_drops_exactly_that_output() {
    struct Case {
        method: &'static str,
        uri: &'static str,
        body: Option<Value>,
        playlist: bool,
        guide: bool,
    }

    let case = |method, uri, body, playlist, guide| Case {
        method,
        uri,
        body,
        playlist,
        guide,
    };

    let cases = [
        // A channel is a line in the playlist and a `<channel>` in the guide.
        case(
            "POST",
            "/api/channels/channels/",
            Some(json!({ "name": "Synth Added" })),
            true,
            true,
        ),
        case(
            "PATCH",
            "/api/channels/channels/1005/",
            Some(json!({ "channel_number": 50 })),
            true,
            true,
        ),
        case("DELETE", "/api/channels/channels/1005/", None, true, true),
        case(
            "POST",
            "/api/channels/channels/bulk-delete/",
            Some(json!({ "ids": [1005] })),
            true,
            true,
        ),
        // Membership decides which channels a named profile's outputs carry.
        case(
            "PATCH",
            "/api/channels/profiles/1001/channels/1005/",
            Some(json!({ "enabled": true })),
            true,
            true,
        ),
        case(
            "POST",
            "/api/channels/profiles/1001/channels/bulk-update/",
            Some(json!({ "channel_ids": [1005], "enabled": true })),
            true,
            true,
        ),
        // Artwork resolves through the logo row in both.
        case(
            "PATCH",
            "/api/channels/logos/1001/",
            Some(json!({ "url": "https://logos.example/moved.png" })),
            true,
            true,
        ),
        case("DELETE", "/api/channels/logos/1001/", None, true, true),
        case(
            "POST",
            "/api/channels/logos/bulk-delete/",
            Some(json!({ "ids": [1001] })),
            true,
            true,
        ),
        // Streams and groups reach the playlist alone: `?direct=true` URLs and
        // `group-title`. Neither appears in a guide.
        case(
            "PUT",
            "/api/channels/channels/1005/streams/",
            Some(json!({ "ids": [] })),
            true,
            false,
        ),
        case(
            "PATCH",
            "/api/channels/groups/1001/",
            Some(json!({ "name": "Synth Renamed Group" })),
            true,
            false,
        ),
        case("DELETE", "/api/channels/groups/1004/", None, true, false),
        case(
            "PATCH",
            "/api/channels/streams/1001/",
            Some(json!({ "url": "https://provider.example/moved.ts" })),
            true,
            false,
        ),
        case("DELETE", "/api/channels/streams/1002/", None, true, false),
        case(
            "POST",
            "/api/channels/streams/bulk-delete/",
            Some(json!({ "ids": [1002] })),
            true,
            false,
        ),
        // Deleting a provider account cascades its streams away with it.
        case("DELETE", "/api/m3u/accounts/1003/", None, true, false),
        // A guide source reaches the guide alone: its data is what channels map
        // to, and `dummy` is decided at render time.
        case(
            "PATCH",
            "/api/epg/sources/1001/",
            Some(json!({ "refresh_interval": 12 })),
            false,
            true,
        ),
        case("DELETE", "/api/epg/sources/1003/", None, false, true),
    ];

    for case in cases {
        let app = TestApp::synthetic().await;
        let admin = app.login_as(Principal::Admin).await;
        let what = format!("{} {}", case.method, case.uri);

        let (status, _, _) = app.raw("/output/m3u").await;
        assert_eq!(status, StatusCode::OK, "{what}");
        let (status, _, _) = app.raw("/output/epg").await;
        assert_eq!(status, StatusCode::OK, "{what}");
        assert_eq!(cached_outputs(&app, "m3u-", "m3u").len(), 1, "{what}");
        assert_eq!(cached_outputs(&app, "epg-", "xml").len(), 1, "{what}");

        let (status, body) = app
            .request_as(&admin, case.method, case.uri, case.body)
            .await;
        assert!(status.is_success(), "{what}: {status} {body}");

        assert_eq!(
            cached_outputs(&app, "m3u-", "m3u").is_empty(),
            case.playlist,
            "{what} left the playlist cache in the wrong state"
        );
        assert_eq!(
            cached_outputs(&app, "epg-", "xml").is_empty(),
            case.guide,
            "{what} left the guide cache in the wrong state"
        );
    }
}

// ------------------------------------------------------- group number ranges
//
// A group's range is what keeps a lineup stable as a provider grows: a channel
// created for the group — by a refresh or by hand — takes the lowest free
// number inside it, so it lands beside its siblings and moves nothing else.
// The range is the group's own, not a provider link's, so two providers
// feeding one group cannot disagree about it. The one deliberate exception is
// the renumber, which an operator runs once to move an imported click-order
// lineup into ranges before re-adding the tuner in Plex; nothing in a refresh
// calls it.

/// The synthetic seed's admin, as a bearer token.
async fn synthetic_admin(app: &TestApp) -> String {
    match app.login_as(Principal::Admin).await {
        Credential::Bearer(token) => token,
        other => panic!("the admin did not get a bearer token: {other:?}"),
    }
}

async fn effective_numbers(app: &TestApp) -> BTreeMap<i64, Option<f64>> {
    sqlx::query_as::<_, (i64, Option<f64>)>("SELECT id, channel_number FROM effective_channel")
        .fetch_all(&app.state.db)
        .await
        .unwrap()
        .into_iter()
        .collect()
}

async fn group_link(
    app: &TestApp,
    account: i64,
    group: i64,
) -> dollet_core::db::m3u::GroupAccountLink {
    dollet_core::db::m3u::list_group_links(&app.state.db, Some(account))
        .await
        .unwrap()
        .into_iter()
        .find(|link| link.channel_group_id == group)
        .expect("the seed links the group to the account")
}

async fn set_range(app: &TestApp, token: &str, group: i64, body: Value) -> (StatusCode, Value) {
    app.json(
        "PATCH",
        &format!("/api/channels/groups/{group}/"),
        token,
        Some(body),
    )
    .await
}

/// The seed's group 1002 keeps its provider's numbers. A renumber test on it
/// has to take that off first, or the refusal is what gets tested.
async fn number_from_range(app: &TestApp, token: &str, account: i64, group: i64) {
    let (status, saved) = app
        .json(
            "POST",
            &format!("/api/m3u/accounts/{account}/groups/"),
            token,
            Some(json!({ "channel_group": group, "numbering_mode": "range" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["numbering_mode"], "range");
}

async fn set_policy(app: &TestApp, token: &str, value: Value) -> (StatusCode, Value) {
    app.json(
        "PATCH",
        "/api/core/settings/numbering_settings/",
        token,
        Some(json!({ "value": value })),
    )
    .await
}

async fn renumber_by(app: &TestApp, token: &str, group: i64, order: &str) -> (StatusCode, Value) {
    app.json(
        "POST",
        &format!("/api/channels/groups/{group}/renumber/?order={order}"),
        token,
        None,
    )
    .await
}

async fn move_channel(
    app: &TestApp,
    token: &str,
    channel: i64,
    between: Value,
) -> (StatusCode, Value) {
    app.json(
        "POST",
        &format!("/api/channels/channels/{channel}/move/"),
        token,
        Some(between),
    )
    .await
}

async fn renumber(app: &TestApp, token: &str, group: i64) -> (StatusCode, Value) {
    app.json(
        "POST",
        &format!("/api/channels/groups/{group}/renumber/"),
        token,
        None,
    )
    .await
}

#[tokio::test]
async fn a_channel_created_without_a_number_takes_the_first_free_one_in_its_groups_range() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    // Group 1002 owns 200–299, and nothing in the seed sits inside it yet.
    let (status, created) = app
        .json(
            "POST",
            "/api/channels/channels/",
            &token,
            Some(json!({ "name": "Synth Hand Made", "channel_group_id": 1002 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["effective_channel_number"], 200.0);

    let (status, next) = app
        .json(
            "POST",
            "/api/channels/channels/",
            &token,
            Some(json!({ "name": "Synth Hand Made Too", "channel_group_id": 1002 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{next}");
    assert_eq!(next["effective_channel_number"], 201.0);

    // A number given explicitly is still the operator's to give.
    let (status, pinned) = app
        .json(
            "POST",
            "/api/channels/channels/",
            &token,
            Some(json!({
                "name": "Synth Pinned",
                "channel_group_id": 1002,
                "channel_number": 250.5,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{pinned}");
    assert_eq!(pinned["effective_channel_number"], 250.5);
}

/// The rule that produces every number an imported lineup carries: the lowest
/// free integer. What it must never be is "unnumbered" — the HDHR lineup drops
/// those, so a one-click create would make a channel Plex never shows.
#[tokio::test]
async fn a_channel_created_without_a_number_in_a_group_with_no_range_still_reaches_the_lineup() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    // Group 1001 has no range. The seed holds 1 and 2.5, so the lowest free
    // integer is 2.
    let (status, created) = app
        .json(
            "POST",
            "/api/channels/channels/",
            &token,
            Some(json!({ "name": "Synth Hand Made", "channel_group_id": 1001 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["effective_channel_number"], 2.0);

    let (status, lineup) = app.anonymous("GET", "/hdhr/lineup.json").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        lineup.as_array().unwrap().iter().any(|entry| {
            entry["GuideNumber"] == "2" && entry["GuideName"] == "Synth Hand Made"
        }),
        "the new channel is missing from what Plex reads: {lineup}"
    );
}

/// Everything the Groups page shows in one row, in one call.
#[tokio::test]
async fn the_group_list_carries_its_range_its_counts_and_its_standing_with_each_provider() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    let (status, list) = app.json("GET", "/api/channels/groups/", &token, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let by_id = |id: i64| {
        rows(&list)
            .iter()
            .find(|group| group["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("group {id} missing from {list}"))
    };

    let news = by_id(1002);
    assert_eq!(news["number_start"], 200.0);
    assert_eq!(news["number_end"], 299.0);
    // Channel 1001 is in this group by base value only; its override moves it.
    assert_eq!(news["channel_count"], 5);
    let streams: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE channel_group_id = 1002")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(news["stream_count"], streams);
    // Its link keeps the provider's numbers, which the seed stores the way an
    // import does: `channel_numbering_mode` inside `custom_properties`.
    assert_eq!(
        news["links"],
        json!([{
            "m3u_account_id": 1001,
            "enabled": true,
            "auto_channel_sync": true,
            "numbering_mode": "provider",
        }])
    );

    // Two providers feed Sports; the row says so once per provider.
    let sports = by_id(1001);
    assert_eq!(sports["number_start"], Value::Null);
    assert_eq!(sports["links"].as_array().unwrap().len(), 2);

    // A provider that stopped importing the group still appears, as off.
    assert_eq!(
        by_id(1005)["links"],
        json!([{
            "m3u_account_id": 1001,
            "enabled": false,
            "auto_channel_sync": false,
            "numbering_mode": "range",
        }])
    );
}

#[tokio::test]
async fn a_groups_range_is_validated_and_can_be_cleared() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    let (status, saved) = set_range(
        &app,
        &token,
        1001,
        json!({ "number_start": 100, "number_end": 199 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["number_start"], 100.0);
    assert_eq!(saved["number_end"], 199.0);

    // Checked here, not stored and discovered on the first refresh when every
    // allocation fails.
    let (status, refused) = set_range(&app, &token, 1001, json!({ "number_end": 50 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(
        refused["fields"]["number_end"],
        "must be at or above the start"
    );
    let (status, refused) = set_range(&app, &token, 1001, json!({ "number_start": -1 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["fields"]["number_start"], "must be at or above 0");

    // An absent key leaves that half alone.
    let (status, saved) = set_range(&app, &token, 1001, json!({ "number_end": 150 })).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["number_start"], 100.0);
    assert_eq!(saved["number_end"], 150.0);

    // An explicit null takes the range off.
    let (status, saved) = set_range(
        &app,
        &token,
        1001,
        json!({ "number_start": null, "number_end": null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["number_start"], Value::Null);
    assert_eq!(saved["number_end"], Value::Null);

    // The same body renames, and an empty name is refused rather than stored.
    let (status, _) = set_range(&app, &token, 1001, json!({ "name": "  " })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, saved) = set_range(&app, &token, 1001, json!({ "name": "Synth Sport" })).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["name"], "Synth Sport");

    // A new group can be born with its range.
    let (status, created) = app
        .json(
            "POST",
            "/api/channels/groups/",
            &token,
            Some(json!({ "name": "Synth Made", "number_start": 700 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["number_start"], 700.0);
    assert_eq!(created["number_end"], Value::Null);
    assert_eq!(created["channel_count"], 0);
    assert_eq!(created["links"], json!([]));
}

#[tokio::test]
async fn renumbering_a_group_walks_its_range_in_lineup_order_and_moves_nothing_else() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    number_from_range(&app, &token, 1001, 1002).await;

    // Group 1002's channels as the lineup orders them: 3, 4, 11, 12, then the
    // unnumbered one last. Channel 1001 is in the group only by base value —
    // its override moves it to 1001 — so it is not a member here.
    let (status, result) = renumber(&app, &token, 1002).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["renumbered"], 5);
    let assigned: Vec<(i64, f64)> = result["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_i64().unwrap(),
                c["channel_number"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        assigned,
        vec![
            (1003, 200.0),
            (1004, 201.0),
            (1011, 202.0),
            (1012, 203.0),
            (1002, 204.0)
        ]
    );

    // Served, not merely stored — and nothing outside the group moved.
    let numbers = effective_numbers(&app).await;
    assert_eq!(numbers[&1002], Some(204.0));
    assert_eq!(numbers[&1000], Some(1.0));
    assert_eq!(
        numbers[&1001],
        Some(2.5),
        "a channel outside the group moved"
    );

    // A range with no room refuses before touching anything.
    let (status, _) = set_range(
        &app,
        &token,
        1002,
        json!({ "number_start": 300, "number_end": 302 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, refused) = renumber(&app, &token, 1002).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(effective_numbers(&app).await[&1003], Some(200.0));

    // Numbers other groups hold are stepped over, never taken: 1 belongs to
    // channel 1000, so the walk from 1 starts at 2.
    let (status, _) = set_range(
        &app,
        &token,
        1002,
        json!({ "number_start": 1, "number_end": null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, result) = renumber(&app, &token, 1002).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let numbers = effective_numbers(&app).await;
    assert_eq!(numbers[&1003], Some(2.0), "took channel 1000's number");
    let mut held: Vec<f64> = numbers.values().flatten().copied().collect();
    let count = held.len();
    held.sort_by(f64::total_cmp);
    held.dedup();
    assert_eq!(
        held.len(),
        count,
        "two channels share a number: {numbers:?}"
    );

    // No range, nothing to renumber into; and a group that does not exist is
    // not silently an empty renumber.
    let (status, refused) = renumber(&app, &token, 1001).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let (status, _) = renumber(&app, &token, 4242).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The order a renumber walks is a choice, and the choice reaches the planner.
///
/// Group 1002's numbers say 1003, 1004, 1011, 1012, then the unnumbered one.
/// Two of those channels are mapped to a guide entry and three are not, so
/// `order=guide` is a different sequence from the lineup's own — which is the
/// only kind of assertion that can tell the two apart.
#[tokio::test]
async fn a_renumber_can_walk_a_group_by_its_guide_names() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;
    number_from_range(&app, &token, 1001, 1002).await;

    let (status, result) = renumber_by(&app, &token, 1002, "guide").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let assigned: Vec<(i64, f64)> = result["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_i64().unwrap(),
                c["channel_number"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        assigned,
        vec![
            // "Synth Dummy Guide" then "Synth Gap Guide"...
            (1011, 200.0),
            (1012, 201.0),
            // ...and the channels with no guide keep their order, last.
            (1003, 202.0),
            (1004, 203.0),
            (1002, 204.0),
        ]
    );

    // An order nobody offers is refused rather than quietly meaning "current".
    let (status, _) = renumber_by(&app, &token, 1002, "sideways").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Dropping a channel between two others writes one number.
#[tokio::test]
async fn a_moved_channel_takes_a_number_between_its_new_neighbours() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    // 1012 is at 12; 1003 and 1004 are at 3 and 4 in the same group.
    let (status, moved) =
        move_channel(&app, &token, 1012, json!({ "after": 1003, "before": 1004 })).await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["channel_number"], 3.5);

    let numbers = effective_numbers(&app).await;
    assert_eq!(
        numbers[&1012],
        Some(3.5),
        "the move is served, not just stored"
    );
    assert_eq!(numbers[&1003], Some(3.0), "a neighbour moved");
    assert_eq!(numbers[&1004], Some(4.0), "a neighbour moved");

    // A pair with nothing left between them is told so, and nothing is
    // written: the way out is renumbering the group, not shifting the rest.
    // Two decimals is as fine as a number gets, by hand or by drop.
    let (status, edited) = app
        .json(
            "PATCH",
            "/api/channels/channels/1004/",
            &token,
            Some(json!({ "channel_number": 3.01 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edited}");

    let (status, refused) =
        move_channel(&app, &token, 1011, json!({ "after": 1003, "before": 1004 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("no room between 3 and 3.01"),
        "{refused}"
    );
    assert_eq!(effective_numbers(&app).await[&1011], Some(11.0));

    // And a neighbour from another group is refused outright: a drop must not
    // move a channel out of the group whose range it is numbered from.
    let (status, refused) =
        move_channel(&app, &token, 1011, json!({ "after": 1005, "before": null })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(effective_numbers(&app).await[&1011], Some(11.0));
}

/// Channel 1001 sits in group 1001 through its override and is numbered 2.5 by
/// the same override. The renumber has to reach it — the view serves the
/// override, so a base number alone would change nothing Plex sees — and must
/// not take the channel's other customisations with the number.
#[tokio::test]
async fn renumbering_clears_the_number_override_and_keeps_the_rest_of_it() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    let (status, _) = set_range(&app, &token, 1001, json!({ "number_start": 500 })).await;
    assert_eq!(status, StatusCode::OK);
    let (status, result) = renumber(&app, &token, 1001).await;
    assert_eq!(status, StatusCode::OK, "{result}");

    let (status, detail) = app
        .json("GET", "/api/channels/channels/1001/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        detail["effective_channel_number"].as_f64().unwrap() >= 500.0,
        "{detail}"
    );
    assert_eq!(
        detail["override"]["channel_number"],
        Value::Null,
        "{detail}"
    );
    assert_eq!(detail["override"]["name"], "Synth Two & A Half", "{detail}");
    assert_eq!(detail["effective_name"], "Synth Two & A Half");
}

#[tokio::test]
async fn switching_auto_sync_on_previews_what_the_next_refresh_would_create() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    let (status, _) = set_range(
        &app,
        &token,
        1001,
        json!({ "number_start": 100, "number_end": 199 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Stream 1020 is in group 1001 on account 1001 and on no channel: exactly
    // what auto-sync would turn into a channel, and exactly the kind of stream
    // an operator may have left out on purpose.
    let enable = json!({ "channel_group": 1001, "auto_channel_sync": true });
    let (status, refused) = app
        .json(
            "POST",
            "/api/m3u/accounts/1001/groups/",
            &token,
            Some(enable.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["cost"]["channels_created"], 1);
    assert_eq!(refused["cost"]["channels"][0]["stream"], 1020);
    assert_eq!(refused["cost"]["channels"][0]["name"], "Synth Loose Feed");
    assert_eq!(refused["cost"]["channels"][0]["channel_number"], 100.0);

    // The refusal wrote nothing.
    assert!(!group_link(&app, 1001, 1001).await.auto_channel_sync);

    let mut confirmed = enable.clone();
    confirmed["confirm"] = json!(true);
    let (status, saved) = app
        .json(
            "POST",
            "/api/m3u/accounts/1001/groups/",
            &token,
            Some(confirmed),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["auto_channel_sync"], true);
    assert!(group_link(&app, 1001, 1001).await.auto_channel_sync);

    // A group with nothing to create switches on without ceremony.
    let (status, saved) = app
        .json(
            "POST",
            "/api/m3u/accounts/1001/groups/",
            &token,
            Some(json!({ "channel_group": 1003, "auto_channel_sync": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["auto_channel_sync"], true);
}

/// The guide job's own pass runs on the guide's schedule, which can be a day
/// away; a channel a playlist refresh created would show in Plex with an
/// empty strip until then. When the operator opted into matching on refresh,
/// the playlist refresh runs the pass over exactly the channels it made.
#[tokio::test]
async fn a_refresh_matches_the_channels_it_created_to_the_guide_when_asked() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    let (status, _) = app
        .json(
            "PATCH",
            "/api/core/settings/epg_settings/",
            &token,
            Some(json!({ "value": { "epg_auto_match_on_refresh": true } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Group 1002 auto-syncs. Its rename strips `Synth `, which would take the
    // new channel's name away from the guide's; what is under test is the pass
    // running at all, so the rule comes off.
    let (status, _) = app
        .json(
            "POST",
            "/api/m3u/accounts/1001/groups/",
            &token,
            Some(json!({
                "channel_group": 1002,
                "custom_properties": {
                    "channel_numbering_mode": "provider",
                    "channel_numbering_fallback": 200,
                },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // A feed carrying one new stream in the auto-synced group, named as guide
    // channel 1002 is.
    let body = playlist(&[("Synth News Guide", "Synth News", &feed_url(2020))]);
    let path = app.state.config.cache_dir().join("news.m3u");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();
    let mut account = dollet_core::db::m3u::get_account(&app.state.db, 1001)
        .await
        .unwrap()
        .unwrap();
    account.file_path = Some(path.to_string_lossy().into_owned());
    let account = dollet_core::db::m3u::save_account(&app.state.db, &account)
        .await
        .unwrap();
    let handle = super::jobs::test_handle(&app.state, &super::ingest::m3u::job_key(1001));

    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect("the refresh ran");
    assert!(
        summary.contains("1 channels created") && summary.contains("1 matched to a guide"),
        "{summary}"
    );

    let (id, epg): (i64, Option<i64>) =
        sqlx::query_as("SELECT id, epg_data_id FROM channel WHERE name = 'Synth News Guide'")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(
        epg,
        Some(1002),
        "channel {id} was created without its guide"
    );

    // A channel the operator left unmapped is not the playlist refresh's to
    // decide, whatever its name would score.
    let still: Option<i64> = sqlx::query_scalar("SELECT epg_data_id FROM channel WHERE id = 1002")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(still, None);
}

/// A source keeps the range on the provider link, with a default of start 1
/// and no end on every link ever created. The import carries a range someone
/// set and leaves that default behind, or every group would arrive claiming a
/// range nobody chose.
#[tokio::test]
async fn the_import_carries_a_range_an_operator_set_and_leaves_the_sources_default_behind() {
    let app = TestApp::synthetic().await;
    let db = &app.state.db;

    sqlx::query("UPDATE channel_group SET number_start = NULL, number_end = NULL")
        .execute(db)
        .await
        .unwrap();
    // As the source's rows arrive: a configured range on an auto-synced link,
    // the untouched default on a plain link, the default on a link whose
    // auto-sync is on (that one *is* a choice), and a range on a disabled link.
    for (id, start, end, sync) in [
        (1002, Some(200.0), Some(299.0), 1),
        (1001, Some(1.0), None, 0),
        (1003, Some(1.0), None, 1),
        (1005, Some(50.0), None, 0),
    ] {
        sqlx::query(
            "UPDATE channel_group_m3u_account SET auto_sync_channel_start = ?,
                 auto_sync_channel_end = ?, auto_channel_sync = ? WHERE id = ?",
        )
        .bind(start)
        .bind(end)
        .bind(sync)
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    }

    super::importer::carry_group_ranges(db).await.unwrap();

    let range = |id: i64| async move {
        dollet_core::db::channels::get_group(db, id)
            .await
            .unwrap()
            .map(|group| (group.number_start, group.number_end))
            .unwrap()
    };
    assert_eq!(range(1002).await, (Some(200.0), Some(299.0)));
    assert_eq!(
        range(1001).await,
        (None, None),
        "the source's default became a range"
    );
    assert_eq!(range(1003).await, (Some(1.0), None));
    assert_eq!(
        range(1005).await,
        (None, None),
        "a disabled link's range was carried"
    );
}

/// Filling the lowest free number would hand a gap the operator left on
/// purpose — or a deleted channel's slot — to whatever arrives next. Here a
/// new channel goes after the range's highest, and the gap is the operator's.
#[tokio::test]
async fn a_new_channel_appends_after_the_ranges_highest_and_leaves_a_gap_alone() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    let create = |name: &'static str, number: Option<f64>| {
        let token = token.clone();
        let app = &app;
        async move {
            let mut body = json!({ "name": name, "channel_group_id": 1002 });
            if let Some(number) = number {
                body["channel_number"] = json!(number);
            }
            let (status, created) = app
                .json("POST", "/api/channels/channels/", &token, Some(body))
                .await;
            assert_eq!(status, StatusCode::CREATED, "{created}");
            created["effective_channel_number"].as_f64().unwrap()
        }
    };

    assert_eq!(create("Synth First", None).await, 200.0);
    // Placed by hand, leaving 201–204 open for what belongs there.
    assert_eq!(create("Synth Placed", Some(205.0)).await, 205.0);
    assert_eq!(
        create("Synth Next", None).await,
        206.0,
        "a gap left on purpose was filled"
    );
}

#[tokio::test]
async fn the_channel_step_puts_new_numbers_on_its_grid() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;
    let (status, saved) = set_policy(&app, &token, json!({ "channel_step": 10 })).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["value"]["channel_step"], 10.0);

    let create = |name: &'static str, number: Option<f64>| {
        let token = token.clone();
        let app = &app;
        async move {
            let mut body = json!({ "name": name, "channel_group_id": 1002 });
            if let Some(number) = number {
                body["channel_number"] = json!(number);
            }
            let (status, created) = app
                .json("POST", "/api/channels/channels/", &token, Some(body))
                .await;
            assert_eq!(status, StatusCode::CREATED, "{created}");
            created["effective_channel_number"].as_f64().unwrap()
        }
    };

    assert_eq!(create("Synth A", None).await, 200.0);
    assert_eq!(create("Synth B", None).await, 210.0);
    // A hand-placed 215 is a number like any other; the grid continues past it.
    assert_eq!(create("Synth C", Some(215.0)).await, 215.0);
    assert_eq!(create("Synth D", None).await, 220.0);
}

#[tokio::test]
async fn renumbering_lays_the_group_out_on_the_step() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;
    number_from_range(&app, &token, 1001, 1002).await;
    let (status, _) = set_policy(&app, &token, json!({ "channel_step": 10 })).await;
    assert_eq!(status, StatusCode::OK);

    let (status, result) = renumber(&app, &token, 1002).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let numbers: Vec<f64> = result["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["channel_number"].as_f64().unwrap())
        .collect();
    assert_eq!(numbers, vec![200.0, 210.0, 220.0, 230.0, 240.0]);

    // And the block's capacity is the step's: 200–229 holds three at ten.
    let (status, _) = set_range(
        &app,
        &token,
        1002,
        json!({ "number_start": 200, "number_end": 229 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, refused) = renumber(&app, &token, 1002).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(
        effective_numbers(&app).await[&1003],
        Some(200.0),
        "a refused renumber wrote"
    );
}

#[tokio::test]
async fn numbering_settings_are_checked_before_they_are_stored() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    let (status, refused) = set_policy(&app, &token, json!({ "channel_step": 0 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(refused["fields"]["channel_step"].is_string(), "{refused}");

    let (status, refused) = set_policy(
        &app,
        &token,
        json!({ "channel_step": 10, "group_block_size": 5 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["fields"]["group_block_size"].is_string(),
        "{refused}"
    );

    // Nothing stuck from the refusals.
    let (status, row) = app
        .json(
            "GET",
            "/api/core/settings/numbering_settings/",
            &token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(row["value"]["channel_step"], 1.0);
    assert_eq!(row["value"]["group_block_size"], 100.0);

    let (status, saved) = set_policy(
        &app,
        &token,
        json!({ "channel_step": 10, "group_block_size": 1000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["value"]["group_block_size"], 1000.0);
}

#[tokio::test]
async fn assigning_ranges_previews_blocks_in_the_order_given_and_applies_only_what_was_shown() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;
    let (status, _) = set_policy(&app, &token, json!({ "group_block_size": 1000 })).await;
    assert_eq!(status, StatusCode::OK);

    // 1002 already has 200–299, so the first free boundary is 1000; 1002 is
    // skipped wherever it appears in the order, and an unknown id is ignored.
    let (status, plan) = app
        .json(
            "POST",
            "/api/channels/groups/plan-ranges/",
            &token,
            Some(json!({ "order": [1003, 1002, 1001, 4242, 1003] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert_eq!(plan["block_size"], 1000.0);
    assert_eq!(
        plan["ranges"],
        json!([
            { "id": 1003, "name": "Synth Adults", "number_start": 1000.0, "number_end": 1999.0 },
            { "id": 1001, "name": "Synth Sports", "number_start": 2000.0, "number_end": 2999.0 },
        ])
    );

    // A plan writes nothing until it is applied.
    let (_, groups) = app.json("GET", "/api/channels/groups/", &token, None).await;
    assert!(
        rows(&groups)
            .iter()
            .filter(|group| group["id"] != 1002)
            .all(|group| group["number_start"].is_null()),
        "{groups}"
    );

    let (status, applied) = app
        .json(
            "POST",
            "/api/channels/groups/assign-ranges/",
            &token,
            Some(json!({ "ranges": plan["ranges"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_eq!(applied["assigned"], 2);
    let (_, groups) = app.json("GET", "/api/channels/groups/", &token, None).await;
    let sports = rows(&groups)
        .iter()
        .find(|g| g["id"] == 1001)
        .unwrap()
        .clone();
    assert_eq!(sports["number_start"], 2000.0);
    assert_eq!(sports["number_end"], 2999.0);

    // Applying never moves a block that exists, so a stale plan is refused.
    let (status, refused) = app
        .json(
            "POST",
            "/api/channels/groups/assign-ranges/",
            &token,
            Some(json!({ "ranges": [{ "id": 1001, "number_start": 5000, "number_end": 5999 }] })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");

    // The next plan starts above the blocks just assigned.
    let (_, plan) = app
        .json(
            "POST",
            "/api/channels/groups/plan-ranges/",
            &token,
            Some(json!({ "order": [1004] })),
        )
        .await;
    assert_eq!(plan["ranges"][0]["number_start"], 3000.0);
}

#[tokio::test]
async fn renumber_all_lays_out_every_ranged_group_and_says_which_it_left_alone() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;
    let (status, _) = set_range(
        &app,
        &token,
        1001,
        json!({ "number_start": 100, "number_end": 199 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, plan) = app
        .json("POST", "/api/channels/groups/plan-renumber/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{plan}");

    // 1001 is the one group that is ranged, populated and numbered from its
    // range. 1002 keeps its provider's numbers; the rest have no range or
    // nothing in them.
    let groups = plan["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{plan}");
    assert_eq!(groups[0]["id"], 1001);
    let channels = groups[0]["channels"].as_array().unwrap();
    assert_eq!(channels.len(), 10);
    assert_eq!(channels[0]["to"], 100.0);
    assert_eq!(channels[9]["to"], 109.0);
    assert!(
        channels
            .iter()
            .all(|c| c["name"].is_string() && !c["from"].is_null())
    );

    let skipped: Vec<(i64, String)> = plan["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["id"].as_i64().unwrap(),
                g["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        skipped
            .iter()
            .any(|(id, reason)| *id == 1002 && reason.starts_with("numbered by ")),
        "{skipped:?}"
    );
    assert!(
        skipped
            .iter()
            .any(|(id, reason)| *id == 1003 && reason == "no range"),
        "{skipped:?}"
    );

    // Still only a plan.
    assert_eq!(effective_numbers(&app).await[&1000], Some(1.0));

    let (status, done) = app
        .json("POST", "/api/channels/groups/renumber-all/", &token, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["renumbered"], 10);
    assert_eq!(done["groups"], 1);
    let numbers = effective_numbers(&app).await;
    assert_eq!(numbers[&1000], Some(100.0));
    // The provider-numbered group did not move.
    assert_eq!(numbers[&1003], Some(3.0));
}

/// An OTA tuner's `5.1` is that channel's identity, not a slot in a range: a
/// group fed by a provider whose numbers are kept is not renumbered, and the
/// link says which mode it is in so the page can say why.
#[tokio::test]
async fn a_group_numbered_by_its_provider_refuses_a_renumber() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;

    let (status, refused) = renumber(&app, &token, 1002).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["detail"].as_str().unwrap().contains("numbered by"),
        "{refused}"
    );
    assert_eq!(effective_numbers(&app).await[&1003], Some(3.0));

    // The mode is the link's to change, both ways, and only to one of the two.
    let (status, saved) = app
        .json(
            "POST",
            "/api/m3u/accounts/1001/groups/",
            &token,
            Some(json!({ "channel_group": 1001, "numbering_mode": "provider" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["numbering_mode"], "provider");
    let (status, refused) = app
        .json(
            "POST",
            "/api/m3u/accounts/1001/groups/",
            &token,
            Some(json!({ "channel_group": 1001, "numbering_mode": "sideways" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(refused["fields"]["numbering_mode"].is_string(), "{refused}");

    let (_, groups) = app.json("GET", "/api/channels/groups/", &token, None).await;
    let sports = rows(&groups)
        .iter()
        .find(|g| g["id"] == 1001)
        .unwrap()
        .clone();
    assert!(
        sports["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link["m3u_account_id"] == 1001 && link["numbering_mode"] == "provider"),
        "{sports}"
    );
}

/// The one outcome of a refresh that shows up nowhere: the stream is in the
/// catalogue, the refresh succeeded, and Plex never sees the channel. It is a
/// notification until a refresh finds the range has room, and gone after.
#[tokio::test]
async fn a_full_range_is_a_notification_until_a_refresh_finds_room_again() {
    let app = TestApp::synthetic().await;
    let token = synthetic_admin(&app).await;
    number_from_range(&app, &token, 1001, 1002).await;

    // A range with exactly one slot, already taken: the playlist's new stream
    // in this group has nowhere to go.
    let (status, _) = set_range(
        &app,
        &token,
        1002,
        json!({ "number_start": 200, "number_end": 200 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app
        .json(
            "POST",
            "/api/channels/channels/",
            &token,
            Some(json!({ "name": "Synth Squatter", "channel_group_id": 1002, "channel_number": 200 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let account = account_reading(&app, 1001, "playlist.m3u").await;
    let handle = super::jobs::test_handle(&app.state, &super::ingest::m3u::job_key(1001));
    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect("the first refresh ran");
    assert!(!summary.contains("channels created"), "{summary}");

    let raised: Vec<dollet_core::db::notifications::Notification> =
        dollet_core::db::notifications::list(&app.state.db)
            .await
            .unwrap()
            .into_iter()
            .filter(|n| n.kind == "auto_sync.range_full")
            .collect();
    assert_eq!(raised.len(), 1, "{raised:?}");
    assert_eq!(raised[0].subject, "group:1002");
    assert!(
        raised[0].message.contains("Synth News"),
        "{}",
        raised[0].message
    );
    assert!(
        raised[0].message.contains("200–200"),
        "{}",
        raised[0].message
    );
    assert_eq!(raised[0].detail["unnumbered"], 1);

    // Room again: the stream becomes a channel, and the bell is quiet.
    let (status, _) = set_range(&app, &token, 1002, json!({ "number_end": 299 })).await;
    assert_eq!(status, StatusCode::OK);
    let summary = super::ingest::m3u::refresh(&app.state, &account, &handle)
        .await
        .expect("the second refresh ran");
    assert!(summary.contains("1 channels created"), "{summary}");
    let still: Vec<_> = dollet_core::db::notifications::list(&app.state.db)
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.kind == "auto_sync.range_full")
        .collect();
    assert!(
        still.is_empty(),
        "the condition was fixed and the notification stayed"
    );
}

mod backups;
