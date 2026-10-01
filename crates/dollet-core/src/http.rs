//! Outbound HTTP with SSRF protection.
//!
//! Every URL this project fetches is attacker-influenced: M3U bodies, EPG icon
//! URLs, artwork, provider APIs. Guarding that takes three checks, and each has
//! a test.
//!
//! For a *hostname*, validation happens inside DNS resolution rather than
//! before the request, because a check-then-fetch leaves a window where the
//! second lookup returns a different address (DNS rebinding).
//!
//! For a *bare address* there is no resolution to hook — hyper dials
//! `http://169.254.169.254/` exactly as written — so the resolver never runs
//! and the URL has to be judged up front.
//!
//! For a *redirect*, the target is a URL the caller never saw and the provider
//! chose. A 302 is the cheapest way to turn a permitted fetch into a forbidden
//! one, so every hop is judged again.
//!
//! [`get`] applies all three. [`client`] alone applies the first and third.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

/// Networks a fetch may never reach, regardless of policy.
///
/// Link-local covers cloud metadata services (169.254.169.254), which is the
/// single most valuable SSRF target on a hosted box.
fn is_always_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                // fe80::/10 link-local and fec0::/10 site-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || (v6.segments()[0] & 0xffc0) == 0xfec0
        }
    }
}

/// RFC1918 and friends. Allowed only when the caller opts in, because LAN
/// providers and local artwork are legitimate targets here.
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64,
        IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

fn is_allowed(ip: IpAddr, allow_private: bool) -> bool {
    if is_always_blocked(ip) {
        return false;
    }
    allow_private || !is_private(ip)
}

#[derive(Debug, thiserror::Error)]
pub enum SsrfError {
    #[error("no addresses resolved for {0}")]
    Unresolved(String),
    #[error("{host} resolves only to blocked addresses")]
    Blocked { host: String },
    #[error("not a fetchable url: {0}")]
    Unparseable(String),
    #[error("too many redirects")]
    TooManyRedirects,
}

struct GuardedResolver {
    allow_private: bool,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?
                .collect();

            if resolved.is_empty() {
                return Err(Box::new(SsrfError::Unresolved(host)) as _);
            }

            let permitted: Vec<SocketAddr> = resolved
                .into_iter()
                .filter(|addr| is_allowed(addr.ip(), allow_private))
                .collect();

            if permitted.is_empty() {
                return Err(Box::new(SsrfError::Blocked { host }) as _);
            }

            Ok(Box::new(permitted.into_iter()) as Addrs)
        })
    }
}

/// Build a client whose DNS resolution refuses blocked address space.
///
/// **Necessary but not sufficient on its own.** The resolver only runs for
/// hosts that need resolving, so a URL carrying a bare address —
/// `http://169.254.169.254/`, `http://127.0.0.1:9191/` — never reaches it.
/// Pair it with [`check_url`], or just use [`get`], which does both.
///
/// `allow_private` is for fetches that legitimately target the LAN (a local
/// provider, artwork on the same network). Leave it off for anything reachable
/// from user-supplied configuration without review.
pub fn client(user_agent: &str, allow_private: bool) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        // Every hop is a new URL the caller never saw. Without this, a provider
        // answers an artwork fetch with a 302 to 169.254.169.254 and the guard
        // only ever inspected hop zero.
        .redirect(reqwest::redirect::Policy::custom(
            move |attempt| match may_follow(
                attempt.url().as_str(),
                attempt.previous().len(),
                allow_private,
            ) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            },
        ))
        .dns_resolver(Arc::new(GuardedResolver { allow_private }))
        .build()
}

/// Whether a redirect may be followed to `url`.
///
/// Split out of the policy closure because the closure cannot be reached from a
/// test: exercising it end to end needs a source on non-blocked space, and a
/// test only has loopback, which fails at hop zero before any redirect happens.
fn may_follow(url: &str, hops: usize, allow_private: bool) -> Result<(), SsrfError> {
    if hops >= MAX_REDIRECTS {
        return Err(SsrfError::TooManyRedirects);
    }
    check_url(url, allow_private)
}

const MAX_REDIRECTS: usize = 10;

/// Reject a URL whose host is an address in blocked space.
///
/// Closes the half [`client`] cannot see. A hostname passes here and is judged
/// later by the resolver, which is also what keeps the rebinding window shut:
/// the address that gets dialled is the one that was checked.
pub fn check_url(url: &str, allow_private: bool) -> Result<(), SsrfError> {
    let parsed = Url::parse(url).map_err(|_| SsrfError::Unparseable(url.to_owned()))?;

    match parsed.host() {
        // A bare address is dialled as written, so judge it now.
        Some(Host::Ipv4(ip)) if !is_allowed(IpAddr::V4(ip), allow_private) => {
            Err(SsrfError::Blocked {
                host: ip.to_string(),
            })
        }
        Some(Host::Ipv6(ip)) if !is_allowed(IpAddr::V6(ip), allow_private) => {
            Err(SsrfError::Blocked {
                host: ip.to_string(),
            })
        }
        Some(_) => Ok(()),
        None => Err(SsrfError::Unparseable(url.to_owned())),
    }
}

