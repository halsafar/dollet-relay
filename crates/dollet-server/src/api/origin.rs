//! The absolute origin a client reached us on.
//!
//! Every HDHR lineup URL, every M3U stream URL and the Xtream `server_info`
//! block is absolute, so getting this wrong fails as "discovery works, playback
//! doesn't" — Plex finds the tuner, reads a lineup full of URLs pointing
//! somewhere unreachable, and reports nothing more useful than a timeout.
//!
//! Forwarding headers are believed only from a trusted peer, for the same
//! reason they are ignored in `auth`: any client can send them, and here a
//! forged one would rewrite the URLs handed to every other client.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;

use super::error::ApiError;
use crate::AppState;

/// Default ports, so `:80` and `:443` stay out of the URLs the way every
/// client expects.
const HTTP: u16 = 80;
const HTTPS: u16 = 443;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
}

impl Origin {
    /// `http://host:9191`, with no trailing slash.
    pub fn base_url(&self) -> String {
        match self.port {
            Some(port) if !self.is_default_port(port) => {
                format!("{}://{}:{}", self.scheme, self.host, port)
            }
            _ => format!("{}://{}", self.scheme, self.host),
        }
    }

    /// The port Xtream's `server_info` should report.
    ///
    /// Read off the `Host` header, a request with no explicit port would report
    /// `"80"` while every URL in the same response carries `:9191` — a client
    /// that believes `server_info` then builds unreachable URLs.
    /// Ours reports the port the URLs actually use.
    pub fn port_str(&self) -> String {
        self.port
            .unwrap_or(if self.scheme == "https" { HTTPS } else { HTTP })
            .to_string()
    }

    fn is_default_port(&self, port: u16) -> bool {
        (self.scheme == "http" && port == HTTP) || (self.scheme == "https" && port == HTTPS)
    }

    fn parse_authority(scheme: &str, authority: &str) -> Option<Self> {
        let authority = authority.trim();
        if authority.is_empty() {
            return None;
        }

        // IPv6 literals are bracketed, so the last colon is only a port
        // separator when it follows the closing bracket.
        let split_at = match authority.rfind(']') {
            Some(bracket) => authority[bracket..].rfind(':').map(|i| bracket + i),
            None => authority.rfind(':'),
        };

        let (host, port) = match split_at {
            Some(index) => {
                let port = authority[index + 1..].parse().ok()?;
                (&authority[..index], Some(port))
            }
            None => (authority, None),
        };

        (!host.is_empty()).then(|| Self {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        })
    }
}

/// Distinct origins this process has served a client-facing output on.
///
/// The operator cannot be told which address to give Plex — the server does not
/// know how a client reaches it — so the next best thing is showing which
/// addresses clients have already used. Recorded after the resolution above, so
/// it reports what the lineup actually advertised rather than what the request
/// line said.
///
/// Bounded, because the base URL contains the `Host` header and an
/// unauthenticated client can vary it per request: without a cap this map is a
/// memory leak anyone who can reach the outputs is able to drive. Thirty-two is
/// far above the handful a real deployment produces — a LAN address, a
/// hostname, whatever a reverse proxy forwards — and the least recently seen
/// entry is what gets dropped, so the addresses in current use are the ones
/// that survive.
const MAX_SEEN_ORIGINS: usize = 32;

static SEEN: LazyLock<DashMap<String, Seen>> = LazyLock::new(DashMap::new);

#[derive(Debug, Clone)]
pub struct Seen {
    /// `hdhr`, `m3u`, `epg`. A set rather than a last-one-wins string: one
    /// address is usually asked for all three, and which ones tell an operator
    /// whether the client on that address is Plex or a player.
    kinds: BTreeSet<&'static str>,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    requests: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeenOrigin {
    pub base_url: String,
    pub kinds: Vec<&'static str>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub requests: u64,
}

/// Note that a client asked for `kind` on this origin.
pub fn record(kind: &'static str, origin: &Origin) {
    record_in(&SEEN, kind, origin.base_url(), Utc::now());
}

/// The map is taken as a parameter so the eviction rule can be exercised
/// without the process-global one, which every other test in this binary is
/// writing to at the same time.
fn record_in(
    map: &DashMap<String, Seen>,
    kind: &'static str,
    base_url: String,
    now: DateTime<Utc>,
) {
    if let Some(mut existing) = map.get_mut(&base_url) {
        existing.kinds.insert(kind);
        existing.last_seen = now;
        existing.requests += 1;
        return;
    }

    while map.len() >= MAX_SEEN_ORIGINS {
        let oldest = map
            .iter()
            .min_by_key(|entry| entry.last_seen)
            .map(|entry| entry.key().clone());
        match oldest {
            Some(key) => map.remove(&key),
            None => break,
        };
    }

    map.insert(
        base_url,
        Seen {
            kinds: BTreeSet::from([kind]),
            first_seen: now,
            last_seen: now,
            requests: 1,
        },
    );
}

/// Most recently seen first, which is the order an operator reads it in.
pub fn seen() -> Vec<SeenOrigin> {
    let mut origins: Vec<SeenOrigin> = SEEN
        .iter()
        .map(|entry| SeenOrigin {
            base_url: entry.key().clone(),
            kinds: entry.kinds.iter().copied().collect(),
            first_seen: entry.first_seen,
            last_seen: entry.last_seen,
            requests: entry.requests,
        })
        .collect();
    origins.sort_by_key(|origin| std::cmp::Reverse(origin.last_seen));
    origins
}

