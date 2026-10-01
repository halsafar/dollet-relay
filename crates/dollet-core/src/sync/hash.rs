//! The dedup key that decides whether a refreshed stream is the same stream.
//!
//! This is the highest-stakes function in the sync path. The hash is what links
//! a parsed playlist entry to the row already in the database, and every channel
//! assignment hangs off that row. Compute it even slightly differently from the
//! value already stored and the next refresh matches nothing: every stream is
//! "new", every old one goes stale, and the user's curated lineup is gone.
//!
//! So it reproduces the stored hash's bytes exactly, including the parts that look
//! accidental — Python's `json.dumps` defaults, which put a space after every
//! `:` and `,` and escape every non-ASCII character. Those are load-bearing.

use sha2::{Digest, Sha256};

use crate::domain::{Id, M3uAccountType};

/// A field that may take part in the dedup key.
///
/// The selection is stored as the comma-separated `m3u_hash_key` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HashKey {
    Group,
    M3uId,
    Name,
    TvgId,
    Url,
}

impl HashKey {
    /// The JSON object key, which is also what sorts the object.
    fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::M3uId => "m3u_id",
            Self::Name => "name",
            Self::TvgId => "tvg_id",
            Self::Url => "url",
        }
    }

    /// Parse one `m3u_hash_key` element. Unknown names are dropped — a typo
    /// narrows the key rather than failing the refresh.
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "group" => Some(Self::Group),
            "m3u_id" => Some(Self::M3uId),
            "name" => Some(Self::Name),
            "tvg_id" => Some(Self::TvgId),
            "url" => Some(Self::Url),
            _ => None,
        }
    }
}

/// Parse the `m3u_hash_key` setting; the shipped default is `url`.
///
/// Order is irrelevant — the JSON object is sorted by key before hashing — and
/// duplicates collapse, because the hashed object is a map.
///
/// **An empty result means every stream in the account hashes identically**, so
/// a whole playlist collapses onto one row. That is not hypothetical and not a
/// bug here: an imported instance can carry the setting empty, and an empty
/// selection is nothing. Reproducing it is correct — the imported rows are
/// already in that state — but the caller must not let it pass quietly.
/// [`crate::sync::streams::Plan::hash_key_selects_nothing`] reports the effect
/// when a refresh runs into it.
pub fn parse_keys(setting: &str) -> Vec<HashKey> {
    let mut keys: Vec<HashKey> = setting.split(',').filter_map(HashKey::parse).collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Everything the key may be computed from.
pub struct StreamIdentity<'a> {
    pub name: &'a str,
    pub url: &'a str,
    pub tvg_id: &'a str,
    pub group: &'a str,
    pub m3u_account_id: Id,
    pub account_type: M3uAccountType,
    /// The provider's own id for this stream, where it has one.
    pub provider_stream_id: Option<i64>,
}

/// One JSON value, in the two shapes a stream field can take.
enum Value<'a> {
    Text(&'a str),
    Number(i64),
}

pub fn stream_hash(identity: &StreamIdentity<'_>, keys: &[HashKey]) -> String {
    // An Xtream provider rotates the credentials embedded in its stream URLs,
    // so hashing the URL would orphan the whole catalogue on every rotation.
    // The provider's own stream id is stable, so it stands in. Note the zero
    // check: the format treats a zero id as absent, because 0 is falsy in the
    // Python it was defined in.
    let use_stream_id = identity.account_type == M3uAccountType::XtreamCodes
        && identity.provider_stream_id.is_some_and(|id| id != 0)
        && keys.contains(&HashKey::Url);

    let mut parts: Vec<(&'static str, Value<'_>)> = Vec::with_capacity(keys.len() + 1);
    for key in keys {
        let value = match key {
            HashKey::Group => Value::Text(identity.group),
            HashKey::M3uId => Value::Number(identity.m3u_account_id),
            HashKey::Name => Value::Text(identity.name),
            HashKey::TvgId => Value::Text(identity.tvg_id),
            HashKey::Url if use_stream_id => {
                Value::Number(identity.provider_stream_id.unwrap_or_default())
            }
            HashKey::Url => Value::Text(identity.url),
        };
        parts.push((key.as_str(), value));
    }

    // A provider stream id is only unique within its account, so substituting it
    // for the URL without the account id would collide across two XC providers.
    if use_stream_id && !keys.contains(&HashKey::M3uId) {
        parts.push((
            HashKey::M3uId.as_str(),
            Value::Number(identity.m3u_account_id),
        ));
    }

    // `json.dumps(..., sort_keys=True)`.
    parts.sort_by_key(|(key, _)| *key);

    let mut json = String::from("{");
    for (index, (key, value)) in parts.iter().enumerate() {
        if index > 0 {
            json.push_str(", ");
        }
        write_json_string(&mut json, key);
        json.push_str(": ");
        match value {
            Value::Text(text) => write_json_string(&mut json, text),
            Value::Number(number) => json.push_str(&number.to_string()),
        }
    }
    json.push('}');

    format!("{:x}", Sha256::digest(json.as_bytes()))
}