/// Fetch a URL with both halves of the guard applied.
///
/// Prefer this over driving [`client`] directly; it is the only path where
/// forgetting [`check_url`] is impossible.
pub async fn get(
    client: &reqwest::Client,
    url: &str,
    allow_private: bool,
) -> Result<reqwest::Response, FetchError> {
    check_url(url, allow_private)?;
    Ok(client.get(url).send().await?)
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error(transparent)]
    Refused(#[from] SsrfError),
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_loopback_and_metadata_regardless_of_policy() {
        for addr in ["127.0.0.1", "169.254.169.254", "0.0.0.0", "224.0.0.1"] {
            assert!(
                !is_allowed(ip(addr), false),
                "{addr} allowed with private off"
            );
            assert!(
                !is_allowed(ip(addr), true),
                "{addr} allowed with private on"
            );
        }
    }

    #[test]
    fn blocks_ipv6_loopback_and_link_local() {
        for addr in ["::1", "fe80::1", "ff02::1", "::"] {
            assert!(!is_allowed(ip(addr), true), "{addr} allowed");
        }
    }

    #[test]
    fn private_ranges_follow_the_flag() {
        for addr in ["192.168.1.10", "10.0.0.5", "172.16.0.1", "100.64.0.1"] {
            assert!(
                !is_allowed(ip(addr), false),
                "{addr} allowed with private off"
            );
            assert!(is_allowed(ip(addr), true), "{addr} blocked with private on");
        }
        assert!(!is_allowed(ip("fc00::1"), false));
        assert!(is_allowed(ip("fc00::1"), true));
    }

    #[test]
    fn public_addresses_are_allowed() {
        for addr in ["1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(is_allowed(ip(addr), false), "{addr} blocked");
        }
    }

    #[test]
    fn bare_addresses_are_refused_before_any_request() {
        for url in [
            "http://127.0.0.1:9191/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:8080/x",
            "http://0.0.0.0/",
        ] {
            assert!(check_url(url, true).is_err(), "{url} admitted");
        }
    }

    #[test]
    fn bare_private_addresses_follow_the_flag() {
        let lan = "http://192.168.1.50:8080/playlist.m3u";
        assert!(
            check_url(lan, false).is_err(),
            "lan admitted with private off"
        );
        assert!(check_url(lan, true).is_ok(), "lan refused with private on");
    }

    #[test]
    fn hostnames_pass_here_and_are_judged_by_the_resolver() {
        assert!(check_url("http://provider.example/get.php", false).is_ok());
        // Including one that will resolve to loopback — that is the resolver's
        // call, and deferring it is what keeps the rebinding window shut.
        assert!(check_url("http://localhost/x", false).is_ok());
    }

    #[test]
    fn unfetchable_urls_are_refused_rather_than_dialled() {
        for url in ["not a url", "file:///etc/passwd", "http://", ""] {
            assert!(check_url(url, true).is_err(), "{url} admitted");
        }
    }

    /// Checking the URL the caller handed over says nothing about where a 302
    /// sends the fetch next, and a provider controls that.
    #[test]
    fn a_redirect_into_blocked_space_is_refused() {
        // allow_private is on, so only always-blocked space may refuse these.
        for target in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:9191/api/core/settings/",
            "http://[::1]/",
        ] {
            assert!(
                may_follow(target, 1, true).is_err(),
                "{target} would have been followed"
            );
        }
        assert!(may_follow("http://provider.example/logo.png", 1, false).is_ok());
    }

    #[test]
    fn a_redirect_chain_is_bounded() {
        let ok = "http://provider.example/x";
        assert!(may_follow(ok, 9, false).is_ok());
        assert!(matches!(
            may_follow(ok, 10, false),
            Err(SsrfError::TooManyRedirects)
        ));
    }

    /// The guarded resolver never runs for a URL that carries its own address,
    /// so `client` alone lets loopback through.
    #[tokio::test]
    async fn get_refuses_a_loopback_server_that_client_alone_would_reach() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = client("test", false).unwrap();
        let url = server.uri();

        assert!(
            client.get(&url).send().await.is_ok(),
            "precondition: the raw client reaches loopback, which is why get() exists"
        );
        assert!(matches!(
            get(&client, &url, false).await,
            Err(FetchError::Refused(_))
        ));
    }
}
