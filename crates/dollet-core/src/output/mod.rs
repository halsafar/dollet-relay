//! Output serializers.
//!
//! Everything here is `struct -> bytes`, taking [`crate::domain::EffectiveChannel`]
//! and friends. No database access: the caller supplies rows already coalesced.
//!
//! All are on the must-be-100% list, and all are verified by golden-file diffs
//! under `fixtures/golden/` rather than by coverage alone.

use std::borrow::Cow;

pub mod dummy_epg;
pub mod hdhr;
pub mod m3u;
pub mod xc;
pub mod xmltv;

/// Which field becomes the `tvg-id` / XMLTV `channel id` a client sees.
///
/// `Gracenote` means "use `tvc_guide_stationid`", which is what
/// Plex wants when its guide comes from Gracenote rather than from our XMLTV.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TvgIdSource {
    #[default]
    ChannelNumber,
    TvgId,
    Gracenote,
}

impl TvgIdSource {
    /// Anything unrecognised falls back to the default: a typo in a query
    /// parameter must not empty a client's guide.
    pub fn from_query(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "tvg_id" => Self::TvgId,
            "gracenote" => Self::Gracenote,
            _ => Self::ChannelNumber,
        }
    }

    /// The id for one channel under this source, or `None` when the channel has
    /// nothing usable and the caller should fall back to its row id.
    pub fn resolve(self, tvg_id: Option<&str>, gracenote_id: Option<&str>) -> Option<String> {
        let pick = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_string);
        match self {
            Self::TvgId => pick(tvg_id),
            Self::Gracenote => pick(gracenote_id),
            Self::ChannelNumber => None,
        }
    }
}

/// Display form of an effective channel number: `123.0` renders as `123`,
/// `123.5` stays `123.5`, and `None` means the channel has no number at all.
///
/// HDHomeRun skips numberless channels outright, so this returning `None` is
/// load-bearing rather than cosmetic.
pub fn format_channel_number(value: Option<f64>) -> Option<String> {
    let value = value.filter(|v| v.is_finite())?;
    if value == value.trunc() {
        Some(format!("{}", value as i64))
    } else {
        Some(format!("{value}"))
    }
}

/// True for characters XML 1.0 cannot represent at all — not as a literal and
/// not as a numeric reference.
///
/// These matter far more than they look: a conformant parser rejects the
/// **entire document** on one of them, so a single control byte in one
/// programme's title is a total guide outage rather than one bad cell. A
/// lenient parser on the way in lets such a byte into the database, so the
/// guard has to live here.
pub fn is_forbidden_xml_char(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}')
}

/// Escape text for an XML element's content — all five of `&`, `<`, `>`, `"`
/// and `'`, as the snapshots pin — and drop characters XML cannot carry.
pub fn escape_xml(text: &str) -> Cow<'_, str> {
    escape(text, false)
}

/// Escape text for an XML attribute value.
///
/// Beyond [`escape_xml`], tab, newline, and carriage return become character
/// references. XML mandates that a parser normalize literal whitespace in an
/// attribute value to a space, so a newline inside a `tvg-id` survives our own
/// lenient reader but arrives at Plex as a space — and the channel silently
/// loses its guide with nothing on either side reporting a problem.
pub fn escape_xml_attr(text: &str) -> Cow<'_, str> {
    escape(text, true)
}