/// Python's `json.dumps` string encoding, defaults included.
///
/// `ensure_ascii` defaults to true, so every character outside printable ASCII
/// becomes a `\uXXXX` escape — an accented channel name hashes differently under
/// a naive encoder, and that difference is invisible until a refresh loses it.
fn write_json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' '..='~' => out.push(c),
            _ => {
                let mut buffer = [0u16; 2];
                for unit in c.encode_utf16(&mut buffer) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity<'a>(name: &'a str, url: &'a str) -> StreamIdentity<'a> {
        StreamIdentity {
            name,
            url,
            tvg_id: "",
            group: "",
            m3u_account_id: 2,
            account_type: M3uAccountType::Standard,
            provider_stream_id: None,
        }
    }

    /// `(label, account type, provider id, keys, expected JSON)`.
    type SubstitutionCase<'a> = (&'a str, M3uAccountType, Option<i64>, &'a [HashKey], &'a str);

    /// `sha256(json.dumps(parts, sort_keys=True).encode()).hexdigest()`, which
    /// is the whole of the algorithm.
    fn python_hash(json: &str) -> String {
        format!("{:x}", Sha256::digest(json.as_bytes()))
    }

    #[test]
    fn the_setting_parses_into_a_sorted_deduplicated_key_set() {
        let cases: &[(&str, &[HashKey])] = &[
            ("url", &[HashKey::Url]),
            ("name,url", &[HashKey::Name, HashKey::Url]),
            // Order in the setting is irrelevant: the object is sorted anyway.
            ("url,name", &[HashKey::Name, HashKey::Url]),
            (" url , name ", &[HashKey::Name, HashKey::Url]),
            ("url,url", &[HashKey::Url]),
            // A typo narrows the key rather than failing the refresh.
            ("url,nonsense", &[HashKey::Url]),
            ("nonsense", &[]),
            ("", &[]),
            (
                "group,m3u_id,name,tvg_id,url",
                &[
                    HashKey::Group,
                    HashKey::M3uId,
                    HashKey::Name,
                    HashKey::TvgId,
                    HashKey::Url,
                ],
            ),
        ];
        for (setting, want) in cases {
            assert_eq!(parse_keys(setting), *want, "{setting}");
        }
    }

    #[test]
    fn the_serialization_is_pythons_json_dumps() {
        // Spaces after `:` and `,` are `json.dumps` defaults, and they are part
        // of the bytes that get hashed.
        let keys = parse_keys("url");
        assert_eq!(
            stream_hash(&identity("A", "http://x/1"), &keys),
            python_hash(r#"{"url": "http://x/1"}"#)
        );

        let keys = parse_keys("name,url");
        assert_eq!(
            stream_hash(&identity("A", "http://x/1"), &keys),
            python_hash(r#"{"name": "A", "url": "http://x/1"}"#)
        );
    }

    #[test]
    fn keys_are_sorted_alphabetically_not_in_setting_order() {
        let keys = parse_keys("url,tvg_id,name,m3u_id,group");
        let subject = StreamIdentity {
            tvg_id: "a.us",
            group: "News",
            ..identity("A", "http://x/1")
        };
        assert_eq!(
            stream_hash(&subject, &keys),
            python_hash(
                r#"{"group": "News", "m3u_id": 2, "name": "A", "tvg_id": "a.us", "url": "http://x/1"}"#
            )
        );
    }

    #[test]
    fn the_account_id_is_a_number_not_a_string() {
        let keys = parse_keys("m3u_id");
        assert_eq!(
            stream_hash(&identity("A", "http://x/1"), &keys),
            python_hash(r#"{"m3u_id": 2}"#)
        );
    }

    #[test]
    fn non_ascii_names_are_escaped_the_way_python_escapes_them() {
        // The failure this guards against is silent and total: an accented
        // channel name hashed by a naive encoder matches nothing on refresh.
        let keys = parse_keys("name");
        let cases: &[(&str, &str)] = &[
            ("Caf\u{e9}", r#"{"name": "Caf\u00e9"}"#),
            ("\u{4e2d}\u{6587}", r#"{"name": "\u4e2d\u6587"}"#),
            // Outside the BMP, Python emits a surrogate pair.
            ("\u{1f600}", r#"{"name": "\ud83d\ude00"}"#),
            ("say \"hi\"", r#"{"name": "say \"hi\""}"#),
            ("back\\slash", r#"{"name": "back\\slash"}"#),
            ("tab\there", r#"{"name": "tab\there"}"#),
            ("nl\nhere", r#"{"name": "nl\nhere"}"#),
            ("cr\rhere", r#"{"name": "cr\rhere"}"#),
            ("bs\u{8}here", r#"{"name": "bs\bhere"}"#),
            ("ff\u{c}here", r#"{"name": "ff\fhere"}"#),
            ("ctl\u{1}here", r#"{"name": "ctl\u0001here"}"#),
            // DEL is outside the printable range Python leaves alone.
            ("del\u{7f}here", r#"{"name": "del\u007fhere"}"#),
            // A forward slash is *not* escaped, unlike some encoders.
            ("a/b", r#"{"name": "a/b"}"#),
        ];
        for (name, json) in cases {
            assert_eq!(
                stream_hash(&identity(name, "http://x/1"), &keys),
                python_hash(json),
                "{name:?}"
            );
        }
    }

    #[test]
    fn an_xtream_account_hashes_the_provider_id_instead_of_the_url() {
        // Xtream providers rotate the credentials inside stream URLs. Hashing
        // the URL would orphan the entire catalogue every time they do.
        let keys = parse_keys("url");
        let subject = StreamIdentity {
            account_type: M3uAccountType::XtreamCodes,
            provider_stream_id: Some(800008033),
            ..identity("A", "http://x/live/user/pass/800008033")
        };
        assert_eq!(
            stream_hash(&subject, &keys),
            // m3u_id is added because a provider id is only unique per account.
            python_hash(r#"{"m3u_id": 2, "url": 800008033}"#)
        );

        // The same stream after a credential rotation hashes identically.
        let rotated = StreamIdentity {
            url: "http://x/live/newuser/newpass/800008033",
            ..subject
        };
        assert_eq!(
            stream_hash(&rotated, &keys),
            stream_hash(&subject, &keys),
            "a credential rotation must not orphan the stream"
        );
    }

    #[test]
    fn the_account_id_is_not_added_twice_when_already_selected() {
        let keys = parse_keys("m3u_id,url");
        let subject = StreamIdentity {
            account_type: M3uAccountType::XtreamCodes,
            provider_stream_id: Some(7),
            ..identity("A", "http://x/1")
        };
        assert_eq!(
            stream_hash(&subject, &keys),
            python_hash(r#"{"m3u_id": 2, "url": 7}"#)
        );
    }

    #[test]
    fn the_provider_id_substitution_needs_all_three_conditions() {
        let with_url = parse_keys("url");
        let without_url = parse_keys("name");
        let url = "http://x/live/u/p/9";

        let cases: &[SubstitutionCase<'_>] = &[
            // Not an Xtream account: the URL is used as written.
            (
                "standard account",
                M3uAccountType::Standard,
                Some(9),
                &with_url,
                r#"{"url": "http://x/live/u/p/9"}"#,
            ),
            // No provider id to substitute.
            (
                "no provider id",
                M3uAccountType::XtreamCodes,
                None,
                &with_url,
                r#"{"url": "http://x/live/u/p/9"}"#,
            ),
            // Zero is falsy in the Python the format comes from, so it is not
            // substituted.
            (
                "provider id zero",
                M3uAccountType::XtreamCodes,
                Some(0),
                &with_url,
                r#"{"url": "http://x/live/u/p/9"}"#,
            ),
            // The URL is not part of the key at all.
            (
                "url not selected",
                M3uAccountType::XtreamCodes,
                Some(9),
                &without_url,
                r#"{"name": "A"}"#,
            ),
        ];
        for (label, account_type, provider_stream_id, keys, json) in cases {
            let subject = StreamIdentity {
                account_type: *account_type,
                provider_stream_id: *provider_stream_id,
                ..identity("A", url)
            };
            assert_eq!(stream_hash(&subject, keys), python_hash(json), "{label}");
        }
    }

    #[test]
    fn an_empty_key_set_still_produces_a_stable_hash() {
        // Degenerate, but it must not panic: every stream in the account would
        // collide onto one row, which the caller can at least observe.
        assert_eq!(
            stream_hash(&identity("A", "http://x/1"), &[]),
            python_hash("{}")
        );
    }

    #[test]
    fn changing_a_selected_field_changes_the_hash() {
        let keys = parse_keys("name,url");
        let base = stream_hash(&identity("A", "http://x/1"), &keys);
        assert_ne!(base, stream_hash(&identity("B", "http://x/1"), &keys));
        assert_ne!(base, stream_hash(&identity("A", "http://x/2"), &keys));

        // And a field outside the key set does not.
        let other_group = StreamIdentity {
            group: "Different",
            tvg_id: "different",
            ..identity("A", "http://x/1")
        };
        assert_eq!(base, stream_hash(&other_group, &keys));
    }
}
