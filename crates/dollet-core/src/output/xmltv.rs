//! XMLTV guide generation for `/output/epg` and the Xtream `xmltv.php` route.
//!
//! Written through a [`std::io::Write`] rather than returned as a `String`: the
//! caller streams this straight into the on-disk cache file Plex re-fetches, and
//! a full XMLTV feed is the one output here that has no bound on its size.
//!
//! The caller drives the order — all `<channel>` elements first, then
//! `<programme>` elements — because that order is the XMLTV convention and
//! because it lets the caller page programmes out of SQLite in id order.

use std::io::{self, Write};

use chrono::{DateTime, Utc};
use serde_json::Value as Json;

use crate::domain::{EffectiveChannel, Program};

use super::{TvgIdSource, escape_xml, escape_xml_attr, format_channel_number};

pub struct XmltvOptions<'a> {
    pub generator_name: &'a str,
    pub generator_url: &'a str,
    pub tvg_id_source: TvgIdSource,
}

impl Default for XmltvOptions<'_> {
    fn default() -> Self {
        Self {
            generator_name: "dollet-relay",
            generator_url: "https://github.com/halsafar/dollet-relay",
            tvg_id_source: TvgIdSource::default(),
        }
    }
}

/// One programme to serialize, decoupled from where it came from so generated
/// dummy programmes and stored rows share a single writer.
pub struct Programme<'a> {
    pub channel_id: &'a str,
    pub start: DateTime<Utc>,
    pub stop: DateTime<Utc>,
    pub title: &'a str,
    pub sub_title: Option<&'a str>,
    pub desc: Option<&'a str>,
    /// `Program::custom_properties`: everything XMLTV carries that has no column.
    pub extra: Option<&'a Json>,
}

impl<'a> Programme<'a> {
    pub fn from_program(channel_id: &'a str, program: &'a Program) -> Self {
        Self {
            channel_id,
            start: program.start_time,
            stop: program.end_time,
            title: &program.title,
            sub_title: program.sub_title.as_deref(),
            desc: program.description.as_deref(),
            extra: Some(&program.custom_properties),
        }
    }

    pub fn from_dummy(channel_id: &'a str, program: &'a super::dummy_epg::DummyProgram) -> Self {
        Self {
            channel_id,
            start: program.start_time,
            stop: program.end_time,
            title: &program.title,
            sub_title: None,
            desc: Some(&program.description),
            extra: None,
        }
    }
}

/// The `id` a channel is published under, honouring `tvg_id_source` and falling
/// back to the channel number, then the row id.
pub fn channel_id(channel: &EffectiveChannel, source: TvgIdSource) -> String {
    source
        .resolve(
            channel.tvg_id.as_deref(),
            channel.tvc_guide_stationid.as_deref(),
        )
        .or_else(|| format_channel_number(channel.channel_number))
        .unwrap_or_else(|| channel.id.to_string())
}

pub struct XmltvWriter<W: Write> {
    inner: W,
}

