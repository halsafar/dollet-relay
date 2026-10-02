//! Process configuration: things fixed at startup.
//!
//! Anything a user can change at runtime belongs in `settings` (database
//! backed), not here.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::settings::Cidr;

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    /// Overrides the scheme/host used to build absolute URLs in HDHR lineups
    /// and M3U output. Needed when behind a reverse proxy that does not set
    /// `X-Forwarded-*`; getting it wrong fails as "discovery works, playback
    /// doesn't".
    pub advertised_base_url: Option<String>,
    /// Overrides the base of artwork URLs alone — `tvg-logo`, the guide's
    /// `<icon>`, Xtream's `stream_icon`.
    ///
    /// Those are the one class of URL a *browser* resolves rather than the
    /// server that fetched the document: Plex hands the guide's icon URLs
    /// straight to whatever renders the guide. A deployment Plex reaches over a
    /// docker network therefore serves logos no viewer can load, and the fix
    /// cannot be `advertised_base_url` — moving that would move the stream URLs
    /// too, which is the thing that had to stay on the docker address.
    pub artwork_base_url: Option<String>,
    pub trusted_proxies: TrustedProxies,
    /// A backup to import on boot, when this instance has no users
    /// yet. See `api::importer::maybe_import_on_boot` for what that gate means.
    pub import_backup: Option<PathBuf>,
    pub log_filter: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustedProxies {
    /// Honour `X-Forwarded-*` from loopback and private peers. Opt-in, because
    /// on a home network every device on the operator's LAN is a private peer
    /// and would be able to assert its own address, scheme and host.
    PrivateAndLoopback,
    None,
    Cidrs(Vec<Cidr>),
}

/// A startup value that cannot be honoured.
///
/// Failing beats falling back: a fallback of `0.0.0.0:9191` for an unparseable
/// `DOLLET_LISTEN` would publish a mistyped loopback bind to the network.
#[derive(Debug, thiserror::Error)]
#[error("{variable}={value}: {reason}")]
pub struct ConfigError {
    pub variable: &'static str,
    pub value: String,
    pub reason: String,
}

impl ConfigError {
    fn new(variable: &'static str, value: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            variable,
            value: value.into(),
            reason: reason.into(),
        }
    }
}

/// The parsed form of `DOLLET_ADVERTISED_BASE_URL` and `DOLLET_ARTWORK_BASE_URL`.
///
/// Shared with `api::origin`, so what startup validation accepts and what the
/// request path can actually use cannot drift apart.
pub fn parse_advertised_base_url(raw: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(raw.trim().trim_end_matches('/')).ok()?;
    parsed.has_host().then_some(parsed)
}

/// Every `DOLLET_*` name this build reads.
///
/// The parser below uses these same literals, and
/// `every_variable_the_parser_reads_is_listed_as_known` pins the two together by
/// scanning this file's own source — so a variable added to `from_env` cannot
/// leave `unknown_variables` calling it a typo.
pub const KNOWN_VARIABLES: &[&str] = &[
    "DOLLET_LISTEN",
    "DOLLET_DATA_DIR",
    "DOLLET_ADVERTISED_BASE_URL",
    "DOLLET_ARTWORK_BASE_URL",
    "DOLLET_IMPORT_BACKUP",
    "DOLLET_TRUSTED_PROXIES",
    "DOLLET_LOG",
];

/// `DOLLET_*` names that are set but mean nothing to this build, sorted.
///
/// A variable set for a build that does not read it produces no error, no
/// effect and nothing to search the log for; this is the line that says so.
///
/// Returned rather than logged because `Config::from_env` runs before the
/// tracing subscriber exists, and printing to stderr from a library is how a
/// message ends up somewhere nobody is looking. A warning, never a refusal: a
/// compose file written for a newer build must still start an older binary, and
/// somebody's unrelated `DOLLET_`-prefixed shell variable is not our business.
pub fn unknown_variables() -> Vec<String> {
    unknown_among(std::env::vars().map(|(name, _)| name))
}