/// Resolve the origin from configuration, then forwarding headers, then `Host`.
pub fn resolve(state: &AppState, parts: &Parts, client_is_trusted: bool) -> Origin {
    // An explicit override wins outright: it exists precisely for deployments
    // whose proxy sets none of these headers correctly.
    if let Some(configured) = state
        .config
        .advertised_base_url
        .as_deref()
        .and_then(from_configured)
    {
        return configured;
    }

    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };

    let scheme = if client_is_trusted {
        header("x-forwarded-proto")
            .and_then(|value| value.split(',').next())
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "http".to_owned())
    } else {
        "http".to_owned()
    };

    let authority = client_is_trusted
        .then(|| header("x-forwarded-host").and_then(|value| value.split(',').next()))
        .flatten()
        .or_else(|| header("host"));

    authority
        .and_then(|authority| Origin::parse_authority(&scheme, authority.trim()))
        .unwrap_or_else(|| Origin {
            scheme,
            host: state.config.listen.ip().to_string(),
            port: Some(state.config.listen.port()),
        })
}

/// An operator-configured base, as an `Origin`.
///
/// Also how `outputs::artwork_base` reads `DOLLET_ARTWORK_BASE_URL`, so the two
/// overrides normalise a trailing slash and a default port identically.
pub(super) fn from_configured(raw: &str) -> Option<Origin> {
    // The same parse startup validation ran, so a value that booted cannot fail
    // to resolve here.
    let parsed = dollet_core::config::parse_advertised_base_url(raw)?;
    Some(Origin {
        scheme: parsed.scheme().to_owned(),
        host: parsed.host_str()?.to_owned(),
        port: parsed.port(),
    })
}

impl FromRequestParts<AppState> for Origin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let trusted = super::auth::peer_is_trusted(parts, &state.config.trusted_proxies);
        Ok(resolve(state, parts, trusted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ports_are_omitted_and_others_kept() {
        let plain = Origin {
            scheme: "http".into(),
            host: "ipx.test".into(),
            port: Some(80),
        };
        assert_eq!(plain.base_url(), "http://ipx.test");
        assert_eq!(plain.port_str(), "80");

        let explicit = Origin {
            scheme: "http".into(),
            host: "ipx.test".into(),
            port: Some(9191),
        };
        assert_eq!(explicit.base_url(), "http://ipx.test:9191");
        assert_eq!(explicit.port_str(), "9191");

        let tls = Origin {
            scheme: "https".into(),
            host: "ipx.test".into(),
            port: Some(443),
        };
        assert_eq!(tls.base_url(), "https://ipx.test");
        assert_eq!(tls.port_str(), "443");
    }

    #[test]
    fn a_portless_authority_reports_the_schemes_default() {
        let origin = Origin::parse_authority("https", "ipx.test").unwrap();
        assert_eq!(origin.port, None);
        assert_eq!(origin.base_url(), "https://ipx.test");
        assert_eq!(origin.port_str(), "443");
    }

    #[test]
    fn ipv6_literals_keep_their_brackets_and_lose_only_the_port() {
        let origin = Origin::parse_authority("http", "[2001:db8::1]:9191").unwrap();
        assert_eq!(origin.host, "[2001:db8::1]");
        assert_eq!(origin.port, Some(9191));
        assert_eq!(origin.base_url(), "http://[2001:db8::1]:9191");

        let bare = Origin::parse_authority("http", "[2001:db8::1]").unwrap();
        assert_eq!(bare.host, "[2001:db8::1]");
        assert_eq!(bare.port, None);
    }

    #[test]
    fn nonsense_authorities_are_rejected_rather_than_half_parsed() {
        assert!(Origin::parse_authority("http", "").is_none());
        assert!(Origin::parse_authority("http", ":9191").is_none());
        assert!(Origin::parse_authority("http", "host:notaport").is_none());
    }

    #[test]
    fn the_seen_map_evicts_the_least_recently_seen_rather_than_growing() {
        // The `Host` header is client-supplied, so this is the path an
        // unauthenticated caller drives by varying one header.
        let map: DashMap<String, Seen> = DashMap::new();
        let start = Utc::now();

        for index in 0..MAX_SEEN_ORIGINS + 8 {
            record_in(
                &map,
                "hdhr",
                format!("http://host-{index:03}.test"),
                start + chrono::Duration::seconds(index as i64),
            );
        }

        assert_eq!(map.len(), MAX_SEEN_ORIGINS);
        assert!(!map.contains_key("http://host-000.test"), "nothing evicted");
        assert!(
            map.contains_key(&format!("http://host-{:03}.test", MAX_SEEN_ORIGINS + 7)),
            "the newest arrival was the one dropped"
        );
    }

    #[test]
    fn a_second_request_on_one_origin_updates_it_rather_than_adding_another() {
        let map: DashMap<String, Seen> = DashMap::new();
        let start = Utc::now();
        let later = start + chrono::Duration::seconds(30);

        record_in(&map, "hdhr", "http://ipx.test:9191".into(), start);
        record_in(&map, "m3u", "http://ipx.test:9191".into(), later);

        assert_eq!(map.len(), 1);
        let entry = map.get("http://ipx.test:9191").unwrap();
        assert_eq!(entry.requests, 2);
        // `first_seen` is the one field a repeat request must not move: it is
        // how long this address has been in use.
        assert_eq!(entry.first_seen, start);
        assert_eq!(entry.last_seen, later);
        assert_eq!(
            entry.kinds.iter().copied().collect::<Vec<_>>(),
            vec!["hdhr", "m3u"]
        );
    }

    #[test]
    fn a_configured_base_url_parses_with_and_without_a_port() {
        let with_port = from_configured("https://tv.example:8443/").unwrap();
        assert_eq!(with_port.base_url(), "https://tv.example:8443");

        let without = from_configured("https://tv.example").unwrap();
        assert_eq!(without.base_url(), "https://tv.example");

        assert!(from_configured("not a url").is_none());
    }
}
