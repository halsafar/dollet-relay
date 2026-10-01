//! Reading a provider's catalogue through the Xtream Codes API.
//!
//! An Xtream account could be refreshed through `get.php?type=m3u_plus`, which
//! is one request instead of two and yields an ordinary playlist. It is the
//! wrong choice, and the reason is the dedup hash.
//!
//! `sync::hash` substitutes the provider's own `stream_id` for the URL when
//! hashing an Xtream stream, because an Xtream provider rotates the credentials
//! embedded in every stream URL — hash the URL and the whole catalogue orphans
//! itself the first time the provider rotates them. Every existing row becomes
//! unmatched, the refresh reports hundreds of new streams, and every channel's
//! failover list points at rows that are about to expire.
//!
//! That substitution needs a stream id, and `get.php` does not carry one: its
//! `#EXTINF` attributes are `tvg-id`, `tvg-name`, `tvg-logo`, `tvg-chno`,
//! `tvc-guide-stationid` and `group-title`, with the id appearing only inside
//! the URL path it is meant to replace. `player_api.php` returns it as a field.
//! So: two requests, and the branch that exists for this actually fires.

use std::collections::BTreeMap;

use dollet_core::Error;
use dollet_core::domain::M3uAccount;
use dollet_core::sync::streams::StreamFields;
use serde::Deserialize;

use crate::AppState;
use crate::api::jobs::JobHandle;

/// One entry of `action=get_live_categories`.
#[derive(Deserialize)]
struct Category {
    /// A string in every provider response seen, including our own server's.
    category_id: String,
    category_name: String,
}

/// One entry of `action=get_live_streams`.
///
/// Typed loosely on purpose: `stream_id` and `is_adult` arrive as a number from
/// some providers and a quoted number from others, and a refresh that fails to
/// deserialise is a refresh that deletes nothing but also imports nothing.
#[derive(Deserialize)]
struct LiveStream {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    stream_id: Option<serde_json::Value>,
    #[serde(default)]
    num: Option<serde_json::Value>,
    #[serde(default)]
    stream_icon: Option<String>,
    #[serde(default)]
    epg_channel_id: Option<String>,
    #[serde(default)]
    category_id: Option<String>,
    #[serde(default)]
    is_adult: Option<serde_json::Value>,
}