/// Taken as an iterator so the rule can be exercised without mutating the
/// process environment, which every other test in this binary shares.
fn unknown_among(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut unknown: Vec<String> = names
        .filter(|name| name.starts_with("DOLLET_") && !KNOWN_VARIABLES.contains(&name.as_str()))
        .collect();
    unknown.sort();
    unknown.dedup();
    unknown
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("dollet.sqlite")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir.join("cache")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }

    /// A database waiting to replace [`Self::db_path`] at the next boot. It is
    /// applied before the pool opens, because nothing may hold the file being
    /// replaced; see `backup::apply_staged`.
    pub fn staged_restore_path(&self) -> PathBuf {
        self.data_dir.join("dollet.sqlite.restore")
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        let listen = match std::env::var("DOLLET_LISTEN") {
            Ok(raw) => parse_listen(&raw)?,
            Err(_) => SocketAddr::from(([0, 0, 0, 0], 9191)),
        };

        let data_dir = std::env::var("DOLLET_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/data"));

        let advertised_base_url = match std::env::var("DOLLET_ADVERTISED_BASE_URL") {
            Ok(raw) => parse_base_url("DOLLET_ADVERTISED_BASE_URL", &raw)?,
            Err(_) => None,
        };

        let artwork_base_url = match std::env::var("DOLLET_ARTWORK_BASE_URL") {
            Ok(raw) => parse_base_url("DOLLET_ARTWORK_BASE_URL", &raw)?,
            Err(_) => None,
        };

        let import_backup = std::env::var("DOLLET_IMPORT_BACKUP")
            .ok()
            .and_then(|raw| parse_import_backup(&raw));

        let trusted_proxies = match std::env::var("DOLLET_TRUSTED_PROXIES") {
            Ok(raw) => parse_trusted_proxies(&raw)?,
            Err(_) => TrustedProxies::None,
        };

        Ok(Self {
            listen,
            data_dir,
            advertised_base_url,
            artwork_base_url,
            trusted_proxies,
            import_backup,
            log_filter: std::env::var("DOLLET_LOG").unwrap_or_else(|_| "info".into()),
        })
    }
}

/// An absolute base URL, or nothing. Kept as the operator wrote it — every
/// consumer reparses it through `parse_advertised_base_url`, and that is what
/// normalises the trailing slash and the default port.
///
/// Shared by both base-URL variables so they cannot disagree about what a usable
/// base is, and so a bad one names the variable that was actually set rather
/// than the other one.
fn parse_base_url(variable: &'static str, raw: &str) -> Result<Option<String>, ConfigError> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    parse_advertised_base_url(raw).ok_or_else(|| {
        ConfigError::new(
            variable,
            raw,
            "not an absolute URL with a host, e.g. https://tv.example.com",
        )
    })?;
    Ok(Some(raw.to_owned()))
}

/// Nothing is validated here but emptiness. Whether the file exists, is a zip
/// or a bare dump, and whether it parses are all questions the import answers,
/// where the answer can be a message about *this* backup rather than a startup
/// refusal naming an environment variable.
fn parse_import_backup(raw: &str) -> Option<PathBuf> {
    (!raw.trim().is_empty()).then(|| PathBuf::from(raw))
}

fn parse_listen(raw: &str) -> Result<SocketAddr, ConfigError> {
    raw.trim().parse().map_err(|_| {
        ConfigError::new(
            "DOLLET_LISTEN",
            raw,
            "not an address:port, e.g. 0.0.0.0:9191 or 127.0.0.1:9191",
        )
    })
}

