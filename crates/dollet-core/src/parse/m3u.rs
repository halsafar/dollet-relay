//! M3U / EXTINF parser.
//!
//! Provider playlists are not a format so much as a convention, and every
//! provider bends it differently: attributes in any order, single or double
//! quotes, unquoted values, `#EXTGRP` instead of `group-title`, a BOM, CRLF, a
//! missing final newline, Latin-1 bytes in channel names. Nothing here rejects
//! a playlist; a malformed entry is dropped and the rest is kept, because one
//! bad line must not cost the user their other 60 channels.

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::Result;

use super::compress;

/// A parsed playlist. Duplicate entries are preserved as-is: deduplication is
/// a function of the account's hash key, which lives in the ingest layer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct M3uPlaylist {
    /// Attributes on the `#EXTM3U` line, most usefully `x-tvg-url`.
    pub header: BTreeMap<String, String>,
    pub entries: Vec<M3uEntry>,
    /// `#EXTINF` blocks discarded because no URL followed them.
    pub entries_without_url: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct M3uEntry {
    /// Display title: the text after the comma, falling back the way the
    /// base EXTINF spec implies (see [`extinf_name`]).
    pub name: String,
    /// The text after the comma, verbatim and possibly empty.
    pub display_name: String,
    pub url: String,
    /// `-1` for live streams; `None` when the provider omitted it entirely.
    pub duration: Option<f64>,
    /// Attribute keys are lowercased so lookups need no case folding.
    pub attributes: BTreeMap<String, String>,
    /// `#EXTVLCOPT:` payloads, minus the prefix, in file order.
    pub vlc_opts: Vec<String>,
    /// `#KODIPROP:` payloads, minus the prefix, in file order.
    pub kodi_props: Vec<String>,
}

/// The `#EXTINF:` line alone, before any following directive or URL.
#[derive(Debug, Clone, PartialEq)]
pub struct Extinf {
    pub duration: Option<f64>,
    pub display_name: String,
    pub attributes: BTreeMap<String, String>,
}

/// Schemes accepted as a stream URL. A line that is neither a
/// directive nor one of these is a stray, and discarding it keeps the pending
/// `#EXTINF` alive so the real URL on the next line still binds to it.
const URL_SCHEMES: &[&str] = &["http", "rtsp", "rtp", "udp"];

pub fn parse(input: &[u8]) -> Result<M3uPlaylist> {
    let decoded = compress::decompress(input)?;
    Ok(parse_str(&String::from_utf8_lossy(&decoded)))
}

/// Providers that mislabel Latin-1 as UTF-8 are common enough that lossy
/// decoding is the only option that does not throw away the whole playlist over
/// one accented channel name.
pub fn parse_str(text: &str) -> M3uPlaylist {
    let mut playlist = M3uPlaylist::default();
    let mut pending: Option<PendingEntry> = None;

    for raw in text.lines() {
        let line = raw.trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXTM3U") {
            playlist.header = parse_attributes(rest).0;
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            if pending.take().is_some() {
                playlist.entries_without_url += 1;
            }
            pending = Some(PendingEntry::new(parse_extinf_body(rest)));
        } else if let Some(rest) = line.strip_prefix("#EXTGRP:") {
            // An explicit `group-title` attribute wins; `#EXTGRP` only fills a gap.
            if let Some(p) = pending.as_mut() {
                p.extinf
                    .attributes
                    .entry("group-title".into())
                    .or_insert_with(|| rest.trim().to_string());
            }
        } else if let Some(rest) = line.strip_prefix("#EXTVLCOPT:") {
            if let Some(p) = pending.as_mut() {
                p.vlc_opts.push(rest.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("#KODIPROP:") {
            if let Some(p) = pending.as_mut() {
                p.kodi_props.push(rest.to_string());
            }
        } else if line.starts_with('#') {
            // Unknown directive: skip it without disturbing the pending entry.
        } else if is_stream_url(line)
            && let Some(p) = pending.take()
        {
            playlist.entries.push(p.finish(line));
        }
    }

    if pending.is_some() {
        playlist.entries_without_url += 1;
    }
    playlist
}

/// Parse one `#EXTINF:` line. Returns `None` for any other line.
pub fn parse_extinf(line: &str) -> Option<Extinf> {
    line.trim_start_matches('\u{feff}')
        .trim()
        .strip_prefix("#EXTINF:")
        .map(parse_extinf_body)
}

/// ffmpeg does not understand VLC's `udp://@` multicast syntax, where the `@`
/// means "listen on all interfaces". Strip it before the URL reaches a process.
pub fn normalize_stream_url(url: &str) -> Cow<'_, str> {
    match url.strip_prefix("udp://@") {
        Some(rest) => Cow::Owned(format!("udp://{rest}")),
        None => Cow::Borrowed(url),
    }
}

/// The inverse of [`normalize_stream_url`], for playlists we hand back out:
/// VLC and its derivatives need the `@` to join the multicast group.
pub fn restore_vlc_multicast_url(url: &str) -> Cow<'_, str> {
    let Some(rest) = url.strip_prefix("udp://") else {
        return Cow::Borrowed(url);
    };
    if rest.starts_with('@') || !authority_is_multicast(rest) {
        return Cow::Borrowed(url);
    }
    Cow::Owned(format!("udp://@{rest}"))
}