fn as_i64(value: &Option<serde_json::Value>) -> Option<i64> {
    match value.as_ref()? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_f64(value: &Option<serde_json::Value>) -> Option<f64> {
    match value.as_ref()? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The base every Xtream URL is built from, with any trailing slash and any
/// `player_api.php`/`get.php` the operator pasted in removed.
///
/// Operators paste the URL their provider gave them, which is as often the full
/// `player_api.php` line as the bare host. Appending to that produces a 404 and
/// an empty catalogue, which reconcile reads as "the provider dropped
/// everything".
fn base_url(account: &M3uAccount) -> Result<String, Error> {
    let raw = account
        .server_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| Error::invalid("account has neither a server URL nor a file"))?;

    let mut url = url::Url::parse(raw)
        .map_err(|_| Error::invalid("the account's server URL is not a URL"))?;
    let path = url.path().to_owned();
    for suffix in ["player_api.php", "get.php", "panel_api.php", "xmltv.php"] {
        if let Some(trimmed) = path.strip_suffix(suffix) {
            url.set_path(trimmed);
            break;
        }
    }
    url.set_query(None);
    url.set_fragment(None);

    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn action_url(base: &str, account: &M3uAccount, action: &str) -> String {
    format!(
        "{base}/player_api.php?username={}&password={}&action={action}",
        crate::api::urlencode(account.username.as_deref().unwrap_or_default()),
        crate::api::urlencode(account.password.as_deref().unwrap_or_default()),
    )
}

/// Fetch one action and deserialise it.
///
/// Through `super::download` rather than a direct request so the SSRF guard,
/// the size cap and the credential redaction are the same ones every other
/// provider fetch goes through, rather than a second set that has to be kept in
/// step.
async fn fetch<T: serde::de::DeserializeOwned>(
    state: &AppState,
    account: &M3uAccount,
    user_agent: &str,
    action: &str,
    slot: &str,
) -> Result<Vec<T>, Error> {
    let url = action_url(&base_url(account)?, account, action);
    let destination = super::feed_path(state, slot, account.id);
    super::download(&url, user_agent, &destination).await?;

    let bytes = tokio::fs::read(&destination).await?;
    serde_json::from_slice(&bytes).map_err(|e| {
        // An Xtream server answers bad credentials with a JSON object rather
        // than an array, so this is the shape a wrong password arrives in.
        Error::upstream(format!(
            "the provider's `{action}` response was not a list of entries \
             (check the account's username and password): {e}"
        ))
    })
}

/// The account's live catalogue, as the fields reconciliation reads.
pub async fn catalogue(
    state: &AppState,
    account: &M3uAccount,
    user_agent: &str,
    default_group: &str,
    handle: &JobHandle,
) -> Result<Vec<StreamFields>, Error> {
    let base = base_url(account)?;

    handle.progress(0.05, "fetching categories").await;
    let categories: Vec<Category> =
        fetch(state, account, user_agent, "get_live_categories", "xc-cat").await?;

    handle.progress(0.15, "fetching streams").await;
    let streams: Vec<LiveStream> =
        fetch(state, account, user_agent, "get_live_streams", "xc-live").await?;

    Ok(entries(&base, account, default_group, categories, streams))
}

/// Turn the two responses into stream fields. Split from the fetch so it can be
/// driven from the responses in `fixtures/golden/`.
fn entries(
    base: &str,
    account: &M3uAccount,
    default_group: &str,
    categories: Vec<Category>,
    streams: Vec<LiveStream>,
) -> Vec<StreamFields> {
    let names: BTreeMap<String, String> = categories
        .into_iter()
        .map(|category| (category.category_id, category.category_name))
        .collect();

    // The stream URL an Xtream client would use, which is also what the proxy
    // will fetch. The credentials in it are the ones that rotate, and the whole
    // reason the hash keys on `stream_id` instead.
    let prefix = format!(
        "{base}/live/{}/{}",
        crate::api::urlencode(account.username.as_deref().unwrap_or_default()),
        crate::api::urlencode(account.password.as_deref().unwrap_or_default()),
    );

    streams
        .into_iter()
        .filter_map(|stream| {
            // No id means no URL to build and nothing for the hash to key on.
            // Skipped rather than guessed: a fabricated id collides with a real
            // one on the next refresh and merges two streams into one row.
            let stream_id = as_i64(&stream.stream_id)?;

            Some(StreamFields {
                // A provider that sends no name still has to be addressable, so
                // it is named after the account and the id.
                name: stream
                    .name
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| format!("{} - {stream_id}", account.name)),
                url: format!("{prefix}/{stream_id}.ts"),
                logo_url: stream.stream_icon.unwrap_or_default(),
                tvg_id: stream.epg_channel_id.unwrap_or_default(),
                group: stream
                    .category_id
                    .as_ref()
                    .and_then(|id| names.get(id))
                    .cloned()
                    .unwrap_or_else(|| default_group.to_owned()),
                is_adult: as_i64(&stream.is_adult).unwrap_or(0) != 0,
                provider_stream_id: Some(stream_id),
                provider_channel_number: as_f64(&stream.num),
                // Catch-up is out of scope for 1.0, so nothing advertises an
                // archive this cannot serve.
                is_catchup: false,
                catchup_days: 0,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(server_url: &str) -> M3uAccount {
        M3uAccount {
            id: 2,
            name: "Provider".into(),
            server_url: Some(server_url.into()),
            file_path: None,
            username: Some("bob smith".into()),
            password: Some("p@ss/word".into()),
            account_type: dollet_core::domain::M3uAccountType::XtreamCodes,
            max_streams: 0,
            is_active: true,
            locked: false,
            priority: 0,
            user_agent_id: None,
            stream_profile_id: None,
            refresh_interval_hours: 24,
            stale_stream_days: 7,
            custom_properties: serde_json::json!({}),
        }
    }

    #[test]
    fn the_base_url_survives_whatever_the_operator_pasted() {
        for raw in [
            "http://provider.example:8080",
            "http://provider.example:8080/",
            "http://provider.example:8080/player_api.php",
            "http://provider.example:8080/get.php?username=a&password=b&type=m3u_plus",
        ] {
            assert_eq!(
                base_url(&account(raw)).unwrap(),
                "http://provider.example:8080",
                "{raw}"
            );
        }
    }

    #[test]
    fn credentials_are_escaped_into_every_url() {
        let account = account("http://provider.example:8080");
        let url = action_url(
            &base_url(&account).unwrap(),
            &account,
            "get_live_categories",
        );
        assert_eq!(
            url,
            "http://provider.example:8080/player_api.php\
             ?username=bob%20smith&password=p%40ss%2Fword&action=get_live_categories"
        );
    }

    /// The provider's own numbers arrive quoted as often as not.
    #[test]
    fn numeric_fields_are_read_in_both_of_the_shapes_providers_send() {
        let quoted: LiveStream =
            serde_json::from_str(r#"{"stream_id":"171","num":"1.5","is_adult":"1","name":"Q"}"#)
                .unwrap();
        assert_eq!(as_i64(&quoted.stream_id), Some(171));
        assert_eq!(as_f64(&quoted.num), Some(1.5));
        assert_eq!(as_i64(&quoted.is_adult), Some(1));

        let bare: LiveStream =
            serde_json::from_str(r#"{"stream_id":171,"num":1,"is_adult":0,"name":"B"}"#).unwrap();
        assert_eq!(as_i64(&bare.stream_id), Some(171));
        assert_eq!(as_f64(&bare.num), Some(1.0));
        assert_eq!(as_i64(&bare.is_adult), Some(0));
    }

    fn golden<T: serde::de::DeserializeOwned>(name: &str) -> Vec<T> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/golden")
            .join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        serde_json::from_slice(&bytes).unwrap()
    }

    /// The whole reason this module exists, against a real pair of responses.
    ///
    /// The golden responses are this server answering as an Xtream provider,
    /// which is the same API the real one speaks; they are strong evidence
    /// rather than proof, and they are the closest thing to a provider that can
    /// be run in a test.
    #[test]
    fn a_credential_rotation_does_not_move_a_single_hash() {
        let categories = golden::<Category>("xc-get-live-categories.json");
        let streams = golden::<LiveStream>("xc-get-live-streams.json");
        assert_eq!(
            streams.len(),
            16,
            "the golden response is not the one expected"
        );

        let before = account("http://provider.example:8080");
        let mut after = account("http://provider.example:8080");
        after.username = Some("bob2".into());
        after.password = Some("rotated".into());

        let old = entries(
            "http://provider.example:8080",
            &before,
            "Default Group",
            golden::<Category>("xc-get-live-categories.json"),
            golden::<LiveStream>("xc-get-live-streams.json"),
        );
        let new = entries(
            "http://provider.example:8080",
            &after,
            "Default Group",
            categories,
            streams,
        );

        assert_eq!(old.len(), 16);
        assert!(
            old.iter().all(|fields| fields.provider_stream_id.is_some()),
            "an entry reached reconciliation with no provider stream id, which is \
             what `get.php` yields"
        );
        assert_ne!(
            old[0].url, new[0].url,
            "the rotation did not change the URLs"
        );

        // `url` is in the key, and the URL is entirely different — so this
        // passing is the substitution working, and nothing else.
        let keys = dollet_core::sync::hash::parse_keys("url");
        for (old, new) in old.iter().zip(&new) {
            let identity = |fields: &StreamFields, account: &M3uAccount| {
                dollet_core::sync::hash::stream_hash(
                    &dollet_core::sync::hash::StreamIdentity {
                        name: &fields.name,
                        url: &fields.url,
                        tvg_id: &fields.tvg_id,
                        group: &fields.group,
                        m3u_account_id: account.id,
                        account_type: account.account_type,
                        provider_stream_id: fields.provider_stream_id,
                    },
                    &keys,
                )
            };
            assert_eq!(
                identity(old, &before),
                identity(new, &after),
                "{} moved when the credentials rotated, so the next refresh would \
                 call it a new stream and expire the old row",
                old.name
            );
        }
    }

    fn synthetic<T: serde::de::DeserializeOwned>(name: &str) -> Vec<T> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/synthetic/providers")
            .join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The hand-written catalogue, which carries the shapes the golden corpus
    /// does not: a quoted id beside a numeric one, an entry with no `name` at all,
    /// an entry whose name is whitespace, a category nothing is in, a category
    /// id no category declares, and an entry with no id.
    ///
    /// Driven through `entries` rather than through `catalogue`, which is the
    /// one gap in this file: `catalogue` fetches over HTTP, `allow_private` is
    /// false for provider feeds, and loopback is refused regardless of that
    /// flag — so a mock server cannot be reached and there is nothing between
    /// `download` and here for a test to stand in.
    #[test]
    fn the_hand_written_catalogue_covers_what_the_golden_corpus_cannot() {
        let account = {
            let mut account = account("https://xtream.example:8080");
            account.name = "Synth Xtream".into();
            account.username = Some("synthxc".into());
            account.password = Some("synthxcpass".into());
            account
        };

        let fields = entries(
            "https://xtream.example:8080",
            &account,
            "Default Group",
            synthetic::<Category>("xtream-get-live-categories.json"),
            synthetic::<LiveStream>("xtream-get-live-streams.json"),
        );

        // Five of the six entries survive. The one with no `stream_id` is
        // skipped rather than given a fabricated one: a made-up id collides
        // with a real one on the next refresh and merges two streams into one
        // row.
        assert_eq!(fields.len(), 5);
        assert!(
            !fields.iter().any(|f| f.name == "Synth XC No Id"),
            "an entry with no stream id reached reconciliation"
        );

        // A quoted id is the same id as a bare one, on every field that can
        // arrive either way.
        let quoted = &fields[1];
        assert_eq!(quoted.provider_stream_id, Some(2003));
        assert_eq!(quoted.provider_channel_number, Some(2.0));
        assert!(quoted.is_adult, "a quoted `is_adult` was read as false");
        assert_eq!(fields[0].provider_channel_number, Some(1.0));
        assert_eq!(fields[2].provider_channel_number, Some(3.5));

        // A provider that sends no name, and one that sends whitespace, both
        // still have to be addressable.
        assert_eq!(fields[2].name, "Synth Xtream - 2004");
        assert_eq!(fields[3].name, "Synth Xtream - 2005");

        // A category the provider declares and puts nothing in never reaches a
        // stream, and a category id no category declares falls back to the
        // default group rather than dropping the entry.
        let groups: Vec<&str> = fields.iter().map(|f| f.group.as_str()).collect();
        assert!(
            !groups.contains(&"Synth Empty Category"),
            "a category with no streams in it became a group: {groups:?}"
        );
        assert_eq!(fields[4].group, "Default Group");
        assert_eq!(groups[..2], ["Synth Sports", "Synth News"]);

        // Catch-up is out of scope for 1.0, so nothing here may advertise an
        // archive this server has no handler for.
        assert!(fields.iter().all(|f| !f.is_catchup && f.catchup_days == 0));
    }

    /// The whole reason this module exists, on the hand-written catalogue: an
    /// Xtream provider rotates the credentials embedded in every stream URL,
    /// and the dedup key must not move when it does.
    #[test]
    fn a_rotation_of_the_synthetic_credentials_moves_no_hash() {
        let before = {
            let mut account = account("https://xtream.example:8080");
            account.name = "Synth Xtream".into();
            account.username = Some("synthxc".into());
            account.password = Some("synthxcpass".into());
            account
        };
        let mut after = before.clone();
        after.username = Some("synthxc2".into());
        after.password = Some("rotated-in-the-night".into());

        let old = entries(
            "https://xtream.example:8080",
            &before,
            "Default Group",
            synthetic::<Category>("xtream-get-live-categories.json"),
            synthetic::<LiveStream>("xtream-get-live-streams.json"),
        );
        let new = entries(
            "https://xtream.example:8080",
            &after,
            "Default Group",
            synthetic::<Category>("xtream-get-live-categories.json"),
            synthetic::<LiveStream>("xtream-get-live-streams.json"),
        );

        assert_ne!(old[0].url, new[0].url, "the rotation changed no URL");

        // `url` is the key, and every URL is different — so these matching is
        // the stream-id substitution working and nothing else.
        let keys = dollet_core::sync::hash::parse_keys("url");
        let hash = |fields: &StreamFields, account: &M3uAccount| {
            dollet_core::sync::hash::stream_hash(
                &dollet_core::sync::hash::StreamIdentity {
                    name: &fields.name,
                    url: &fields.url,
                    tvg_id: &fields.tvg_id,
                    group: &fields.group,
                    m3u_account_id: account.id,
                    account_type: account.account_type,
                    provider_stream_id: fields.provider_stream_id,
                },
                &keys,
            )
        };
        for (old, new) in old.iter().zip(&new) {
            assert_eq!(
                hash(old, &before),
                hash(new, &after),
                "{} moved when the credentials rotated",
                old.name
            );
        }

        // And a *standard* account in the same position does move, which is
        // what makes the substitution a choice rather than an accident.
        let mut standard = before.clone();
        standard.account_type = dollet_core::domain::M3uAccountType::Standard;
        let mut rotated = after.clone();
        rotated.account_type = dollet_core::domain::M3uAccountType::Standard;
        assert_ne!(hash(&old[0], &standard), hash(&new[0], &rotated));
    }

    /// The same corpus read the way `get.php` delivers it: no stream id at all.
    #[test]
    fn the_playlist_form_is_why_this_module_exists() {
        let playlist = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/golden/xc-get-php.m3u"),
        )
        .unwrap();
        let parsed = dollet_core::parse::m3u::parse(&playlist).unwrap();

        assert_eq!(parsed.entries.len(), 16);
        assert!(
            parsed
                .entries
                .iter()
                .all(|entry| entry.attr("stream_id").is_none()),
            "`get.php` carries a stream id after all, and this module is \
             unnecessary"
        );
    }
}