impl<W: Write> XmltvWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub fn start(&mut self, opts: &XmltvOptions<'_>) -> io::Result<()> {
        writeln!(self.inner, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
        writeln!(
            self.inner,
            r#"<tv generator-info-name="{}" generator-info-url="{}">"#,
            escape_xml_attr(opts.generator_name),
            escape_xml_attr(opts.generator_url),
        )
    }

    /// `icon` is written even when empty; the snapshots pin it that way.
    pub fn write_channel(&mut self, id: &str, display_name: &str, icon: &str) -> io::Result<()> {
        writeln!(self.inner, r#"  <channel id="{}">"#, escape_xml_attr(id))?;
        writeln!(
            self.inner,
            "    <display-name>{}</display-name>",
            escape_xml(display_name)
        )?;
        writeln!(
            self.inner,
            r#"    <icon src="{}" />"#,
            escape_xml_attr(icon)
        )?;
        writeln!(self.inner, "  </channel>")
    }

    /// **The caller must exclude `hidden_from_output` channels before calling
    /// this.** The list-shaped serializers filter for themselves, but this one
    /// writes exactly what it is handed: a channel element and its programmes
    /// are written in separate passes, so silently skipping one here would
    /// leave the other's programmes pointing at a channel that does not exist.
    pub fn write_effective_channel(
        &mut self,
        channel: &EffectiveChannel,
        source: TvgIdSource,
    ) -> io::Result<()> {
        let id = channel_id(channel, source);
        self.write_channel(
            &id,
            &channel.name,
            channel.logo_url.as_deref().unwrap_or_default(),
        )
    }

    pub fn write_programme(&mut self, programme: &Programme<'_>) -> io::Result<()> {
        writeln!(
            self.inner,
            r#"  <programme start="{}" stop="{}" channel="{}">"#,
            format_time(programme.start),
            format_time(programme.stop),
            escape_xml_attr(programme.channel_id),
        )?;
        writeln!(
            self.inner,
            "    <title>{}</title>",
            escape_xml(programme.title)
        )?;
        if let Some(sub_title) = programme.sub_title.filter(|s| !s.is_empty()) {
            writeln!(
                self.inner,
                "    <sub-title>{}</sub-title>",
                escape_xml(sub_title)
            )?;
        }
        if let Some(desc) = programme.desc.filter(|s| !s.is_empty()) {
            writeln!(self.inner, "    <desc>{}</desc>", escape_xml(desc))?;
        }
        if let Some(extra) = programme.extra.and_then(Json::as_object) {
            self.write_extra(extra)?;
        }
        writeln!(self.inner, "  </programme>")
    }

    pub fn finish(&mut self) -> io::Result<()> {
        writeln!(self.inner, "</tv>")
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Element order here is load-bearing: the snapshots are byte comparisons,
    /// not XML-canonical ones.
    fn write_extra(&mut self, extra: &serde_json::Map<String, Json>) -> io::Result<()> {
        let get = |k: &str| extra.get(k);
        let text = |k: &str| get(k).and_then(Json::as_str).filter(|s| !s.is_empty());

        for value in get("categories")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(v) = value.as_str() {
                writeln!(self.inner, "    <category>{}</category>", escape_xml(v))?;
            }
        }
        for value in get("keywords")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(v) = value.as_str() {
                writeln!(self.inner, "    <keyword>{}</keyword>", escape_xml(v))?;
            }
        }

        // A stored onscreen string beats one synthesised from the episode number.
        if let Some(v) = text("onscreen_episode") {
            self.episode_num("onscreen", &escape_xml(v))?;
        } else if let Some(episode) = get("episode").and_then(as_int) {
            self.episode_num("onscreen", &format!("E{episode}"))?;
        }
        if let Some(v) = text("dd_progid") {
            self.episode_num("dd_progid", &escape_xml(v))?;
        }
        for system in ["thetvdb.com", "themoviedb.org", "imdb.com"] {
            if let Some(v) = text(&format!("{system}_id")) {
                self.episode_num(system, &escape_xml(v))?;
            }
        }
        if let (Some(season), Some(episode)) = (get("season"), get("episode")) {
            // XMLTV's numbering is zero-based; ours is not. A value that is not
            // a number at all becomes 0 rather than failing the programme.
            let zero_based = |v: &Json| as_int(v).map_or(0, |n| n - 1).max(0);
            self.episode_num(
                "xmltv_ns",
                &format!("{}.{}.", zero_based(season), zero_based(episode)),
            )?;
        }

        if let Some(v) = text("language") {
            writeln!(self.inner, "    <language>{}</language>", escape_xml(v))?;
        }
        if let Some(v) = text("original_language") {
            writeln!(
                self.inner,
                "    <orig-language>{}</orig-language>",
                escape_xml(v)
            )?;
        }
        if let Some(length) = get("length").and_then(Json::as_object) {
            let units = length
                .get("units")
                .and_then(Json::as_str)
                .unwrap_or("minutes");
            let value = length.get("value").map(scalar_text).unwrap_or_default();
            writeln!(
                self.inner,
                r#"    <length units="{}">{}</length>"#,
                escape_xml_attr(units),
                escape_xml(&value)
            )?;
        }

        self.write_group(
            "video",
            get("video"),
            &["present", "colour", "aspect", "quality"],
        )?;
        self.write_group("audio", get("audio"), &["present", "stereo"])?;

        for entry in get("subtitles")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            let type_attr = match entry.get("type").and_then(Json::as_str) {
                Some(t) if !t.is_empty() => format!(r#" type="{}""#, escape_xml_attr(t)),
                _ => String::new(),
            };
            writeln!(self.inner, "    <subtitles{type_attr}>")?;
            if let Some(lang) = entry.get("language").and_then(Json::as_str) {
                writeln!(
                    self.inner,
                    "      <language>{}</language>",
                    escape_xml(lang)
                )?;
            }
            writeln!(self.inner, "    </subtitles>")?;
        }

        if let Some(rating) = text("rating") {
            // XMLTV's `system` attribute names the rating authority, and a
            // programme carrying a bare `TV-14`-style value without one is
            // using the US TV Parental Guidelines. Naming it lets a client pick
            // the right icon set instead of guessing from the value.
            let system = text("rating_system").unwrap_or("TV Parental Guidelines");
            writeln!(
                self.inner,
                r#"    <rating system="{}">"#,
                escape_xml_attr(system)
            )?;
            writeln!(self.inner, "      <value>{}</value>", escape_xml(rating))?;
            writeln!(self.inner, "    </rating>")?;
        }
        for entry in get("star_ratings")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            let Some(value) = entry.get("value").and_then(Json::as_str) else {
                continue;
            };
            let system = match entry.get("system").and_then(Json::as_str) {
                Some(s) => format!(r#" system="{}""#, escape_xml_attr(s)),
                None => String::new(),
            };
            writeln!(self.inner, "    <star-rating{system}>")?;
            writeln!(self.inner, "      <value>{}</value>", escape_xml(value))?;
            writeln!(self.inner, "    </star-rating>")?;
        }

        for entry in get("reviews")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            let Some(content) = entry.get("content").and_then(Json::as_str) else {
                continue;
            };
            let mut attrs = vec![format!(
                r#"type="{}""#,
                escape_xml_attr(entry.get("type").and_then(Json::as_str).unwrap_or("text"))
            )];
            for key in ["source", "reviewer"] {
                if let Some(v) = entry.get(key).and_then(Json::as_str) {
                    attrs.push(format!(r#"{key}="{}""#, escape_xml_attr(v)));
                }
            }
            writeln!(
                self.inner,
                "    <review {}>{}</review>",
                attrs.join(" "),
                escape_xml(content)
            )?;
        }
        for entry in get("images").and_then(Json::as_array).into_iter().flatten() {
            let Some(url) = entry.get("url").and_then(Json::as_str) else {
                continue;
            };
            let mut attrs = String::new();
            for key in ["type", "size", "orient", "system"] {
                if let Some(v) = entry.get(key).and_then(Json::as_str) {
                    attrs.push_str(&format!(r#" {key}="{}""#, escape_xml_attr(v)));
                }
            }
            writeln!(self.inner, "    <image{attrs}>{}</image>", escape_xml(url))?;
        }

        if let Some(credits) = get("credits").and_then(Json::as_object) {
            writeln!(self.inner, "    <credits>")?;
            let roles = [
                "director",
                "writer",
                "adapter",
                "producer",
                "composer",
                "editor",
                "presenter",
                "commentator",
                "guest",
            ];
            for role in roles {
                for name in string_list(credits.get(role)) {
                    writeln!(self.inner, "      <{role}>{}</{role}>", escape_xml(&name))?;
                }
            }
            for actor in as_list(credits.get("actor")) {
                self.write_actor(actor)?;
            }
            writeln!(self.inner, "    </credits>")?;
        }

        if let Some(v) = text("date") {
            writeln!(self.inner, "    <date>{}</date>", escape_xml(v))?;
        }
        if let Some(v) = text("country") {
            writeln!(self.inner, "    <country>{}</country>", escape_xml(v))?;
        }
        if let Some(v) = text("icon") {
            writeln!(self.inner, r#"    <icon src="{}" />"#, escape_xml_attr(v))?;
        }

        if get("previously_shown")
            .and_then(Json::as_bool)
            .unwrap_or(false)
        {
            let details = get("previously_shown_details").and_then(Json::as_object);
            let mut attrs = String::new();
            for key in ["start", "channel"] {
                if let Some(v) = details.and_then(|d| d.get(key)).and_then(Json::as_str) {
                    attrs.push_str(&format!(r#" {key}="{}""#, escape_xml_attr(v)));
                }
            }
            writeln!(self.inner, "    <previously-shown{attrs} />")?;
        }
        self.write_flag("premiere", get("premiere"), text("premiere_text"))?;
        self.write_flag("last-chance", get("last_chance"), text("last_chance_text"))?;
        if get("new").and_then(Json::as_bool).unwrap_or(false) {
            writeln!(self.inner, "    <new />")?;
        }
        if get("live").and_then(Json::as_bool).unwrap_or(false) {
            writeln!(self.inner, "    <live />")?;
        }
        Ok(())
    }

    /// `system` is always one of our own literals, never provider data.
    fn episode_num(&mut self, system: &str, text: &str) -> io::Result<()> {
        writeln!(
            self.inner,
            r#"    <episode-num system="{system}">{text}</episode-num>"#
        )
    }

    fn write_group(&mut self, name: &str, value: Option<&Json>, keys: &[&str]) -> io::Result<()> {
        let Some(group) = value.and_then(Json::as_object) else {
            return Ok(());
        };
        if keys
            .iter()
            .all(|k| group.get(*k).and_then(Json::as_str).is_none())
        {
            return Ok(());
        }
        writeln!(self.inner, "    <{name}>")?;
        for key in keys {
            if let Some(v) = group.get(*key).and_then(Json::as_str) {
                writeln!(self.inner, "      <{key}>{}</{key}>", escape_xml(v))?;
            }
        }
        writeln!(self.inner, "    </{name}>")
    }

    fn write_actor(&mut self, actor: &Json) -> io::Result<()> {
        let (name, role, guest) = match actor {
            Json::String(name) => (name.as_str(), None, false),
            Json::Object(map) => (
                map.get("name").and_then(Json::as_str).unwrap_or_default(),
                map.get("role").and_then(Json::as_str),
                map.get("guest").and_then(Json::as_bool).unwrap_or(false),
            ),
            _ => return Ok(()),
        };
        let role = role.map_or(String::new(), |r| {
            format!(r#" role="{}""#, escape_xml_attr(r))
        });
        let guest = if guest { r#" guest="yes""# } else { "" };
        writeln!(
            self.inner,
            "      <actor{role}{guest}>{}</actor>",
            escape_xml(name)
        )
    }

    fn write_flag(&mut self, tag: &str, set: Option<&Json>, text: Option<&str>) -> io::Result<()> {
        if !set.and_then(Json::as_bool).unwrap_or(false) {
            return Ok(());
        }
        match text {
            Some(t) => writeln!(self.inner, "    <{tag}>{}</{tag}>", escape_xml(t)),
            None => writeln!(self.inner, "    <{tag} />"),
        }
    }
}

fn format_time(value: DateTime<Utc>) -> String {
    value.format("%Y%m%d%H%M%S +0000").to_string()
}

/// Season and episode reach us as either a number or a numeric string,
/// depending on which provider the row was parsed from.
fn as_int(value: &Json) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

/// Providers put numbers where XMLTV wants text often enough that coercing is
/// cheaper than dropping the field.
fn scalar_text(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        Json::Number(n) => n.to_string(),
        Json::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn as_list(value: Option<&Json>) -> Vec<&Json> {
    match value {
        Some(Json::Array(items)) => items.iter().collect(),
        Some(other @ (Json::String(_) | Json::Object(_))) => vec![other],
        _ => Vec::new(),
    }
}

fn string_list(value: Option<&Json>) -> Vec<String> {
    as_list(value)
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::UserLevel;
    use serde_json::json;
    use uuid::Uuid;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn channel(id: i64, name: &str, number: Option<f64>) -> EffectiveChannel {
        EffectiveChannel {
            id,
            uuid: Uuid::from_u128(id as u128),
            channel_number: number,
            name: name.into(),
            channel_group_id: Some(1),
            created_at: chrono::DateTime::UNIX_EPOCH,
            group_name: Some("News".into()),
            logo_url: Some(format!("http://ipx.test/logo/{id}.png")),
            tvg_id: Some(format!("ch{id}.us")),
            tvc_guide_stationid: Some(format!("{}", 90000 + id)),
            epg_data_id: Some(id),
            stream_profile_id: None,
            user_level: UserLevel::Streamer,
            is_adult: false,
            hidden_from_output: false,
            is_catchup: false,
            catchup_days: 0,
        }
    }

    fn program(title: &str, extra: Json) -> Program {
        Program {
            id: 1,
            epg_data_id: 1,
            tvg_id: Some("ch1.us".into()),
            start_time: utc("2026-09-11T15:30:00Z"),
            end_time: utc("2026-09-11T16:00:00Z"),
            title: title.into(),
            sub_title: Some("Pilot".into()),
            description: Some("A description.".into()),
            custom_properties: extra,
        }
    }

    fn render(body: impl FnOnce(&mut XmltvWriter<Vec<u8>>)) -> String {
        let mut writer = XmltvWriter::new(Vec::new());
        writer.start(&XmltvOptions::default()).unwrap();
        body(&mut writer);
        writer.finish().unwrap();
        let xml = String::from_utf8(writer.into_inner()).unwrap();
        assert_eq!(well_formedness_errors(&xml), Vec::<String>::new());
        xml
    }

    /// Everything a conformant parser would reject. Plex rejects the *whole*
    /// document on any of these, so a snapshot that merely "looks right" is not
    /// evidence the guide loads — which is exactly how a control byte in one
    /// title becomes a total outage.
    fn well_formedness_errors(xml: &str) -> Vec<String> {
        let mut errors: Vec<String> = xml
            .char_indices()
            .filter(|(_, c)| crate::output::is_forbidden_xml_char(*c))
            .map(|(i, c)| format!("illegal XML character {c:?} at byte {i}"))
            .collect();

        let mut reader = quick_xml::Reader::from_str(xml);
        // Defaults, stated because the parser used elsewhere in this crate
        // deliberately relaxes both and must not be confused with this one.
        reader.config_mut().check_end_names = true;
        reader.config_mut().allow_unmatched_ends = false;
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(e) => {
                    errors.push(format!("not well-formed: {e}"));
                    break;
                }
            }
        }
        errors
    }

    #[test]
    fn guide_with_channels_and_a_plain_programme() {
        let channels = [
            channel(1, "Channel A", Some(101.0)),
            channel(2, "Channel & B", None),
        ];
        let programme = program("Show", Json::Null);
        insta::assert_snapshot!(render(|w| {
            for ch in &channels {
                w.write_effective_channel(ch, TvgIdSource::ChannelNumber)
                    .unwrap();
            }
            w.write_programme(&Programme::from_program("101", &programme))
                .unwrap();
        }));
    }

    fn full_extra() -> Json {
        json!({
            "categories": ["News", "Live & Local"],
            "keywords": ["breaking"],
            "onscreen_episode": "S02E06",
            "season": 2,
            "episode": 6,
            "dd_progid": "EP01.0002",
            "thetvdb.com_id": "series/12",
            "imdb.com_id": "tt0000001",
            "language": "English",
            "original_language": "French",
            "length": {"value": 30, "units": "minutes"},
            "video": {"present": "yes", "colour": "yes", "aspect": "16:9", "quality": "HDTV"},
            "audio": {"present": "yes", "stereo": "stereo"},
            "subtitles": [{"type": "teletext", "language": "English"}, {"type": "onscreen"},
                          {"language": "Welsh"}],
            "rating": "TV-14",
            "rating_system": "VCHIP",
            "star_ratings": [{"value": "4/5", "system": "tv.com"}, {"value": "8/10"}],
            "reviews": [{"content": "Good.", "type": "text", "source": "x", "reviewer": "Rita"},
                        {"content": "Bare."}],
            "images": [{"url": "http://i/1.jpg", "type": "poster", "size": "2",
                        "orient": "P", "system": "tmdb"},
                       {"url": "http://i/2.jpg"}],
            "credits": {
                "director": ["Dee"],
                "writer": ["Wanda", "Walt"],
                "actor": [{"name": "Ann", "role": "Lead", "guest": true}, {"name": "Bob"}],
            },
            "date": "20240101",
            "country": "US",
            "icon": "http://i/poster.jpg",
            "previously_shown": true,
            "previously_shown_details": {"start": "20240101000000", "channel": "b.us"},
            "premiere": true,
            "premiere_text": "Series premiere",
            "last_chance": true,
            "new": true,
            "live": true,
        })
    }

    #[test]
    fn guide_with_a_fully_populated_programme() {
        let programme = program("Everything", full_extra());
        insta::assert_snapshot!(render(|w| {
            w.write_programme(&Programme::from_program("101", &programme))
                .unwrap();
        }));
    }

    #[test]
    fn guide_with_sparse_metadata_variants() {
        let cases = [
            json!({"episode": 6}),
            json!({"season": 3, "episode": 1}),
            json!({"season": 0, "episode": 0}),
            json!({"length": {"value": "45"}}),
            json!({"video": {"unknown": "x"}}),
            json!({"video": {"present": 5}}),
            json!({"audio": {"stereo": "mono"}}),
            json!({"length": {"value": true}}),
            json!({"length": {"value": ["45"]}}),
            json!({"season": "3", "episode": "12"}),
            json!({"season": "many", "episode": "some"}),
            json!({"season": null, "episode": null}),
            json!({"rating": "PG"}),
            json!({"credits": {"director": "Solo Dee", "actor": "Solo Ann"}}),
            json!({"credits": {"actor": [12345]}}),
            json!({"star_ratings": [{"nothing": true}], "reviews": [{}], "images": [{}]}),
            json!({"subtitles": ["not an object"], "categories": [7], "keywords": [7]}),
            json!({"premiere": true, "last_chance": true}),
            json!({"previously_shown": true}),
            json!({"new": false, "live": false, "premiere": false}),
        ];
        let rendered: Vec<String> = cases
            .iter()
            .map(|extra| {
                let mut writer = XmltvWriter::new(Vec::new());
                let programme = program("Sparse", extra.clone());
                writer
                    .write_programme(&Programme::from_program("1", &programme))
                    .unwrap();
                String::from_utf8(writer.into_inner()).unwrap()
            })
            .collect();
        insta::assert_snapshot!(rendered.join("---\n"));
    }

    #[test]
    fn a_programme_with_no_sub_title_or_description_omits_them() {
        let mut programme = program("Bare", Json::Null);
        programme.sub_title = Some(String::new());
        programme.description = None;
        let out = render(|w| {
            w.write_programme(&Programme::from_program("1", &programme))
                .unwrap();
        });
        assert!(!out.contains("<sub-title>"), "{out}");
        assert!(!out.contains("<desc>"), "{out}");
    }

    #[test]
    fn channel_ids_follow_the_configured_source() {
        let with_everything = channel(1, "A", Some(101.0));
        let mut numberless = channel(2, "B", None);
        numberless.tvg_id = None;
        numberless.tvc_guide_stationid = None;

        let cases = [
            (TvgIdSource::ChannelNumber, &with_everything, "101"),
            (TvgIdSource::TvgId, &with_everything, "ch1.us"),
            (TvgIdSource::Gracenote, &with_everything, "90001"),
            (TvgIdSource::ChannelNumber, &numberless, "2"),
            (TvgIdSource::TvgId, &numberless, "2"),
            (TvgIdSource::Gracenote, &numberless, "2"),
        ];
        for (source, ch, want) in cases {
            assert_eq!(channel_id(ch, source), want, "{source:?} {}", ch.id);
        }
    }

    #[test]
    fn a_channel_without_a_logo_still_gets_an_empty_icon() {
        let mut ch = channel(1, "A", Some(1.0));
        ch.logo_url = None;
        let out = render(|w| w.write_effective_channel(&ch, TvgIdSource::TvgId).unwrap());
        assert!(out.contains(r#"<icon src="" />"#), "{out}");
    }

    #[test]
    fn generator_attributes_are_configurable_and_escaped() {
        let opts = XmltvOptions {
            generator_name: "A & B",
            generator_url: "http://x/?a=1&b=2",
            ..XmltvOptions::default()
        };
        let mut writer = XmltvWriter::new(Vec::new());
        writer.start(&opts).unwrap();
        let out = String::from_utf8(writer.into_inner()).unwrap();
        assert!(out.contains(r#"generator-info-name="A &amp; B""#), "{out}");
        assert!(
            out.contains(r#"generator-info-url="http://x/?a=1&amp;b=2""#),
            "{out}"
        );
    }

    #[test]
    fn dummy_programmes_use_the_same_writer() {
        let dummy = super::super::dummy_epg::DummyProgram {
            start_time: utc("2026-09-11T16:00:00Z"),
            end_time: utc("2026-09-11T20:00:00Z"),
            title: "Channel A".into(),
            description: "No guide data.".into(),
        };
        insta::assert_snapshot!(render(|w| {
            w.write_programme(&Programme::from_dummy("101", &dummy))
                .unwrap();
        }));
    }

    #[test]
    fn hostile_provider_text_still_produces_a_loadable_document() {
        // The control byte and the newline-in-an-id are the two that cost the
        // whole guide.
        let mut ch = channel(1, "Bel\u{7}l & <Co>", Some(101.0));
        ch.tvg_id = Some("a\nb.us".into());
        ch.logo_url = Some("http://l/a.png?x=1&y=2".into());

        let programme = program(
            "Tom & Jerry \u{1f}",
            json!({
                "categories": ["News & \u{b}Weather"],
                "rating": "TV-14", "rating_system": "a\tb",
                "icon": "http://i/1.jpg?a=1&b=2",
                "subtitles": [{"type": "tele\ntext", "language": "En\u{0}glish"}],
                "credits": {"actor": [{"name": "A\u{3}nn", "role": "Le\nad"}]},
                "previously_shown": true,
                "previously_shown_details": {"start": "2024\n01", "channel": "b&c"},
            }),
        );

        let xml = render(|w| {
            w.write_effective_channel(&ch, TvgIdSource::TvgId).unwrap();
            w.write_programme(&Programme::from_program("a\nb.us", &programme))
                .unwrap();
        });

        // `render` already asserts well-formedness; pin the two mechanisms.
        assert!(!xml.contains('\u{7}'), "control bytes must not survive");
        assert!(xml.contains(r#"<channel id="a&#10;b.us">"#), "{xml}");
        insta::assert_snapshot!(xml);
    }

    #[test]
    fn the_well_formedness_check_actually_rejects_bad_documents() {
        // A check that never fails is not a check.
        let cases = [
            ("<tv>\u{7}</tv>", "illegal XML character"),
            ("<tv><channel></tv>", "not well-formed"),
            ("<tv></other>", "not well-formed"),
        ];
        for (xml, want) in cases {
            let errors = well_formedness_errors(xml);
            assert!(
                errors.iter().any(|e| e.contains(want)),
                "{xml:?} produced {errors:?}"
            );
        }
        assert!(well_formedness_errors("<tv><channel id=\"a\"/></tv>").is_empty());
    }

    /// Accepts `remaining` writes, then fails every one after. Sweeping
    /// `remaining` across a whole document exercises each `?` in the writer,
    /// which is the only way a disk filling up mid-guide gets covered.
    struct FailAfter {
        remaining: usize,
    }

    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::StorageFull, "no space"));
            }
            self.remaining -= 1;
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failing_writer_propagates_from_every_element() {
        let ch = channel(1, "Channel A", Some(101.0));
        let programme = program("Everything", full_extra());
        let dummy = super::super::dummy_epg::DummyProgram {
            start_time: utc("2026-09-11T16:00:00Z"),
            end_time: utc("2026-09-11T20:00:00Z"),
            title: "Channel A".into(),
            description: "No guide data.".into(),
        };

        // No `onscreen_episode`, so the synthesised `E{episode}` branch is on
        // the swept path too.
        let synthesised = program("Synthesised", json!({"episode": 6}));

        let write_all = |remaining: usize| -> io::Result<()> {
            let mut w = XmltvWriter::new(FailAfter { remaining });
            w.start(&XmltvOptions::default())?;
            w.write_effective_channel(&ch, TvgIdSource::TvgId)?;
            w.write_programme(&Programme::from_program("101", &programme))?;
            w.write_programme(&Programme::from_program("101", &synthesised))?;
            w.write_programme(&Programme::from_dummy("101", &dummy))?;
            w.finish()
        };

        // `Write` demands a flush; this sink never buffers, so it is a no-op.
        assert!(FailAfter { remaining: 0 }.flush().is_ok());

        let total = (0..)
            .find(|n| write_all(*n).is_ok())
            .expect("the document is finite");
        assert!(total > 40, "expected a multi-element document, got {total}");
        for remaining in 0..total {
            assert!(
                write_all(remaining).is_err(),
                "write {remaining} of {total}"
            );
        }
    }

    #[test]
    fn the_generated_guide_parses_back_to_the_same_programmes() {
        let ch = channel(1, "Channel A", Some(101.0));
        let programme = program(
            "Show",
            json!({"categories": ["News"], "season": 2, "episode": 6, "rating": "TV-14"}),
        );
        let xml = render(|w| {
            w.write_effective_channel(&ch, TvgIdSource::TvgId).unwrap();
            w.write_programme(&Programme::from_program("ch1.us", &programme))
                .unwrap();
        });

        let parsed: Vec<_> = crate::parse::xmltv::from_bytes(xml.as_bytes())
            .unwrap()
            .map(|i| i.unwrap())
            .collect();
        assert_eq!(parsed.len(), 2);
        let programmes: Vec<_> = parsed
            .into_iter()
            .filter_map(|item| match item {
                crate::parse::xmltv::XmltvItem::Programme(p) => Some(p),
                crate::parse::xmltv::XmltvItem::Channel(_) => None,
            })
            .collect();
        let p = &programmes[0];
        assert_eq!(p.title, "Show");
        assert_eq!(p.start_time, programme.start_time);
        assert_eq!(p.end_time, programme.end_time);
        assert_eq!(p.custom_properties["categories"], json!(["News"]));
        assert_eq!(p.custom_properties["season"], json!(2));
        assert_eq!(p.custom_properties["episode"], json!(6));
        assert_eq!(p.custom_properties["rating"], json!("TV-14"));
    }
}