/// `none`, the literal `private`, or a comma-separated CIDR list.
///
/// The default is `none` and `private` has to be asked for: a reverse proxy is
/// one address the operator can name, where "any RFC1918 peer" is every device
/// on their LAN, each of which would be believed about who it is.
fn parse_trusted_proxies(raw: &str) -> Result<TrustedProxies, ConfigError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return Ok(TrustedProxies::None);
    }
    if trimmed.eq_ignore_ascii_case("private") {
        return Ok(TrustedProxies::PrivateAndLoopback);
    }

    let mut cidrs = Vec::new();
    for entry in trimmed.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        cidrs.push(entry.parse::<Cidr>().map_err(|_| {
            ConfigError::new(
                "DOLLET_TRUSTED_PROXIES",
                raw,
                format!("`{entry}` is not `none`, `private`, or a CIDR block"),
            )
        })?);
    }

    // Parsed once here rather than per request: this is consulted on every
    // request that carries a forwarding header.
    Ok(if cidrs.is_empty() {
        TrustedProxies::None
    } else {
        TrustedProxies::Cidrs(cidrs)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_paths_from_data_dir() {
        let cfg = Config {
            listen: SocketAddr::from(([0, 0, 0, 0], 9191)),
            data_dir: PathBuf::from("/tmp/dollet-relay"),
            advertised_base_url: None,
            artwork_base_url: None,
            trusted_proxies: TrustedProxies::None,
            import_backup: None,
            log_filter: "info".into(),
        };
        assert_eq!(
            cfg.db_path(),
            PathBuf::from("/tmp/dollet-relay/dollet.sqlite")
        );
        assert_eq!(cfg.cache_dir(), PathBuf::from("/tmp/dollet-relay/cache"));
        assert_eq!(
            cfg.backups_dir(),
            PathBuf::from("/tmp/dollet-relay/backups")
        );
        assert_eq!(
            cfg.staged_restore_path(),
            PathBuf::from("/tmp/dollet-relay/dollet.sqlite.restore")
        );
    }

    #[test]
    fn a_listen_address_that_will_not_parse_stops_the_process() {
        // Falling back to 0.0.0.0:9191 would let a typo in a loopback bind
        // publish the port to the whole network.
        assert_eq!(
            parse_listen("127.0.0.1:9191").unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 9191))
        );

        for bad in ["9191", "localhost:9191", "0.0.0.0", ""] {
            let error = parse_listen(bad).unwrap_err();
            assert_eq!(error.variable, "DOLLET_LISTEN");
            assert_eq!(error.value, bad);
        }
    }

    #[test]
    fn nothing_is_trusted_unless_it_is_named() {
        assert_eq!(parse_trusted_proxies("").unwrap(), TrustedProxies::None);
        assert_eq!(parse_trusted_proxies("none").unwrap(), TrustedProxies::None);
        assert_eq!(
            parse_trusted_proxies("private").unwrap(),
            TrustedProxies::PrivateAndLoopback
        );

        let cidrs = parse_trusted_proxies(" 172.16.0.0/12 , 10.1.2.3 ").unwrap();
        assert_eq!(
            cidrs,
            TrustedProxies::Cidrs(vec![
                "172.16.0.0/12".parse().unwrap(),
                "10.1.2.3".parse().unwrap(),
            ])
        );
    }

    #[test]
    fn a_proxy_list_that_will_not_parse_is_refused_rather_than_ignored() {
        // Silently dropping the bad entry would leave a deployment believing it
        // trusts its proxy while every forwarded header is discarded.
        let error = parse_trusted_proxies("10.0.0.0/8,not-a-cidr").unwrap_err();
        assert_eq!(error.variable, "DOLLET_TRUSTED_PROXIES");
        assert!(error.reason.contains("not-a-cidr"), "{}", error.reason);
    }

    #[test]
    fn an_import_backup_is_taken_as_given_or_not_at_all() {
        assert_eq!(
            parse_import_backup("/data/dispatcharr-backup.zip"),
            Some(PathBuf::from("/data/dispatcharr-backup.zip"))
        );
        // A path is whatever the filesystem says it is, spaces included.
        assert_eq!(
            parse_import_backup("/data/my backup.zip"),
            Some(PathBuf::from("/data/my backup.zip"))
        );
        for unset in ["", "   ", "\n"] {
            assert_eq!(parse_import_backup(unset), None);
        }
    }

    #[test]
    fn an_advertised_base_url_needs_a_scheme_and_a_host() {
        assert!(parse_advertised_base_url("https://tv.example.com/").is_some());
        assert!(parse_advertised_base_url("http://tv.example.com:8443").is_some());

        for bad in ["", "tv.example.com", "not a url", "mailto:a@b", "https://"] {
            assert!(parse_advertised_base_url(bad).is_none(), "{bad} accepted");
        }
    }

    #[test]
    fn an_artwork_base_url_is_validated_exactly_as_the_advertised_one_is() {
        // The same rule, and the same normalisation: the trailing slash is
        // stripped before parsing, so `…com/` and `…com` are one address.
        assert_eq!(
            parse_base_url("DOLLET_ARTWORK_BASE_URL", "https://tv.example.com/").unwrap(),
            Some("https://tv.example.com/".to_owned())
        );
        assert_eq!(
            parse_base_url("DOLLET_ARTWORK_BASE_URL", "http://172.25.0.41:9191").unwrap(),
            Some("http://172.25.0.41:9191".to_owned())
        );

        // Unset and set to nothing are the same thing.
        for unset in ["", "   "] {
            assert_eq!(
                parse_base_url("DOLLET_ARTWORK_BASE_URL", unset).unwrap(),
                None
            );
        }

        // And the refusal names the variable that was actually set, not the
        // other one that shares the rule.
        for bad in ["tv.example.com", "not a url", "https://"] {
            let error = parse_base_url("DOLLET_ARTWORK_BASE_URL", bad).unwrap_err();
            assert_eq!(error.variable, "DOLLET_ARTWORK_BASE_URL");
            assert_eq!(error.value, bad);
            assert!(error.reason.contains("absolute URL"), "{}", error.reason);
        }
    }

    #[test]
    fn a_dollet_variable_this_build_does_not_read_is_reported() {
        let unknown = unknown_among(
            [
                "DOLLET_ARTWORK_BASE_URL",
                "DOLLET_LISTEN",
                "DOLLET_LOG",
                "DOLLET_ARTWORK_URL",
                "DOLLET_ADVERTISED_BASE_URl",
                "PATH",
                "ARTWORK_BASE_URL",
            ]
            .into_iter()
            .map(str::to_owned),
        );

        // The typo and the near-miss, and nothing else: a variable this build
        // reads is not a typo, and a name without the prefix is not ours to
        // have an opinion about. Case matters, because `std::env` does.
        assert_eq!(
            unknown,
            vec!["DOLLET_ADVERTISED_BASE_URl", "DOLLET_ARTWORK_URL"]
        );

        assert!(unknown_among(KNOWN_VARIABLES.iter().map(|n| (*n).to_owned())).is_empty());
    }

    /// The warning is only as good as its list, and the list is a second copy of
    /// the names `from_env` reads. This is the check that keeps the copy honest:
    /// every `DOLLET_*` name this file pulls out of the environment has to be in
    /// `KNOWN_VARIABLES`. `main.rs` carries the matching check that it reads
    /// none of its own.
    #[test]
    fn every_variable_the_parser_reads_is_listed_as_known() {
        let source = include_str!("config.rs");
        let needle = concat!("std::env::var(", '"');

        let read: Vec<&str> = source
            .match_indices(needle)
            .filter_map(|(at, _)| {
                let rest = &source[at + needle.len()..];
                rest.split_once('"').map(|(name, _)| name)
            })
            .filter(|name| name.starts_with("DOLLET_"))
            .collect();

        assert!(read.len() >= KNOWN_VARIABLES.len(), "{read:?}");
        for name in read {
            assert!(
                KNOWN_VARIABLES.contains(&name),
                "{name} is read but would be warned about as unrecognised"
            );
        }
    }
}