fn escape(text: &str, attribute: bool) -> Cow<'_, str> {
    let needs_work = |c: char| {
        matches!(c, '&' | '<' | '>' | '"' | '\'')
            || is_forbidden_xml_char(c)
            || (attribute && matches!(c, '\t' | '\n' | '\r'))
    };
    if !text.contains(needs_work) {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            '\t' if attribute => out.push_str("&#9;"),
            '\n' if attribute => out.push_str("&#10;"),
            '\r' if attribute => out.push_str("&#13;"),
            _ if is_forbidden_xml_char(c) => {}
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tvg_id_source_from_query() {
        let cases = [
            ("tvg_id", TvgIdSource::TvgId),
            ("TVG_ID", TvgIdSource::TvgId),
            ("gracenote", TvgIdSource::Gracenote),
            ("channel_number", TvgIdSource::ChannelNumber),
            ("nonsense", TvgIdSource::ChannelNumber),
            ("", TvgIdSource::ChannelNumber),
        ];
        for (input, want) in cases {
            assert_eq!(TvgIdSource::from_query(input), want, "{input}");
        }
        assert_eq!(TvgIdSource::default(), TvgIdSource::ChannelNumber);
    }

    #[test]
    fn tvg_id_source_resolution() {
        let cases = [
            (TvgIdSource::TvgId, Some("a.us"), Some("1234"), Some("a.us")),
            (TvgIdSource::TvgId, Some(""), Some("1234"), None),
            (TvgIdSource::TvgId, None, Some("1234"), None),
            (
                TvgIdSource::Gracenote,
                Some("a.us"),
                Some("1234"),
                Some("1234"),
            ),
            (TvgIdSource::Gracenote, Some("a.us"), Some(""), None),
            (TvgIdSource::Gracenote, Some("a.us"), None, None),
            (TvgIdSource::ChannelNumber, Some("a.us"), Some("1234"), None),
        ];
        for (source, tvg, gracenote, want) in cases {
            assert_eq!(
                source.resolve(tvg, gracenote).as_deref(),
                want,
                "{source:?} {tvg:?} {gracenote:?}"
            );
        }
    }

    #[test]
    fn channel_number_formatting() {
        let cases = [
            (Some(123.0), Some("123")),
            (Some(123.5), Some("123.5")),
            (Some(0.0), Some("0")),
            (Some(-3.0), Some("-3")),
            (Some(1.25), Some("1.25")),
            (None, None),
            (Some(f64::NAN), None),
            (Some(f64::INFINITY), None),
        ];
        for (input, want) in cases {
            assert_eq!(format_channel_number(input).as_deref(), want, "{input:?}");
        }
    }

    #[test]
    fn xml_escaping() {
        let cases = [
            ("plain", "plain"),
            ("Tom & Jerry", "Tom &amp; Jerry"),
            ("<b>", "&lt;b&gt;"),
            ("say \"hi\"", "say &quot;hi&quot;"),
            ("it's", "it&#x27;s"),
            ("", ""),
            // Whitespace is legal content and must survive verbatim.
            ("two\nlines\tand a tab", "two\nlines\tand a tab"),
            // Characters XML cannot carry are dropped, not referenced.
            ("bel\u{7}l", "bell"),
            ("\u{0}\u{8}\u{b}\u{c}\u{e}\u{1f}ok", "ok"),
            ("\u{fffe}\u{ffff}ok", "ok"),
        ];
        for (input, want) in cases {
            assert_eq!(escape_xml(input), want, "{input:?}");
        }
        // Text needing no escaping must not be copied.
        let untouched = "plain";
        let escaped = escape_xml(untouched);
        assert!(std::ptr::eq(escaped.as_ptr(), untouched.as_ptr()));
    }

    #[test]
    fn attribute_escaping_also_references_whitespace() {
        let cases = [
            ("plain", "plain"),
            ("Tom & Jerry", "Tom &amp; Jerry"),
            // Left literal, a parser would normalize these to spaces and the
            // value would silently change on the way to the client.
            ("a\nb", "a&#10;b"),
            ("a\rb", "a&#13;b"),
            ("a\tb", "a&#9;b"),
            ("bel\u{7}l", "bell"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(escape_xml_attr(input), want, "{input:?}");
        }
        let untouched = "plain";
        let escaped = escape_xml_attr(untouched);
        assert!(std::ptr::eq(escaped.as_ptr(), untouched.as_ptr()));
    }

    #[test]
    fn forbidden_char_predicate_matches_the_xml_1_0_production() {
        for c in [
            '\u{0}', '\u{8}', '\u{b}', '\u{c}', '\u{e}', '\u{1f}', '\u{fffe}', '\u{ffff}',
        ] {
            assert!(is_forbidden_xml_char(c), "{c:?}");
        }
        for c in ['\t', '\n', '\r', ' ', 'a', '\u{20}', '\u{e9}', '\u{10000}'] {
            assert!(!is_forbidden_xml_char(c), "{c:?}");
        }
    }
}
