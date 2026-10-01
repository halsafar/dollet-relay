//! M3U playlist generation for `/output/m3u` and the Xtream `get.php` flavour.
//!
//! The caller supplies channels already coalesced and ordered; nothing here
//! sorts, filters, or queries. Logo URLs arrive resolved because the choice
//! between the artwork cache and the provider's own URL depends on request
//! parameters the serializer never sees.

use std::borrow::Cow;
use std::fmt::Write as _;

use crate::domain::EffectiveChannel;
use crate::parse::m3u::restore_vlc_multicast_url;

use super::{TvgIdSource, format_channel_number};

/// Group shown for a channel that belongs to none.
const DEFAULT_GROUP: &str = "Default";

pub struct M3uOptions<'a> {
    /// Origin the client reached us on, with no trailing slash.
    pub base_url: &'a str,
    /// Value for `x-tvg-url` and `url-tvg`.
    pub epg_url: &'a str,
    pub tvg_id_source: TvgIdSource,
    /// Appended to every proxy stream URL: `output_profile`, `output_format`.
    pub stream_query: &'a [(&'a str, String)],
    /// Set for `get.php`, which addresses channels by row id under the caller's
    /// credentials rather than by proxy UUID.
    pub xtream_credentials: Option<(&'a str, &'a str)>,
}

pub struct M3uChannel<'a> {
    pub channel: &'a EffectiveChannel,
    /// Provider URL, for `?direct=true`. `None` yields the proxy URL, which is
    /// what every normal request wants.
    pub direct_url: Option<&'a str>,
}

impl<'a> M3uChannel<'a> {
    pub fn proxied(channel: &'a EffectiveChannel) -> Self {
        Self {
            channel,
            direct_url: None,
        }
    }
}

/// Channels flagged `hidden_from_output` are dropped here rather than trusted
/// to have been filtered already. The query excludes them too, so a caller
/// rewriting that query is the likely source of a leak, and a hidden
/// channel appearing in a playlist is the one failure a user cannot undo.
pub fn render(opts: &M3uOptions<'_>, channels: &[M3uChannel<'_>]) -> String {
    let mut out = String::with_capacity(128 + channels.len() * 256);
    let _ = writeln!(
        out,
        "#EXTM3U x-tvg-url=\"{0}\" url-tvg=\"{0}\"",
        attr(opts.epg_url)
    );

    let query = encode_query(opts.stream_query);
    for entry in channels.iter().filter(|e| !e.channel.hidden_from_output) {
        write_entry(&mut out, opts, &query, entry);
    }
    out
}

fn write_entry(out: &mut String, opts: &M3uOptions<'_>, query: &str, entry: &M3uChannel<'_>) {
    let ch = entry.channel;
    let number = format_channel_number(ch.channel_number).unwrap_or_default();
    let tvg_id = opts
        .tvg_id_source
        .resolve(ch.tvg_id.as_deref(), ch.tvc_guide_stationid.as_deref())
        .unwrap_or_else(|| {
            // Channel number is both the default source and the fallback for a
            // channel the chosen source has nothing for.
            if number.is_empty() {
                ch.id.to_string()
            } else {
                number.clone()
            }
        });

    let _ = write!(
        out,
        "#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{}\" tvg-logo=\"{}\" tvg-chno=\"{}\" ",
        attr(&tvg_id),
        attr(&ch.name),
        attr(ch.logo_url.as_deref().unwrap_or_default()),
        attr(&number),
    );
    if let Some(station) = ch.tvc_guide_stationid.as_deref().filter(|s| !s.is_empty()) {
        let _ = write!(out, "tvc-guide-stationid=\"{}\" ", attr(station));
    }
    // No catch-up attributes: `catchup="default"`
    // would advertise a capability 1.0 does not implement — a client would then
    // build archive URLs this server has no handler for. `is_catchup` is
    // carried through the parser and the schema so the gap stays visible, not
    // so outputs can claim it works.
    let group = ch
        .group_name
        .as_deref()
        .filter(|g| !g.is_empty())
        .unwrap_or(DEFAULT_GROUP);
    let _ = writeln!(
        out,
        "group-title=\"{}\",{}",
        attr(group),
        single_line(&ch.name)
    );

    let _ = writeln!(out, "{}", single_line(&stream_url(opts, query, entry)));
}

fn stream_url(opts: &M3uOptions<'_>, query: &str, entry: &M3uChannel<'_>) -> String {
    let ch = entry.channel;
    if let Some((username, password)) = opts.xtream_credentials {
        return format!(
            "{}/live/{username}/{password}/{}{query}",
            opts.base_url, ch.id
        );
    }
    match entry.direct_url.filter(|u| !u.is_empty()) {
        Some(url) => restore_vlc_multicast_url(url).into_owned(),
        None => format!("{}/proxy/ts/stream/{}{query}", opts.base_url, ch.uuid),
    }
}