/// `url::Url` only recognises IP literals for its special schemes, so a
/// `udp://` host comes back as an opaque domain. Split the authority by hand.
fn authority_is_multicast(rest: &str) -> bool {
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host_port.rsplit_once(':').map_or(host_port, |(h, _)| h),
    };
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_multicast())
}

impl M3uEntry {
    /// Case-insensitive attribute lookup.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attributes
            .get(&key.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn tvg_id(&self) -> Option<&str> {
        self.attr("tvg-id").filter(|v| !v.is_empty())
    }

    pub fn tvg_logo(&self) -> Option<&str> {
        self.attr("tvg-logo").filter(|v| !v.is_empty())
    }

    pub fn group_title(&self) -> Option<&str> {
        self.attr("group-title").filter(|v| !v.is_empty())
    }

    /// `tvg-chno`, falling back to `channel-number`, which some providers use
    /// for the same thing.
    pub fn tvg_chno(&self) -> Option<f64> {
        self.attr("tvg-chno")
            .or_else(|| self.attr("channel-number"))
            .and_then(|v| v.trim().parse().ok())
    }

    pub fn catchup(&self) -> Option<&str> {
        self.attr("catchup").filter(|v| !v.is_empty())
    }

    pub fn catchup_source(&self) -> Option<&str> {
        self.attr("catchup-source").filter(|v| !v.is_empty())
    }

    /// `catchup-days` is the M3U spelling, `tv_archive_duration` the Xtream one.
    pub fn catchup_days(&self) -> u32 {
        self.attr("catchup-days")
            .or_else(|| self.attr("tv_archive_duration"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn is_catchup(&self) -> bool {
        let flagged = self.catchup().is_some_and(|v| v != "0");
        let archived = matches!(self.attr("tv_archive"), Some("1") | Some("True"));
        flagged || archived
    }

    /// Providers send `is_adult` as `1` or `"1"`; anything else is not adult.
    pub fn is_adult(&self) -> bool {
        self.attr("is_adult").is_some_and(|v| v.trim() == "1")
    }
}

struct PendingEntry {
    extinf: Extinf,
    vlc_opts: Vec<String>,
    kodi_props: Vec<String>,
}

impl PendingEntry {
    fn new(extinf: Extinf) -> Self {
        Self {
            extinf,
            vlc_opts: Vec::new(),
            kodi_props: Vec::new(),
        }
    }

    fn finish(self, url: &str) -> M3uEntry {
        M3uEntry {
            name: extinf_name(&self.extinf),
            display_name: self.extinf.display_name,
            url: normalize_stream_url(url).into_owned(),
            duration: self.extinf.duration,
            attributes: self.extinf.attributes,
            vlc_opts: self.vlc_opts,
            kodi_props: self.kodi_props,
        }
    }
}

/// Per the base EXTINF spec the comma text is the canonical human-readable
/// title. `tvg-name` is only a fallback because some providers put an EPG key
/// there rather than a label.
fn extinf_name(extinf: &Extinf) -> String {
    let pick = |k: &str| extinf.attributes.get(k).filter(|v| !v.is_empty()).cloned();
    if !extinf.display_name.is_empty() {
        return extinf.display_name.clone();
    }
    pick("tvc-guide-title")
        .or_else(|| pick("tvg-name"))
        .unwrap_or_default()
}

fn is_stream_url(line: &str) -> bool {
    URL_SCHEMES.iter().any(|s| line.starts_with(s))
}

fn parse_extinf_body(body: &str) -> Extinf {
    let body = body.trim_start();

    // The duration is the first token, and it must be consumed before attribute
    // scanning or an unquoted value would swallow it.
    let split = body
        .find(|c: char| c.is_whitespace() || c == ',')
        .unwrap_or(body.len());
    let (duration, rest) = match body[..split].parse::<f64>() {
        Ok(d) => (Some(d), &body[split..]),
        Err(_) => (None, body),
    };

    let (attributes, display_name) = parse_attributes(rest);
    Extinf {
        duration,
        display_name,
        attributes,
    }
}

/// Scan `key=value` pairs and return them with the display name that follows.
///
/// The comma is not a reliable terminator. An unquoted value may contain one —
/// `group-title=News,Sports tvg-id="a",Channel` is real — so stopping at the
/// first comma would lose every attribute after it *and* take the wrong text as
/// the name. Scanning continues past the comma, but only quoted pairs are
/// accepted there, because after the comma everything is a display title until
/// proven otherwise. The name then starts after the last attribute, or after
/// the first comma if that comes later.
fn parse_attributes(input: &str) -> (BTreeMap<String, String>, String) {
    let bytes = input.as_bytes();
    let mut attributes = BTreeMap::new();
    let mut i = 0;
    let mut first_comma = None;
    let mut last_attr_end = 0;

    while i < bytes.len() {
        let c = bytes[i];
        if c == b',' {
            first_comma.get_or_insert(i);
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        let key_start = i;
        while i < bytes.len() && !matches!(bytes[i], b'=' | b',') && !bytes[i].is_ascii_whitespace()
        {
            i += 1;
        }
        let key_end = i;

        // `key = "value"` with spaces around the `=` is rare but real.
        let mut eq = i;
        while eq < bytes.len() && bytes[eq].is_ascii_whitespace() {
            eq += 1;
        }
        if eq >= bytes.len() || bytes[eq] != b'=' {
            // A bare token where a `key=` was expected: skip it rather than
            // letting one typo abort the remaining attributes.
            continue;
        }
        let key = input[key_start..key_end].to_ascii_lowercase();
        i = eq + 1;

        // Only cross whitespace after the `=` when a quote follows it, so an
        // unquoted empty value does not swallow the next attribute.
        let mut quote = i;
        while quote < bytes.len() && bytes[quote].is_ascii_whitespace() {
            quote += 1;
        }
        if matches!(bytes.get(quote), Some(b'"' | b'\'')) {
            i = quote;
        }

        let value = match bytes.get(i) {
            Some(&q @ (b'"' | b'\'')) => {
                i += 1;
                let start = i;
                match input[start..].find(q as char) {
                    Some(off) => {
                        let end = start + off;
                        i = end + 1;
                        &input[start..end]
                    }
                    // Unterminated quote. Ending the value at the next comma
                    // recovers the display name, which matters far more than
                    // the truncated attribute does.
                    None => {
                        let end = input[start..].find(',').map_or(input.len(), |o| start + o);
                        i = end;
                        &input[start..end]
                    }
                }
            }
            // Unquoted values are only credible before the comma; past it, a
            // bare `word=word` is far more likely part of a title.
            _ if first_comma.is_some() => continue,
            _ => {
                let start = i;
                while i < bytes.len() && bytes[i] != b',' && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                &input[start..i]
            }
        };

        if !key.is_empty() {
            attributes.insert(key, value.to_string());
            last_attr_end = i;
        }
    }

    let name_start = match first_comma {
        Some(comma) => last_attr_end.max(comma + 1),
        None => return (attributes, String::new()),
    };
    let name = input[name_start..].trim_start();
    (
        attributes,
        name.strip_prefix(',').unwrap_or(name).trim().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// `(line, expected attributes, expected display name)`.
    type AttrCase<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);

    fn attrs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_a_canonical_playlist() {
        let text = "#EXTM3U x-tvg-url=\"http://e.example/xmltv\"\n\
                    #EXTINF:-1 tvg-id=\"a.us\" tvg-name=\"A\" tvg-logo=\"http://l/a.png\" \
                    tvg-chno=\"101\" group-title=\"News\",Channel A\n\
                    http://p.example/a.ts\n";
        let pl = parse_str(text);

        assert_eq!(pl.header, attrs(&[("x-tvg-url", "http://e.example/xmltv")]));
        assert_eq!(pl.entries.len(), 1);
        let e = &pl.entries[0];
        assert_eq!(e.name, "Channel A");
        assert_eq!(e.display_name, "Channel A");
        assert_eq!(e.url, "http://p.example/a.ts");
        assert_eq!(e.duration, Some(-1.0));
        assert_eq!(e.tvg_id(), Some("a.us"));
        assert_eq!(e.attr("tvg-name"), Some("A"));
        assert_eq!(e.tvg_logo(), Some("http://l/a.png"));
        assert_eq!(e.tvg_chno(), Some(101.0));
        assert_eq!(e.group_title(), Some("News"));
        assert_eq!(pl.entries_without_url, 0);
    }

    #[test]
    fn attribute_quoting_and_ordering_variants() {
        let cases: &[AttrCase<'_>] = &[
            (
                "#EXTINF:-1 group-title=\"Sports\" tvg-id=\"b\",B",
                &[("group-title", "Sports"), ("tvg-id", "b")],
                "B",
            ),
            (
                "#EXTINF:-1 tvg-id='c' tvg-name='C',C",
                &[("tvg-id", "c"), ("tvg-name", "C")],
                "C",
            ),
            (
                "#EXTINF:-1 tvg-id=d tvg-chno=7,D",
                &[("tvg-id", "d"), ("tvg-chno", "7")],
                "D",
            ),
            (
                "#EXTINF:-1 TVG-ID=\"e\" Group-Title=\"G\",E",
                &[("group-title", "G"), ("tvg-id", "e")],
                "E",
            ),
            ("#EXTINF:-1  tvg-id = \"f\" ,F", &[("tvg-id", "f")], "F"),
            ("#EXTINF:-1,Plain Name", &[], "Plain Name"),
            (
                "#EXTINF:-1 tvg-id=\"g\",Name, With, Commas",
                &[("tvg-id", "g")],
                "Name, With, Commas",
            ),
            ("#EXTINF:0", &[], ""),
            ("#EXTINF:-1 tvg-id=\"\",H", &[("tvg-id", "")], "H"),
            // A value containing an `=` and a space, correctly quoted.
            (
                "#EXTINF:-1 catchup-source=\"http://x/?u=1&t=2\",I",
                &[("catchup-source", "http://x/?u=1&t=2")],
                "I",
            ),
            // Bare token where `key=` was expected.
            ("#EXTINF:-1 garbage tvg-id=\"j\",J", &[("tvg-id", "j")], "J"),
            // Unterminated quote: attribute truncates, display name survives.
            ("#EXTINF:-1 tvg-id=\"k,K", &[("tvg-id", "k")], "K"),
            // Unterminated quote with no comma at all.
            ("#EXTINF:-1 tvg-id=\"k", &[("tvg-id", "k")], ""),
            // `=` with nothing after it.
            ("#EXTINF:-1 tvg-id=,L", &[("tvg-id", "")], "L"),
            // `=` with nothing before it.
            (
                r#"#EXTINF:-1 ="orphan" tvg-id="m",M"#,
                &[("tvg-id", "m")],
                "M",
            ),
            // An unquoted value containing a comma: scanning must continue past
            // it or both the later attribute and the name are lost.
            (
                r#"#EXTINF:-1 group-title=News,Sports tvg-id="a",Channel Name"#,
                &[("group-title", "News"), ("tvg-id", "a")],
                "Channel Name",
            ),
            // Past the comma a bare `word=word` is title text, not an attribute.
            (
                r#"#EXTINF:-1 tvg-id="a",Live: Home=Away"#,
                &[("tvg-id", "a")],
                "Live: Home=Away",
            ),
            // Quotes in a name that are not part of a `key="value"` survive.
            (
                r#"#EXTINF:-1 tvg-id="a",The "Best" Channel"#,
                &[("tvg-id", "a")],
                r#"The "Best" Channel"#,
            ),
        ];

        let parsed: Vec<Extinf> = cases
            .iter()
            .filter_map(|(line, ..)| parse_extinf(line))
            .collect();
        assert_eq!(parsed.len(), cases.len(), "every case is an #EXTINF line");
        for (got, (line, want_attrs, want_name)) in parsed.iter().zip(cases) {
            assert_eq!(got.attributes, attrs(want_attrs), "attrs for {line}");
            assert_eq!(got.display_name, *want_name, "name for {line}");
        }
    }

    #[test]
    fn extinf_duration_variants() {
        let cases: &[(&str, Option<f64>)] = &[
            ("#EXTINF:-1,A", Some(-1.0)),
            ("#EXTINF:-1.0 tvg-id=\"a\",A", Some(-1.0)),
            ("#EXTINF:0,A", Some(0.0)),
            ("#EXTINF:3600,A", Some(3600.0)),
            ("#EXTINF: 120 ,A", Some(120.0)),
            ("#EXTINF:tvg-id=\"a\",A", None),
        ];
        for (line, want) in cases {
            assert_eq!(parse_extinf(line).unwrap().duration, *want, "{line}");
        }
    }

    #[test]
    fn parse_extinf_rejects_other_lines() {
        assert!(parse_extinf("http://x/a.ts").is_none());
        assert!(parse_extinf("#EXTGRP:News").is_none());
        assert!(parse_extinf("#EXTINF").is_none());
    }

    #[test]
    fn name_falls_back_past_an_empty_comma_text() {
        let cases: &[(&str, &str)] = &[
            ("#EXTINF:-1 tvg-name=\"From tvg-name\",", "From tvg-name"),
            (
                "#EXTINF:-1 tvc-guide-title=\"Guide\" tvg-name=\"Ignored\",",
                "Guide",
            ),
            ("#EXTINF:-1 tvg-id=\"x\",", ""),
        ];
        for (line, want) in cases {
            let pl = parse_str(&format!("{line}\nhttp://x/a.ts\n"));
            assert_eq!(pl.entries[0].name, *want, "{line}");
        }
    }

    #[test]
    fn extgrp_only_fills_a_missing_group_title() {
        let with_attr =
            parse_str("#EXTINF:-1 group-title=\"Attr\",A\n#EXTGRP:Directive\nhttp://x/a.ts\n");
        assert_eq!(with_attr.entries[0].group_title(), Some("Attr"));

        let without_attr = parse_str("#EXTINF:-1,A\n#EXTGRP: Directive \nhttp://x/a.ts\n");
        assert_eq!(without_attr.entries[0].group_title(), Some("Directive"));
    }

    #[test]
    fn collects_vlcopt_and_kodiprop() {
        let pl = parse_str(
            "#EXTINF:-1,A\n\
             #EXTVLCOPT:http-user-agent=VLC/3\n\
             #KODIPROP:inputstream=inputstream.adaptive\n\
             #EXTVLCOPT:network-caching=1000\n\
             #SOMETHINGELSE:ignored\n\
             http://x/a.ts\n",
        );
        let e = &pl.entries[0];
        assert_eq!(
            e.vlc_opts,
            ["http-user-agent=VLC/3", "network-caching=1000"]
        );
        assert_eq!(e.kodi_props, ["inputstream=inputstream.adaptive"]);
    }

    #[test]
    fn directives_outside_an_entry_are_ignored() {
        let pl = parse_str("#EXTGRP:Orphan\n#EXTVLCOPT:x=1\n#KODIPROP:y=2\nhttp://x/a.ts\n");
        assert!(pl.entries.is_empty());
        assert_eq!(pl.entries_without_url, 0);
    }

    #[test]
    fn entries_without_a_url_are_counted_and_dropped() {
        let pl =
            parse_str("#EXTINF:-1,Orphan\n#EXTINF:-1,Real\nhttp://x/a.ts\n#EXTINF:-1,Trailing\n");
        assert_eq!(pl.entries.len(), 1);
        assert_eq!(pl.entries[0].name, "Real");
        assert_eq!(pl.entries_without_url, 2);
    }

    #[test]
    fn non_url_lines_do_not_consume_the_pending_entry() {
        let pl = parse_str("#EXTINF:-1,A\nnot-a-url\nhttp://x/a.ts\n");
        assert_eq!(pl.entries.len(), 1);
        assert_eq!(pl.entries[0].url, "http://x/a.ts");
    }

    #[test]
    fn accepts_bom_crlf_and_a_missing_final_newline() {
        let text = "\u{feff}#EXTM3U\r\n#EXTINF:-1 tvg-id=\"a\",A\r\nhttp://x/a.ts";
        let pl = parse_str(text);
        assert_eq!(pl.entries.len(), 1);
        assert_eq!(pl.entries[0].tvg_id(), Some("a"));
        assert_eq!(pl.entries[0].url, "http://x/a.ts");
    }

    #[test]
    fn keeps_duplicate_entries() {
        let one = "#EXTINF:-1 tvg-id=\"a\",A\nhttp://x/a.ts\n";
        let pl = parse_str(&one.repeat(3));
        assert_eq!(pl.entries.len(), 3);
    }

    #[test]
    fn accepts_every_supported_url_scheme() {
        for scheme in ["http", "https", "rtsp", "rtp", "udp"] {
            let pl = parse_str(&format!("#EXTINF:-1,A\n{scheme}://h/a\n"));
            assert_eq!(pl.entries.len(), 1, "{scheme}");
        }
        assert!(parse_str("#EXTINF:-1,A\nftp://h/a\n").entries.is_empty());
    }

    #[test]
    fn catchup_accessors() {
        let line = "#EXTINF:-1 catchup=\"default\" catchup-days=\"7\" \
                    catchup-source=\"http://x/{utc}\" is_adult=\"1\",A";
        let pl = parse_str(&format!("{line}\nhttp://x/a.ts\n"));
        let e = &pl.entries[0];
        assert!(e.is_catchup());
        assert_eq!(e.catchup(), Some("default"));
        assert_eq!(e.catchup_days(), 7);
        assert_eq!(e.catchup_source(), Some("http://x/{utc}"));
        assert!(e.is_adult());

        let xc = parse_str("#EXTINF:-1 tv_archive=\"1\" tv_archive_duration=\"3\",A\nhttp://x/a\n");
        assert!(xc.entries[0].is_catchup());
        assert_eq!(xc.entries[0].catchup_days(), 3);

        let off =
            parse_str("#EXTINF:-1 catchup=\"0\" tv_archive=\"0\" is_adult=\"0\",A\nhttp://x/a\n");
        assert!(!off.entries[0].is_catchup());
        assert_eq!(off.entries[0].catchup_days(), 0);
        assert_eq!(off.entries[0].catchup(), Some("0"));
        assert!(!off.entries[0].is_adult());
        assert!(off.entries[0].catchup_source().is_none());
    }

    #[test]
    fn numeric_accessors_tolerate_junk() {
        let cases: &[(&str, Option<f64>)] = &[
            ("tvg-chno=\"12\"", Some(12.0)),
            ("channel-number=\"9\"", Some(9.0)),
            ("tvg-chno=\"12.5\"", Some(12.5)),
            ("tvg-chno=\"n/a\"", None),
            ("", None),
        ];
        for (attr_text, want_chno) in cases {
            let pl = parse_str(&format!("#EXTINF:-1 {attr_text},A\nhttp://x/a\n"));
            assert_eq!(pl.entries[0].tvg_chno(), *want_chno, "{attr_text}");
        }
        // tvg-chno wins over channel-number when both are present.
        let pl = parse_str("#EXTINF:-1 tvg-chno=\"1\" channel-number=\"2\",A\nhttp://x/a\n");
        assert_eq!(pl.entries[0].tvg_chno(), Some(1.0));
        assert_eq!(pl.entries[0].attr("TVG-CHNO"), Some("1"));
        assert_eq!(pl.entries[0].attr("missing"), None);
        assert_eq!(pl.entries[0].attr("tvg-name"), None);
        assert_eq!(pl.entries[0].tvg_logo(), None);
        assert_eq!(pl.entries[0].group_title(), None);
    }

    #[test]
    fn normalizes_vlc_udp_urls() {
        let cases: &[(&str, &str)] = &[
            ("udp://@239.0.0.1:1234", "udp://239.0.0.1:1234"),
            ("udp://239.0.0.1:1234", "udp://239.0.0.1:1234"),
            ("http://x/a.ts", "http://x/a.ts"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_stream_url(input), *want, "{input}");
        }
        let pl = parse_str("#EXTINF:-1,A\nudp://@239.0.0.1:1234\n");
        assert_eq!(pl.entries[0].url, "udp://239.0.0.1:1234");
    }

    #[test]
    fn restores_the_at_sign_only_for_multicast() {
        let cases: &[(&str, &str)] = &[
            ("udp://239.0.0.1:1234", "udp://@239.0.0.1:1234"),
            ("udp://[ff02::1]:1234", "udp://@[ff02::1]:1234"),
            ("udp://@239.0.0.1:1234", "udp://@239.0.0.1:1234"),
            ("udp://10.0.0.5:1234", "udp://10.0.0.5:1234"),
            ("udp://[2001:db8::1]:1234", "udp://[2001:db8::1]:1234"),
            ("udp://host.example:1234", "udp://host.example:1234"),
            ("udp://", "udp://"),
            ("http://x/a.ts", "http://x/a.ts"),
        ];
        for (input, want) in cases {
            assert_eq!(restore_vlc_multicast_url(input), *want, "{input}");
        }
    }

    #[test]
    fn parses_compressed_and_non_utf8_input() {
        let text = "#EXTINF:-1 tvg-id=\"a\",Caf\u{e9} TV\nhttp://x/a.ts\n";

        let plain = parse(text.as_bytes()).unwrap();
        assert_eq!(plain.entries[0].name, "Caf\u{e9} TV");

        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(text.as_bytes()).unwrap();
        let gz = parse(&enc.finish().unwrap()).unwrap();
        assert_eq!(gz.entries[0].name, "Caf\u{e9} TV");

        // Latin-1 0xE9 where UTF-8 was declared: the entry survives, lossily.
        let latin1 = b"#EXTINF:-1,Caf\xe9 TV\nhttp://x/a.ts\n";
        let lossy = parse(latin1).unwrap();
        assert_eq!(lossy.entries[0].name, "Caf\u{fffd} TV");
    }

    #[test]
    fn empty_and_truncated_documents() {
        assert_eq!(parse_str(""), M3uPlaylist::default());
        assert_eq!(parse_str("\n\n   \n"), M3uPlaylist::default());
        assert_eq!(parse_str("#EXTM3U\n"), M3uPlaylist::default());
        assert_eq!(parse(b"").unwrap(), M3uPlaylist::default());

        let truncated = parse_str("#EXTM3U\n#EXTINF:-1 tvg-id=\"a\" tvg-nam");
        assert_eq!(truncated.entries_without_url, 1);
        assert_eq!(truncated.entries.len(), 0);
    }

    #[test]
    fn header_attributes_are_parsed() {
        let pl = parse_str("#EXTM3U url-tvg=\"http://e/x.xml\" x-tvg-url=\"http://e/x.xml\"\n");
        assert_eq!(
            pl.header,
            attrs(&[
                ("url-tvg", "http://e/x.xml"),
                ("x-tvg-url", "http://e/x.xml")
            ])
        );
        assert!(parse_str("#EXTM3U\n").header.is_empty());
    }

    #[test]
    fn compressed_input_errors_propagate() {
        assert!(parse(&[0x1f, 0x8b, 0x08, 0x00]).is_err());
    }
}