fn encode_query(params: &[(&str, String)]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
        .finish();
    format!("?{query}")
}

/// M3U is a line-oriented format with no escaping rule. Two classes of
/// character cannot survive being written raw:
///
/// - A `"` truncates every attribute after it in any client's parser.
/// - A newline ends the `#EXTINF` line early, so the remainder of a channel
///   name is reparsed as a *new entry*. Names come from provider `#EXTINF`
///   data, which makes that a forged playlist entry, not a cosmetic glitch.
///
/// Values containing neither are written as they are.
fn attr(value: &str) -> Cow<'_, str> {
    if !value.contains(['"', '\n', '\r', '\t']) {
        return Cow::Borrowed(value);
    }
    let mut out = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '\n' | '\r' | '\t' => out.push(' '),
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// The display title after the comma, and the URL on the line below it, have no
/// quoting at all — only a line break can break them, so only that is removed.
fn single_line(value: &str) -> Cow<'_, str> {
    if value.contains(['\n', '\r']) {
        Cow::Owned(value.replace(['\n', '\r'], " "))
    } else {
        Cow::Borrowed(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::UserLevel;
    use uuid::Uuid;

    fn uuid(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn channel(id: i64, name: &str, number: Option<f64>) -> EffectiveChannel {
        EffectiveChannel {
            id,
            uuid: uuid(id as u128),
            channel_number: number,
            name: name.into(),
            channel_group_id: Some(1),
            created_at: chrono::DateTime::UNIX_EPOCH,
            group_name: Some("News".into()),
            logo_url: Some(format!("http://ipx.test/api/channels/logos/{id}/cache/")),
            tvg_id: Some(format!("ch{id}.us")),
            tvc_guide_stationid: None,
            epg_data_id: None,
            stream_profile_id: None,
            user_level: UserLevel::Streamer,
            is_adult: false,
            hidden_from_output: false,
            is_catchup: false,
            catchup_days: 0,
        }
    }

    fn options<'a>() -> M3uOptions<'a> {
        M3uOptions {
            base_url: "http://ipx.test",
            epg_url: "http://ipx.test/output/epg",
            tvg_id_source: TvgIdSource::ChannelNumber,
            stream_query: &[],
            xtream_credentials: None,
        }
    }

    fn sample() -> Vec<EffectiveChannel> {
        let mut a = channel(1, "Channel A", Some(101.0));
        a.tvc_guide_stationid = Some("12345".into());
        let mut b = channel(2, "Channel B", Some(102.5));
        b.group_name = None;
        b.logo_url = None;
        let mut c = channel(3, "Channel & \"C\"", None);
        c.is_catchup = true;
        c.catchup_days = 7;
        c.tvg_id = None;
        vec![a, b, c]
    }

    fn render_sample(opts: &M3uOptions<'_>) -> String {
        let channels = sample();
        let entries: Vec<_> = channels.iter().map(M3uChannel::proxied).collect();
        render(opts, &entries)
    }

    #[test]
    fn playlist_by_channel_number() {
        insta::assert_snapshot!(render_sample(&options()));
    }

    #[test]
    fn playlist_by_tvg_id() {
        let opts = M3uOptions {
            tvg_id_source: TvgIdSource::TvgId,
            ..options()
        };
        insta::assert_snapshot!(render_sample(&opts));
    }

    #[test]
    fn playlist_by_gracenote_id() {
        let opts = M3uOptions {
            tvg_id_source: TvgIdSource::Gracenote,
            ..options()
        };
        insta::assert_snapshot!(render_sample(&opts));
    }

    #[test]
    fn playlist_with_stream_query_parameters() {
        let query = [
            ("output_profile", "3".to_string()),
            ("output_format", "mp4".to_string()),
        ];
        let opts = M3uOptions {
            stream_query: &query,
            ..options()
        };
        insta::assert_snapshot!(render_sample(&opts));
    }

    #[test]
    fn playlist_for_xtream_credentials() {
        let opts = M3uOptions {
            xtream_credentials: Some(("bob", "s3cret")),
            ..options()
        };
        insta::assert_snapshot!(render_sample(&opts));
    }

    #[test]
    fn direct_urls_bypass_the_proxy_and_restore_vlc_multicast() {
        let channels = sample();
        let entries = vec![
            M3uChannel {
                channel: &channels[0],
                direct_url: Some("http://p.example/a.ts"),
            },
            M3uChannel {
                channel: &channels[1],
                direct_url: Some("udp://239.0.0.1:1234"),
            },
            // Empty means the provider had no URL: fall back to the proxy.
            M3uChannel {
                channel: &channels[2],
                direct_url: Some(""),
            },
        ];
        insta::assert_snapshot!(render(&options(), &entries));
    }

    #[test]
    fn an_empty_playlist_is_still_a_valid_header() {
        assert_eq!(
            render(&options(), &[]),
            "#EXTM3U x-tvg-url=\"http://ipx.test/output/epg\" \
             url-tvg=\"http://ipx.test/output/epg\"\n"
        );
    }

    #[test]
    fn xtream_credentials_win_over_a_direct_url() {
        let channels = sample();
        let entries = vec![M3uChannel {
            channel: &channels[0],
            direct_url: Some("http://p.example/a.ts"),
        }];
        let opts = M3uOptions {
            xtream_credentials: Some(("bob", "pw")),
            ..options()
        };
        assert!(render(&opts, &entries).contains("http://ipx.test/live/bob/pw/1\n"));
    }

    #[test]
    fn quotes_in_names_are_escaped_and_nothing_else_is() {
        let mut ch = channel(1, "The \"Best\" & <only>", Some(1.0));
        ch.group_name = Some("A \"Group\"".into());
        let rendered = render(&options(), &[M3uChannel::proxied(&ch)]);
        assert!(
            rendered.contains("tvg-name=\"The &quot;Best&quot; & <only>\""),
            "{rendered}"
        );
        assert!(
            rendered.contains("group-title=\"A &quot;Group&quot;\""),
            "{rendered}"
        );
        // The comma text is the display title and carries no quoting rule.
        assert!(rendered.contains(",The \"Best\" & <only>\n"), "{rendered}");
    }

    #[test]
    fn a_numberless_channel_falls_back_to_its_row_id() {
        let ch = channel(42, "No Number", None);
        let rendered = render(&options(), &[M3uChannel::proxied(&ch)]);
        assert!(rendered.contains("tvg-id=\"42\""), "{rendered}");
        assert!(rendered.contains("tvg-chno=\"\""), "{rendered}");
    }

    #[test]
    fn a_newline_in_provider_data_cannot_forge_a_playlist_entry() {
        // Channel names arrive from provider `#EXTINF` data. Before this was
        // escaped, reparsing the output yielded a single entry named
        // "Injected" carrying none of the real channel's identity.
        let mut ch = channel(
            1,
            "Real\n#EXTINF:-1,Injected\nhttp://evil.example/x.ts",
            Some(1.0),
        );
        ch.group_name = Some("News\nGroup".into());
        ch.tvg_id = Some("a\tb".into());
        let entries = vec![M3uChannel {
            channel: &ch,
            direct_url: Some("http://p.example/a.ts\nhttp://evil.example/y.ts"),
        }];
        let opts = M3uOptions {
            tvg_id_source: TvgIdSource::TvgId,
            ..options()
        };
        let rendered = render(&opts, &entries);

        let parsed = crate::parse::m3u::parse_str(&rendered);
        assert_eq!(parsed.entries.len(), 1, "{rendered}");
        assert_eq!(parsed.entries[0].tvg_id(), Some("a b"));
        assert_eq!(parsed.entries[0].group_title(), Some("News Group"));
        assert_eq!(
            parsed.entries[0].url,
            "http://p.example/a.ts http://evil.example/y.ts"
        );
        // The forged text survives as inert content on one line; what must not
        // survive is a second `#EXTINF` or URL *line*.
        assert_eq!(rendered.lines().count(), 3, "{rendered}");
        assert_eq!(
            rendered
                .lines()
                .filter(|l| l.starts_with("#EXTINF"))
                .count(),
            1,
            "{rendered}"
        );
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn a_newline_in_the_epg_url_cannot_forge_a_header() {
        let opts = M3uOptions {
            epg_url: "http://ipx.test/epg\n#EXTINF:-1,Injected",
            ..options()
        };
        let rendered = render(&opts, &[]);
        assert_eq!(rendered.lines().count(), 1, "{rendered}");
    }

    #[test]
    fn channels_hidden_from_output_are_dropped() {
        let mut visible = channel(1, "Visible", Some(1.0));
        visible.hidden_from_output = false;
        let mut hidden = channel(2, "Hidden", Some(2.0));
        hidden.hidden_from_output = true;

        let entries = vec![M3uChannel::proxied(&visible), M3uChannel::proxied(&hidden)];
        let rendered = render(&options(), &entries);
        assert!(rendered.contains("Visible"), "{rendered}");
        assert!(!rendered.contains("Hidden"), "{rendered}");
    }

    #[test]
    fn the_generated_playlist_parses_back_to_the_same_channels() {
        let parsed = crate::parse::m3u::parse_str(&render_sample(&options()));
        assert_eq!(parsed.entries.len(), 3);
        assert_eq!(
            parsed.header.get("x-tvg-url").map(String::as_str),
            Some("http://ipx.test/output/epg")
        );
        assert_eq!(parsed.entries[0].name, "Channel A");
        assert_eq!(parsed.entries[0].tvg_chno(), Some(101.0));
        assert_eq!(parsed.entries[0].group_title(), Some("News"));
        assert_eq!(parsed.entries[1].group_title(), Some("Default"));
        // Catch-up is out of scope for 1.0, so nothing advertises it.
        assert!(!parsed.entries[2].is_catchup());
    }
}
