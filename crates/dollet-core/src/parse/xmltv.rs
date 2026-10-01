//! Streaming XMLTV parser.
//!
//! Guides are the largest thing this process ingests, so the document is pulled
//! element by element rather than held in memory: the caller takes each item,
//! batches it into SQLite, and drops it. Nothing here allocates per-document
//! state beyond the current element.
//!
//! Programme metadata that has no column on [`crate::domain::Program`] is
//! collected into the `custom_properties` JSON shape the importer carries, so
//! the XMLTV serializer re-emits what this parser captured.
//!
//! That is not a full round trip: `<url>`,
//! `lang` attributes, titles in non-primary languages, `<icon>` dimensions, and
//! `clumpidx` are all read past. Nothing downstream consumes them, so capturing
//! them would only widen a JSON blob that every programme row carries.

use std::borrow::Cow;
use std::io::{BufRead, Cursor};

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use quick_xml::Reader;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesStart, BytesText, Event};
use serde_json::{Map, Value as Json, json};

use crate::domain::{EpgData, Id, Program};
use crate::{Error, Result};

use super::{compress, entities};

/// A `<channel>` element. Becomes an `EpgData` row once the source id is known.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedChannel {
    pub tvg_id: String,
    pub display_name: Option<String>,
    pub icon_url: Option<String>,
}

/// A `<programme>` element, keyed by the `channel` attribute rather than by a
/// row id, which the caller resolves.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedProgramme {
    pub tvg_id: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub title: String,
    pub sub_title: Option<String>,
    pub description: Option<String>,
    pub custom_properties: Json,
}

#[derive(Debug, Clone, PartialEq)]
pub enum XmltvItem {
    Channel(ParsedChannel),
    Programme(ParsedProgramme),
}

impl ParsedChannel {
    pub fn into_epg_data(self, id: Id, epg_source_id: Option<Id>) -> EpgData {
        EpgData {
            id,
            epg_source_id,
            name: self.display_name.unwrap_or_else(|| self.tvg_id.clone()),
            tvg_id: Some(self.tvg_id),
            icon_url: self.icon_url,
        }
    }
}

impl ParsedProgramme {
    pub fn into_program(self, id: Id, epg_data_id: Id) -> Program {
        Program {
            id,
            epg_data_id,
            tvg_id: Some(self.tvg_id),
            start_time: self.start_time,
            end_time: self.end_time,
            title: self.title,
            sub_title: self.sub_title,
            description: self.description,
            custom_properties: self.custom_properties,
        }
    }
}

/// Pull parser over an XMLTV document.
///
/// Malformed *elements* are skipped; only a malformed *document* is an error,
/// because a guide with one unparseable programme is still worth 20,000 good
/// ones. Counts of what was skipped are on [`XmltvReader::skipped`].
pub struct XmltvReader<R: BufRead> {
    reader: Reader<R>,
    buf: Vec<u8>,
    skipped: usize,
    unrecognised_zones: usize,
}

pub fn from_reader<R: std::io::Read + 'static>(input: R) -> Result<XmltvReader<Box<dyn BufRead>>> {
    Ok(XmltvReader::new(compress::reader(input)?))
}

pub fn from_bytes(input: &[u8]) -> Result<XmltvReader<Cursor<Vec<u8>>>> {
    Ok(XmltvReader::new(Cursor::new(compress::decompress(input)?)))
}

impl<R: BufRead> XmltvReader<R> {
    pub fn new(inner: R) -> Self {
        let mut reader = Reader::from_reader(inner);
        let config = reader.config_mut();
        // Guides in the wild close tags they never opened and leave tags open at
        // EOF. Both are recoverable; refusing them would cost the whole guide.
        config.check_end_names = false;
        config.allow_unmatched_ends = true;
        // Text is *not* trimmed per event: mixed content like
        // `<sub-title>Part <b>one</b></sub-title>` would lose the space. The
        // concatenated result is trimmed once instead, in `read_text`.
        Self {
            reader,
            buf: Vec::new(),
            skipped: 0,
            unrecognised_zones: 0,
        }
    }

    /// Elements dropped because a required field was missing or unparseable.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// Timestamps whose zone the provider stated but this parser could not
    /// resolve, and which were therefore read as UTC.
    ///
    /// Worth surfacing next to [`Self::skipped`] because the failure is
    /// invisible in the data: an unresolved zone shifts a provider's whole
    /// guide by hours and every programme still looks perfectly well-formed.
    ///
    /// A timestamp with *no* zone is not counted. The DTD leaves those to the
    /// source's local time; this assumes UTC — counting
    /// them would make the number equal the programme count for every guide
    /// that omits zones, which is most of them.
    pub fn unrecognised_zones(&self) -> usize {
        self.unrecognised_zones
    }

    /// Owning each event releases the borrow on the scratch buffer, which is
    /// what lets the element readers below recurse.
    fn next_event(&mut self) -> Result<Event<'static>> {
        self.buf.clear();
        self.reader
            .read_event_into(&mut self.buf)
            .map(Event::into_owned)
            .map_err(|e| Error::invalid(format!("xmltv: {e}")))
    }

    pub fn next_item(&mut self) -> Result<Option<XmltvItem>> {
        loop {
            let start = match self.next_event()? {
                Event::Eof => return Ok(None),
                Event::Start(start) => start,
                // A self-closing `<channel id="x"/>` carries no display-name, so
                // there is nothing worth a row; skip without counting it.
                _ => continue,
            };
            let item = match start.local_name().as_ref() {
                b"channel" => self.read_channel(&start)?,
                b"programme" => self.read_programme(&start)?,
                _ => continue,
            };
            match item {
                Some(item) => return Ok(Some(item)),
                None => self.skipped += 1,
            }
        }
    }

    fn read_channel(&mut self, start: &BytesStart<'_>) -> Result<Option<XmltvItem>> {
        let tvg_id = attribute(start, b"id").unwrap_or_default();
        let mut display_name = None;
        let mut icon_url = None;

        // Every child is consumed whole by `read_text`, so the only `End` that
        // reaches this loop is the channel's own and no depth counter is needed.
        loop {
            match self.next_event()? {
                Event::Start(child) => {
                    let name = child.local_name().as_ref().to_vec();
                    if name == b"icon" {
                        icon_url = icon_url.or_else(|| non_empty(attribute(&child, b"src")));
                    }
                    let text = self.read_text()?;
                    if name == b"display-name" && display_name.is_none() && !text.is_empty() {
                        display_name = Some(text);
                    }
                }
                Event::Empty(child) if child.local_name().as_ref() == b"icon" => {
                    icon_url = icon_url.or_else(|| non_empty(attribute(&child, b"src")));
                }
                Event::End(_) | Event::Eof => break,
                _ => {}
            }
        }

        if tvg_id.is_empty() {
            return Ok(None);
        }
        Ok(Some(XmltvItem::Channel(ParsedChannel {
            tvg_id,
            display_name,
            icon_url,
        })))
    }

    fn read_programme(&mut self, start: &BytesStart<'_>) -> Result<Option<XmltvItem>> {
        let tvg_id = attribute(start, b"channel").unwrap_or_default();
        let mut read_time = |raw: Option<String>| {
            let (time, resolved) = parse_time_checked(&raw?)?;
            if !resolved {
                self.unrecognised_zones += 1;
            }
            Some(time)
        };
        let start_time = read_time(attribute(start, b"start"));
        let end_time = read_time(attribute(start, b"stop"));

        let mut fields = ProgrammeFields::default();
        self.read_programme_children(&mut fields)?;

        let (Some(start_time), Some(end_time)) = (start_time, end_time) else {
            return Ok(None);
        };
        if tvg_id.is_empty() {
            return Ok(None);
        }

        Ok(Some(XmltvItem::Programme(ParsedProgramme {
            tvg_id,
            start_time,
            end_time,
            title: fields.title.unwrap_or_default(),
            sub_title: fields.sub_title,
            description: fields.description,
            custom_properties: fields.props.finish(),
        })))
    }

    fn read_programme_children(&mut self, fields: &mut ProgrammeFields) -> Result<()> {
        loop {
            match self.next_event()? {
                Event::Start(child) => self.read_programme_child(&child, fields)?,
                Event::Empty(child) => fields.props.empty_element(&child),
                Event::End(_) | Event::Eof => return Ok(()),
                _ => {}
            }
        }
    }

    /// Every arm consumes its element to the matching end tag, which is what
    /// lets the caller above treat any `End` it sees as the programme's own.
    fn read_programme_child(
        &mut self,
        child: &BytesStart<'_>,
        fields: &mut ProgrammeFields,
    ) -> Result<()> {
        match child.local_name().as_ref() {
            b"title" => {
                let text = self.read_text()?;
                if fields.title.is_none() && !text.is_empty() {
                    fields.title = Some(text);
                }
            }
            b"sub-title" => {
                let text = self.read_text()?;
                fields.sub_title = fields.sub_title.take().or(non_empty(Some(text)));
            }
            b"desc" => {
                let text = self.read_text()?;
                fields.description = fields.description.take().or(non_empty(Some(text)));
            }
            b"credits" => {
                let credits = self.read_credits()?;
                fields.props.credits = credits;
            }
            b"rating" | b"star-rating" => {
                let system = attribute(child, b"system");
                let value = self.read_nested_text(b"value")?;
                fields
                    .props
                    .rating(child.local_name().as_ref(), system, value);
            }
            b"video" | b"audio" | b"subtitles" => {
                let kind = child.local_name().as_ref().to_vec();
                let sub_type = attribute(child, b"type");
                let parts = self.read_simple_children()?;
                fields.props.grouped(&kind, sub_type, parts);
            }
            b"icon" => {
                fields.props.icon = fields
                    .props
                    .icon
                    .take()
                    .or(non_empty(attribute(child, b"src")));
                self.read_text()?;
            }
            _ => {
                let name = child.local_name().as_ref().to_vec();
                let attrs = collect_attributes(child);
                let text = self.read_text()?;
                fields.props.simple_element(&name, &attrs, text);
            }
        }
        Ok(())
    }

    /// Concatenated character data up to the matching end tag, nested markup
    /// dropped. XMLTV text elements are `#PCDATA`, but guides do smuggle stray
    /// `<br/>` into descriptions.
    fn read_text(&mut self) -> Result<String> {
        let mut out = String::new();
        let mut depth = 0usize;
        loop {
            match self.next_event()? {
                Event::Text(t) => out.push_str(&decode_text(&t)),
                Event::CData(t) => out.push_str(&String::from_utf8_lossy(&t)),
                Event::Start(_) => depth += 1,
                Event::End(_) => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(strip_forbidden(out.trim()))
    }

    /// `<rating><value>TV-14</value></rating>`: the one shape where the payload
    /// sits one level down.
    fn read_nested_text(&mut self, wanted: &[u8]) -> Result<Option<String>> {
        let mut value = None;
        loop {
            match self.next_event()? {
                Event::Start(child) => {
                    let matched = child.local_name().as_ref() == wanted;
                    let text = self.read_text()?;
                    if matched {
                        value = value.or(non_empty(Some(text)));
                    }
                }
                Event::End(_) | Event::Eof => break,
                _ => {}
            }
        }
        Ok(value)
    }

    /// Direct children as `(name, text)`, for `<video>`, `<audio>`, `<subtitles>`.
    fn read_simple_children(&mut self) -> Result<Vec<(Vec<u8>, String)>> {
        let mut parts = Vec::new();
        loop {
            match self.next_event()? {
                Event::Start(child) => {
                    let name = child.local_name().as_ref().to_vec();
                    let text = self.read_text()?;
                    if !text.is_empty() {
                        parts.push((name, text));
                    }
                }
                Event::End(_) | Event::Eof => break,
                _ => {}
            }
        }
        Ok(parts)
    }

    fn read_credits(&mut self) -> Result<Map<String, Json>> {
        let mut credits: Map<String, Json> = Map::new();
        loop {
            match self.next_event()? {
                Event::Start(child) => {
                    let role = String::from_utf8_lossy(child.local_name().as_ref()).into_owned();
                    let actor_role = attribute(&child, b"role");
                    let guest = attribute(&child, b"guest").as_deref() == Some("yes");
                    let name = self.read_text()?;
                    if name.is_empty() {
                        continue;
                    }
                    let entry = if role == "actor" {
                        let mut actor = Map::new();
                        actor.insert("name".into(), json!(name));
                        if let Some(r) = actor_role {
                            actor.insert("role".into(), json!(r));
                        }
                        if guest {
                            actor.insert("guest".into(), json!(true));
                        }
                        Json::Object(actor)
                    } else {
                        json!(name)
                    };
                    credits
                        .entry(role)
                        .or_insert_with(|| Json::Array(Vec::new()))
                        .as_array_mut()
                        .expect("credits entries are always arrays")
                        .push(entry);
                }
                Event::End(_) | Event::Eof => break,
                _ => {}
            }
        }
        Ok(credits)
    }
}

impl<R: BufRead> Iterator for XmltvReader<R> {
    type Item = Result<XmltvItem>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_item().transpose()
    }
}

#[derive(Default)]
struct ProgrammeFields {
    title: Option<String>,
    sub_title: Option<String>,
    description: Option<String>,
    props: PropsBuilder,
}

/// Accumulates everything `Program` has no column for into the
/// `custom_properties` JSON shape.
#[derive(Default)]
struct PropsBuilder {
    map: Map<String, Json>,
    categories: Vec<Json>,
    keywords: Vec<Json>,
    star_ratings: Vec<Json>,
    subtitles: Vec<Json>,
    reviews: Vec<Json>,
    images: Vec<Json>,
    credits: Map<String, Json>,
    icon: Option<String>,
}

impl PropsBuilder {
    fn simple_element(&mut self, name: &[u8], attrs: &[(String, String)], text: String) {
        let attr = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        match name {
            b"category" if !text.is_empty() => self.categories.push(json!(text)),
            b"keyword" if !text.is_empty() => self.keywords.push(json!(text)),
            b"episode-num" if !text.is_empty() => {
                // The XMLTV DTD defaults a missing system to onscreen.
                self.episode_num(&attr("system").unwrap_or_else(|| "onscreen".into()), &text);
            }
            b"date" if !text.is_empty() => self.set("date", json!(text)),
            b"country" if !text.is_empty() => self.set("country", json!(text)),
            b"language" if !text.is_empty() => self.set("language", json!(text)),
            b"orig-language" if !text.is_empty() => self.set("original_language", json!(text)),
            b"length" => {
                if let Ok(value) = text.trim().parse::<i64>() {
                    let units = attr("units").unwrap_or_else(|| "minutes".into());
                    self.set("length", json!({ "value": value, "units": units }));
                }
            }
            b"review" if !text.is_empty() => {
                let mut review = Map::new();
                review.insert("content".into(), json!(text));
                for key in ["type", "source", "reviewer"] {
                    if let Some(v) = attr(key) {
                        review.insert(key.into(), json!(v));
                    }
                }
                self.reviews.push(Json::Object(review));
            }
            b"image" if !text.is_empty() => {
                let mut image = Map::new();
                image.insert("url".into(), json!(text));
                for key in ["type", "size", "orient", "system"] {
                    if let Some(v) = attr(key) {
                        image.insert(key.into(), json!(v));
                    }
                }
                self.images.push(Json::Object(image));
            }
            b"previously-shown" | b"premiere" | b"new" | b"live" | b"last-chance" => {
                self.flag(name, attrs, &text);
            }
            _ => {}
        }
    }

    /// Flags arrive self-closing far more often than not.
    fn empty_element(&mut self, element: &BytesStart<'_>) {
        let name = element.local_name().as_ref().to_vec();
        let attrs = collect_attributes(element);
        let attr = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        match name.as_slice() {
            b"icon" => {
                self.icon = self
                    .icon
                    .take()
                    .or_else(|| attr("src").filter(|v| !v.is_empty()))
            }
            // `<subtitles type="onscreen"/>` with no language is common enough
            // to be worth keeping; the grouped path never sees it.
            b"subtitles" => {
                if let Some(t) = attr("type").filter(|v| !v.is_empty()) {
                    self.subtitles.push(json!({ "type": t }));
                }
            }
            _ => self.simple_element(&name, &attrs, String::new()),
        }
    }

    fn flag(&mut self, name: &[u8], attrs: &[(String, String)], text: &str) {
        let key = String::from_utf8_lossy(name).replace('-', "_");
        self.set(&key, json!(true));
        if !text.is_empty() && matches!(name, b"premiere" | b"last-chance") {
            self.set(&format!("{key}_text"), json!(text));
        }
        if name == b"previously-shown" {
            let mut details = Map::new();
            for k in ["start", "channel"] {
                if let Some((_, v)) = attrs.iter().find(|(n, _)| n == k) {
                    details.insert(k.into(), json!(v));
                }
            }
            if !details.is_empty() {
                self.set("previously_shown_details", Json::Object(details));
            }
        }
    }

    fn episode_num(&mut self, system: &str, text: &str) {
        match system {
            "xmltv_ns" => {
                // Zero-based in the file, one-based everywhere a human sees it.
                let mut parts = text.split('.');
                if let Some(season) = parts.next().and_then(|p| p.trim().parse::<i64>().ok()) {
                    self.set("season", json!(season + 1));
                }
                if let Some(episode) = parts.next().and_then(|p| p.trim().parse::<i64>().ok()) {
                    self.set("episode", json!(episode + 1));
                }
            }
            "onscreen" => {
                self.set("onscreen_episode", json!(text));
                if let Some((season, episode)) = parse_onscreen(text) {
                    self.map.entry("season").or_insert_with(|| json!(season));
                    self.map.entry("episode").or_insert_with(|| json!(episode));
                }
            }
            "dd_progid" => self.set("dd_progid", json!(text)),
            "original-air-date" => self.set("__original_air_date", json!(text)),
            "thetvdb.com" | "themoviedb.org" | "imdb.com" => {
                self.set(&format!("{system}_id"), json!(text));
            }
            _ => {}
        }
    }

    fn rating(&mut self, kind: &[u8], system: Option<String>, value: Option<String>) {
        let Some(value) = value else { return };
        if kind == b"rating" {
            if self.map.contains_key("rating") {
                return;
            }
            self.set("rating", json!(value));
            if let Some(system) = system {
                self.set("rating_system", json!(system));
            }
        } else {
            let mut entry = Map::new();
            entry.insert("value".into(), json!(value));
            if let Some(system) = system {
                entry.insert("system".into(), json!(system));
            }
            self.star_ratings.push(Json::Object(entry));
        }
    }

    fn grouped(&mut self, kind: &[u8], sub_type: Option<String>, parts: Vec<(Vec<u8>, String)>) {
        let wanted: &[&[u8]] = match kind {
            b"video" => &[b"present", b"colour", b"aspect", b"quality"],
            b"audio" => &[b"present", b"stereo"],
            _ => &[b"language"],
        };
        let mut group = Map::new();
        for (name, text) in parts {
            if wanted.contains(&name.as_slice()) {
                group.insert(String::from_utf8_lossy(&name).into_owned(), json!(text));
            }
        }

        if kind == b"subtitles" {
            if let Some(t) = sub_type {
                group.insert("type".into(), json!(t));
            }
            if !group.is_empty() {
                self.subtitles.push(Json::Object(group));
            }
        } else if !group.is_empty() {
            self.set(&String::from_utf8_lossy(kind), Json::Object(group));
        }
    }

    fn set(&mut self, key: &str, value: Json) {
        self.map.insert(key.to_string(), value);
    }

    fn finish(mut self) -> Json {
        let lists = [
            ("categories", self.categories),
            ("keywords", self.keywords),
            ("star_ratings", self.star_ratings),
            ("subtitles", self.subtitles),
            ("reviews", self.reviews),
            ("images", self.images),
        ];
        for (key, values) in lists {
            if !values.is_empty() {
                self.map.insert(key.into(), Json::Array(values));
            }
        }
        if !self.credits.is_empty() {
            self.map
                .insert("credits".into(), Json::Object(self.credits));
        }
        if let Some(icon) = self.icon {
            self.map.insert("icon".into(), json!(icon));
        }

        // `episode-num system="original-air-date"` is only a fallback source for
        // the previously-shown date; an explicit `previously-shown@start` wins.
        if let Some(Json::String(candidate)) = self.map.remove("__original_air_date") {
            let mut details = match self.map.remove("previously_shown_details") {
                Some(Json::Object(existing)) => existing,
                _ => Map::new(),
            };
            details.entry("start").or_insert_with(|| json!(candidate));
            self.map
                .insert("previously_shown_details".into(), Json::Object(details));
        }

        if self.map.is_empty() {
            Json::Null
        } else {
            Json::Object(self.map)
        }
    }
}

/// `20260911153000 +0000`, `20260911153000`, `20260911153000 GMT`, and the
/// truncated forms providers emit when they drop seconds or minutes, plus
/// whether a stated zone was understood.
///
/// `false` means the provider wrote a zone this parser could not resolve — a
/// name outside [`NAMED_OFFSETS`], or a numerically absurd offset — and the
/// timestamp was read as UTC anyway. That is a silent whole-provider shift, so
/// the reader counts it rather than letting it pass unremarked. An *absent*
/// zone is `true`: assuming UTC there is the documented default, not a guess
/// worth reporting on every row.
fn parse_time_checked(raw: &str) -> Option<(DateTime<Utc>, bool)> {
    let trimmed = raw.trim();
    let (digits, rest) = trimmed.split_at(
        trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len()),
    );

    // Pad `YYYYMMDDHH`, `YYYYMMDDHHMM` and friends out to full seconds, which is
    // what the DTD says shorter forms mean.
    let padded = match digits.len() {
        14 => digits.to_string(),
        8 | 10 | 12 => format!("{digits}{}", "0".repeat(14 - digits.len())),
        _ => return None,
    };
    let naive = NaiveDateTime::parse_from_str(&padded, "%Y%m%d%H%M%S").ok()?;

    match parse_offset(rest) {
        // A missing, unrecognised, or out-of-range zone means UTC. Dropping the
        // programme instead would leave a hole in the guide over what is
        // usually a provider typo.
        None => Some((Utc.from_utc_datetime(&naive), rest.trim().is_empty())),
        Some(offset) => offset
            .from_local_datetime(&naive)
            .single()
            .map(|dt| (dt.with_timezone(&Utc), true)),
    }
}

/// The zone abbreviations guides actually emit in place of a numeric offset.
///
/// Deliberately not a general timezone database: an abbreviation is ambiguous
/// (`CST` is three different zones) and resolving one to a named zone would
/// need the date to pick a DST rule. These are the unambiguous ones, in
/// seconds east of UTC.
const NAMED_OFFSETS: &[(&str, i32)] = &[
    ("BST", 3600),
    ("CDT", -5 * 3600),
    ("CEST", 2 * 3600),
    ("CET", 3600),
    ("EDT", -4 * 3600),
    ("EEST", 3 * 3600),
    ("EET", 2 * 3600),
    ("EST", -5 * 3600),
    ("GMT", 0),
    ("MDT", -6 * 3600),
    ("MST", -7 * 3600),
    ("PDT", -7 * 3600),
    ("PST", -8 * 3600),
    ("UT", 0),
    ("UTC", 0),
    ("Z", 0),
];

/// Offsets beyond this are not a real zone. The widest in use is +14:00.
const MAX_OFFSET_SECONDS: i32 = 14 * 3600;

fn parse_offset(rest: &str) -> Option<FixedOffset> {
    let rest = rest.trim();
    let sign = match rest.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => {
            let upper = rest.to_ascii_uppercase();
            let seconds = NAMED_OFFSETS
                .iter()
                .find(|(name, _)| *name == upper)
                .map(|(_, seconds)| *seconds)?;
            return FixedOffset::east_opt(seconds);
        }
    };
    let digits: Vec<i32> = rest[1..]
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| i32::from(b - b'0'))
        .collect();
    let (hours, minutes) = match digits.as_slice() {
        [h1, h2, m1, m2] => (h1 * 10 + h2, m1 * 10 + m2),
        [h1, h2] => (h1 * 10 + h2, 0),
        _ => return None,
    };
    let seconds = sign * (hours * 3600 + minutes * 60);
    if seconds.abs() > MAX_OFFSET_SECONDS {
        return None;
    }
    FixedOffset::east_opt(seconds)
}

/// `S01E02`, `S1 E2`, case-insensitively.
fn parse_onscreen(text: &str) -> Option<(i64, i64)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].eq_ignore_ascii_case(&b'S') {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let season_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j == season_start {
            i += 1;
            continue;
        }
        let season: i64 = text[season_start..j].parse().ok()?;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || !bytes[j].eq_ignore_ascii_case(&b'E') {
            i += 1;
            continue;
        }
        j += 1;
        let episode_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j == episode_start {
            i += 1;
            continue;
        }
        return Some((season, text[episode_start..j].parse().ok()?));
    }
    None
}

/// Resolve entities, falling back to the raw text when one cannot be resolved.
///
/// `quick-xml` returns `Err` for the whole node on a single unknown entity or a
/// bare `&`, and discarding the node on that basis loses an entire correct
/// sentence over one stray character. Keeping the raw text leaves a visible
/// `&copy;` in a title, which a user can see and report; a blank title is
/// indistinguishable from a provider that sent nothing.
fn decode_text(text: &BytesText<'_>) -> String {
    let decoded = text
        .unescape_with(entities::lookup)
        .map(Cow::into_owned)
        .unwrap_or_else(|_| String::from_utf8_lossy(text.as_ref()).into_owned());
    strip_forbidden(&decoded)
}

fn decode_attribute(value: &Attribute<'_>) -> String {
    let decoded = value
        .unescape_value_with(entities::lookup)
        .map(Cow::into_owned)
        .unwrap_or_else(|_| String::from_utf8_lossy(&value.value).into_owned());
    strip_forbidden(decoded.trim())
}

/// Drop characters XML 1.0 cannot represent at all, not even as a numeric
/// reference. `quick-xml` passes them through; a conformant parser downstream
/// rejects the *entire document* over one of them, so a single stray byte in one
/// programme would cost the whole guide.
fn strip_forbidden(text: &str) -> String {
    if !text.chars().any(crate::output::is_forbidden_xml_char) {
        return text.to_string();
    }
    text.chars()
        .filter(|c| !crate::output::is_forbidden_xml_char(*c))
        .collect()
}

fn attribute(element: &BytesStart<'_>, key: &[u8]) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == key)
        .map(|a| decode_attribute(&a))
}

fn collect_attributes(element: &BytesStart<'_>) -> Vec<(String, String)> {
    element
        .attributes()
        .flatten()
        .map(|a| {
            let key = String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned();
            (key, decode_attribute(&a))
        })
        .collect()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(xml: &str) -> Vec<XmltvItem> {
        from_bytes(xml.as_bytes())
            .unwrap()
            .map(|i| i.unwrap())
            .collect()
    }

    fn programmes(xml: &str) -> Vec<ParsedProgramme> {
        items(xml)
            .into_iter()
            .filter_map(|item| match item {
                XmltvItem::Programme(p) => Some(p),
                XmltvItem::Channel(_) => None,
            })
            .collect()
    }

    fn channels(xml: &str) -> Vec<ParsedChannel> {
        items(xml)
            .into_iter()
            .filter_map(|item| match item {
                XmltvItem::Channel(c) => Some(c),
                XmltvItem::Programme(_) => None,
            })
            .collect()
    }

    fn one_programme(body: &str) -> ParsedProgramme {
        let xml = format!(
            "<tv><programme start=\"20260911153000 +0000\" stop=\"20260911160000 +0000\" \
             channel=\"a.us\">{body}</programme></tv>"
        );
        programmes(&xml).remove(0)
    }

    fn props(body: &str) -> Json {
        one_programme(body).custom_properties
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn parses_channels_and_programmes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<tv generator-info-name="x">
  <channel id="a.us">
    <display-name>Channel A</display-name>
    <display-name>Ignored Alias</display-name>
    <icon src="http://l/a.png" />
  </channel>
  <programme start="20260911153000 +0000" stop="20260911160000 +0000" channel="a.us">
    <title>Show</title>
    <sub-title>Pilot</sub-title>
    <desc>A description.</desc>
  </programme>
</tv>"#;
        assert_eq!(
            channels(xml),
            [ParsedChannel {
                tvg_id: "a.us".into(),
                display_name: Some("Channel A".into()),
                icon_url: Some("http://l/a.png".into()),
            }]
        );
        let p = &programmes(xml)[0];
        assert_eq!(p.tvg_id, "a.us");
        assert_eq!(p.start_time, utc("2026-09-11T15:30:00Z"));
        assert_eq!(p.end_time, utc("2026-09-11T16:00:00Z"));
        assert_eq!(p.title, "Show");
        assert_eq!(p.sub_title.as_deref(), Some("Pilot"));
        assert_eq!(p.description.as_deref(), Some("A description."));
        assert_eq!(p.custom_properties, Json::Null);
    }

    #[test]
    fn channel_icon_as_a_start_tag_and_nested_noise() {
        let xml = r#"<tv><channel id="a"><display-name/><url>http://x</url>
            <icon src="http://l/a.png"><dimension w="1"/></icon></channel></tv>"#;
        assert_eq!(
            items(xml),
            [XmltvItem::Channel(ParsedChannel {
                tvg_id: "a".into(),
                display_name: None,
                icon_url: Some("http://l/a.png".into()),
            })]
        );
    }

    #[test]
    fn channels_without_an_id_are_skipped_and_counted() {
        let mut reader =
            from_bytes(br#"<tv><channel><display-name>X</display-name></channel></tv>"#).unwrap();
        assert!(reader.next_item().unwrap().is_none());
        assert_eq!(reader.skipped(), 1);
    }

    #[test]
    fn programmes_missing_required_fields_are_skipped() {
        let cases = [
            r#"<programme stop="20260911160000" channel="a"><title>T</title></programme>"#,
            r#"<programme start="20260911153000" channel="a"><title>T</title></programme>"#,
            r#"<programme start="20260911153000" stop="20260911160000"><title>T</title></programme>"#,
            r#"<programme start="nonsense" stop="20260911160000" channel="a">
               <title>T</title></programme>"#,
            r#"<programme start="20260911153000" stop="nonsense" channel="a">
               <title>T</title></programme>"#,
            // Self-closing: no children, so nothing worth a row either way.
            r#"<programme start="20260911153000" stop="20260911160000" channel="a"/>"#,
        ];
        for case in cases {
            let mut reader = from_bytes(format!("<tv>{case}</tv>").as_bytes()).unwrap();
            assert!(reader.next_item().unwrap().is_none(), "{case}");
        }
    }

    #[test]
    fn a_programme_with_no_title_keeps_its_slot() {
        let p = one_programme("<desc>Only a description</desc>");
        assert_eq!(p.title, "");
        assert_eq!(p.description.as_deref(), Some("Only a description"));
    }

    #[test]
    fn timestamp_variants() {
        let cases: &[(&str, Option<&str>)] = &[
            ("20260911153000 +0000", Some("2026-09-11T15:30:00Z")),
            ("20260911153000", Some("2026-09-11T15:30:00Z")),
            ("20260911153000 -0500", Some("2026-09-11T20:30:00Z")),
            ("20260911153000 +0530", Some("2026-09-11T10:00:00Z")),
            ("20260911153000+0200", Some("2026-09-11T13:30:00Z")),
            ("20260911153000 +02", Some("2026-09-11T13:30:00Z")),
            ("  20260911153000 +0000  ", Some("2026-09-11T15:30:00Z")),
            ("20260911153000 GMT", Some("2026-09-11T15:30:00Z")),
            ("202609111530", Some("2026-09-11T15:30:00Z")),
            ("2026091115", Some("2026-09-11T15:00:00Z")),
            ("20260911", Some("2026-09-11T00:00:00Z")),
            // An out-of-range offset degrades to UTC rather than losing the row.
            ("20260911153000 +9999", Some("2026-09-11T15:30:00Z")),
            ("20260911153000 +1500", Some("2026-09-11T15:30:00Z")),
            ("20260911153000 +1400", Some("2026-09-11T01:30:00Z")),
            ("20260911153000 -1400", Some("2026-09-12T05:30:00Z")),
            // Named zones providers actually emit.
            ("20260911153000 EST", Some("2026-09-11T20:30:00Z")),
            ("20260911153000 est", Some("2026-09-11T20:30:00Z")),
            ("20260911153000 PDT", Some("2026-09-11T22:30:00Z")),
            ("20260911153000 CEST", Some("2026-09-11T13:30:00Z")),
            ("20260911153000 UTC", Some("2026-09-11T15:30:00Z")),
            ("20260911153000 Z", Some("2026-09-11T15:30:00Z")),
            // An abbreviation with no unambiguous offset falls back to UTC.
            ("20260911153000 IST", Some("2026-09-11T15:30:00Z")),
            ("2026091115300", None),
            ("20261311153000", None),
            ("20260911253000", None),
            ("", None),
            ("not a time", None),
            ("20260911153000 +00000", Some("2026-09-11T15:30:00Z")),
        ];
        for (raw, want) in cases {
            let parsed = parse_time_checked(raw).map(|(time, _)| time);
            assert_eq!(parsed, want.map(utc), "{raw}");
        }
    }

    #[test]
    fn timestamps_with_an_unresolvable_zone_are_counted() {
        // A whole-provider time shift is invisible in the data, so the only
        // signal a user can act on is this count.
        let body = |start: &str| {
            format!(
                r#"<tv><programme start="{start}" stop="{start}" channel="a">
                   <title>T</title></programme></tv>"#
            )
        };

        let cases: &[(&str, usize)] = &[
            ("20260911153000 +0000", 0),
            ("20260911153000 EST", 0),
            // No zone stated: the documented UTC default, not a degradation.
            ("20260911153000", 0),
            // Stated but unresolvable: both the start and the stop count.
            ("20260911153000 IST", 2),
            ("20260911153000 +9999", 2),
        ];
        for (start, want) in cases {
            let mut reader = from_bytes(body(start).as_bytes()).unwrap();
            while reader.next_item().unwrap().is_some() {}
            assert_eq!(reader.unrecognised_zones(), *want, "{start}");
            assert_eq!(reader.skipped(), 0, "{start}");
        }
    }

    #[test]
    fn categories_and_keywords() {
        let got = props(
            "<title>T</title><category>News</category><category>Live</category>\
             <category>  </category><keyword>breaking</keyword>",
        );
        assert_eq!(got["categories"], json!(["News", "Live"]));
        assert_eq!(got["keywords"], json!(["breaking"]));
    }

    #[test]
    fn episode_num_systems() {
        let cases: &[(&str, Json)] = &[
            (
                r#"<episode-num system="xmltv_ns">2.5.</episode-num>"#,
                json!({"season": 3, "episode": 6}),
            ),
            (
                r#"<episode-num system="xmltv_ns">. 4 .</episode-num>"#,
                json!({"episode": 5}),
            ),
            (
                r#"<episode-num system="xmltv_ns">2.</episode-num>"#,
                json!({"season": 3}),
            ),
            (
                r#"<episode-num system="onscreen">S02E06</episode-num>"#,
                json!({"onscreen_episode": "S02E06", "season": 2, "episode": 6}),
            ),
            (
                r#"<episode-num>s3 e12</episode-num>"#,
                json!({"onscreen_episode": "s3 e12", "season": 3, "episode": 12}),
            ),
            (
                r#"<episode-num system="onscreen">Part 2</episode-num>"#,
                json!({"onscreen_episode": "Part 2"}),
            ),
            (
                r#"<episode-num system="dd_progid">EP01.0002</episode-num>"#,
                json!({"dd_progid": "EP01.0002"}),
            ),
            (
                r#"<episode-num system="thetvdb.com">series/12</episode-num>"#,
                json!({"thetvdb.com_id": "series/12"}),
            ),
            (
                r#"<episode-num system="unknown.example">x</episode-num>"#,
                Json::Null,
            ),
            // xmltv_ns wins; the onscreen text is still kept.
            (
                r#"<episode-num system="xmltv_ns">0.0.</episode-num>
                   <episode-num system="onscreen">S09E09</episode-num>"#,
                json!({"season": 1, "episode": 1, "onscreen_episode": "S09E09"}),
            ),
        ];
        for (body, want) in cases {
            assert_eq!(props(&format!("<title>T</title>{body}")), *want, "{body}");
        }
    }

    #[test]
    fn onscreen_parser_rejects_near_misses() {
        let cases: &[(&str, Option<(i64, i64)>)] = &[
            ("S1E2", Some((1, 2))),
            ("Season S12E34 end", Some((12, 34))),
            ("SE1", None),
            ("S12", None),
            ("S12X3", None),
            ("S12E", None),
            ("", None),
            ("no digits", None),
            ("S99999999999999999999E1", None),
            ("S1E99999999999999999999", None),
        ];
        for (text, want) in cases {
            assert_eq!(parse_onscreen(text), *want, "{text}");
        }
    }

    #[test]
    fn ratings_and_star_ratings() {
        let got = props(
            r#"<title>T</title>
               <rating system="MPAA"><value>PG-13</value></rating>
               <rating system="Ignored"><value>R</value></rating>
               <star-rating system="tv.com"><value>4/5</value></star-rating>
               <star-rating><value>8/10</value></star-rating>
               <star-rating><nothing/></star-rating>"#,
        );
        assert_eq!(got["rating"], json!("PG-13"));
        assert_eq!(got["rating_system"], json!("MPAA"));
        assert_eq!(
            got["star_ratings"],
            json!([{"value": "4/5", "system": "tv.com"}, {"value": "8/10"}])
        );

        let bare = props("<title>T</title><rating><value>TV-14</value></rating>");
        assert_eq!(bare["rating"], json!("TV-14"));
        assert!(bare.get("rating_system").is_none());
    }

    #[test]
    fn credits_roles_and_actors() {
        let got = props(
            r#"<title>T</title><credits>
                 <director>Dee</director>
                 <writer>Wanda</writer><writer>Walt</writer>
                 <actor role="Lead" guest="yes">Ann</actor>
                 <actor>Bob</actor>
                 <actor></actor>
               </credits>"#,
        );
        assert_eq!(
            got["credits"],
            json!({
                "director": ["Dee"],
                "writer": ["Wanda", "Walt"],
                "actor": [{"name": "Ann", "role": "Lead", "guest": true}, {"name": "Bob"}],
            })
        );
    }

    #[test]
    fn video_audio_and_subtitles() {
        let got = props(
            r#"<title>T</title>
               <video><present>yes</present><colour>yes</colour><unknown>x</unknown></video>
               <audio><stereo>stereo</stereo></audio>
               <subtitles type="teletext"><language>English</language></subtitles>
               <subtitles type="onscreen"/>
               <subtitles/>
               <subtitles type=""/>
               <subtitles><nothing/></subtitles>"#,
        );
        assert_eq!(got["video"], json!({"present": "yes", "colour": "yes"}));
        assert_eq!(got["audio"], json!({"stereo": "stereo"}));
        assert_eq!(
            got["subtitles"],
            json!([{"language": "English", "type": "teletext"}, {"type": "onscreen"}])
        );

        let empty = props("<title>T</title><video><unknown>x</unknown></video>");
        assert_eq!(empty, Json::Null);
    }

    #[test]
    fn scalar_metadata_and_length() {
        let got = props(
            r#"<title>T</title><date>20240101</date><country>US</country>
               <language>English</language><orig-language>French</orig-language>
               <length units="minutes">30</length><icon src="http://p/1.jpg" />"#,
        );
        assert_eq!(got["date"], json!("20240101"));
        assert_eq!(got["country"], json!("US"));
        assert_eq!(got["language"], json!("English"));
        assert_eq!(got["original_language"], json!("French"));
        assert_eq!(got["length"], json!({"value": 30, "units": "minutes"}));
        assert_eq!(got["icon"], json!("http://p/1.jpg"));

        let defaults = props("<title>T</title><length>45</length>");
        assert_eq!(defaults["length"], json!({"value": 45, "units": "minutes"}));

        let junk = props("<title>T</title><length>half an hour</length>");
        assert_eq!(junk, Json::Null);
    }

    #[test]
    fn reviews_and_images() {
        let got = props(
            r#"<title>T</title>
               <review type="text" source="x.example" reviewer="Rita">Good.</review>
               <review>Bare.</review>
               <review></review>
               <image type="poster" size="2" orient="P" system="tmdb">http://i/1.jpg</image>
               <image>http://i/2.jpg</image>
               <image/>"#,
        );
        assert_eq!(
            got["reviews"],
            json!([
                {"content": "Good.", "type": "text", "source": "x.example", "reviewer": "Rita"},
                {"content": "Bare."},
            ])
        );
        assert_eq!(
            got["images"],
            json!([
                {"url": "http://i/1.jpg", "type": "poster", "size": "2",
                 "orient": "P", "system": "tmdb"},
                {"url": "http://i/2.jpg"},
            ])
        );
    }

    #[test]
    fn boolean_flags_and_previously_shown() {
        let got = props(
            r#"<title>T</title><new /><live />
               <premiere>Series premiere</premiere><last-chance />
               <previously-shown start="20240101000000" channel="b.us" />"#,
        );
        assert_eq!(got["new"], json!(true));
        assert_eq!(got["live"], json!(true));
        assert_eq!(got["premiere"], json!(true));
        assert_eq!(got["premiere_text"], json!("Series premiere"));
        assert_eq!(got["last_chance"], json!(true));
        assert_eq!(got["previously_shown"], json!(true));
        assert_eq!(
            got["previously_shown_details"],
            json!({"start": "20240101000000", "channel": "b.us"})
        );

        // Only premiere and last-chance keep their text; the rest are pure flags.
        let with_text = props("<title>T</title><live>yes</live><new>also</new>");
        assert_eq!(with_text["live"], json!(true));
        assert_eq!(with_text["new"], json!(true));
        assert!(with_text.get("live_text").is_none());

        let bare = props("<title>T</title><previously-shown />");
        assert_eq!(bare["previously_shown"], json!(true));
        assert!(bare.get("previously_shown_details").is_none());
    }

    #[test]
    fn original_air_date_only_fills_a_missing_previously_shown_start() {
        let filled = props(
            r#"<title>T</title><episode-num system="original-air-date">2024-01-01</episode-num>"#,
        );
        assert_eq!(
            filled["previously_shown_details"],
            json!({"start": "2024-01-01"})
        );
        assert!(filled.get("__original_air_date").is_none());

        let explicit = props(
            r#"<title>T</title><previously-shown start="20200101000000" />
               <episode-num system="original-air-date">2024-01-01</episode-num>"#,
        );
        assert_eq!(
            explicit["previously_shown_details"],
            json!({"start": "20200101000000"})
        );
    }

    #[test]
    fn text_handling_covers_entities_cdata_and_stray_markup() {
        let p = one_programme(
            "<title>Tom &amp; Jerry</title>\
             <desc><![CDATA[Raw <b>markup</b> & co]]></desc>\
             <sub-title>Part <b>one</b></sub-title>",
        );
        assert_eq!(p.title, "Tom & Jerry");
        assert_eq!(p.description.as_deref(), Some("Raw <b>markup</b> & co"));
        assert_eq!(p.sub_title.as_deref(), Some("Part one"));
    }

    #[test]
    fn html_named_entities_resolve_instead_of_blanking_the_node() {
        // The failure this guards against is silent: quick-xml reports an error
        // for the whole text node, and dropping it leaves an empty title that
        // looks exactly like a provider who sent nothing.
        let p = one_programme(
            "<title>Caf&eacute; Central</title>\
             <sub-title>Season&nbsp;2 &mdash; Part &#8212; Two</sub-title>\
             <desc>&copy; 2026 Acme &amp; Co. 100&deg;</desc>",
        );
        assert_eq!(p.title, "Caf\u{e9} Central");
        assert_eq!(
            p.sub_title.as_deref(),
            Some("Season\u{a0}2 \u{2014} Part \u{2014} Two")
        );
        assert_eq!(
            p.description.as_deref(),
            Some("\u{a9} 2026 Acme & Co. 100\u{b0}")
        );
    }

    #[test]
    fn an_unresolvable_entity_keeps_the_raw_text_rather_than_blanking_it() {
        let cases: &[(&str, &str)] = &[
            // A name in no table at all.
            ("A &madeupentity; B", "A &madeupentity; B"),
            // A bare ampersand, which is simply not well-formed.
            ("Acme & Co", "Acme & Co"),
            // An unterminated reference.
            ("100&deg", "100&deg"),
        ];
        for (raw, want) in cases {
            let p = one_programme(&format!("<title>{raw}</title>"));
            assert_eq!(p.title, *want, "{raw}");
        }
    }

    #[test]
    fn entities_resolve_in_attributes_too() {
        let channel = channels(
            r#"<tv><channel id="caf&eacute;.fr"><display-name>X</display-name>
               <icon src="http://l/a.png?x=1&amp;y=2" /></channel></tv>"#,
        )
        .remove(0);
        assert_eq!(channel.tvg_id, "caf\u{e9}.fr");
        assert_eq!(channel.icon_url.as_deref(), Some("http://l/a.png?x=1&y=2"));

        // An unresolvable entity in an attribute keeps the raw value.
        let raw =
            channels(r#"<tv><channel id="a&nope;b"><display-name>X</display-name></channel></tv>"#)
                .remove(0);
        assert_eq!(raw.tvg_id, "a&nope;b");
    }

    #[test]
    fn characters_xml_cannot_carry_are_stripped_at_ingest() {
        // quick-xml passes these through; a conformant parser downstream would
        // reject the entire document, so they must not reach a `Program` row.
        let p = one_programme(
            "<title>Bel\u{7}l</title><desc>a\u{0}b\u{1f}c</desc>\
             <category>N\u{c}ews</category>",
        );
        assert_eq!(p.title, "Bell");
        assert_eq!(p.description.as_deref(), Some("abc"));
        assert_eq!(p.custom_properties["categories"], json!(["News"]));

        // Tab, newline and carriage return are legal and must survive.
        let kept = one_programme("<desc>line one\nline two\ttabbed</desc><title>T</title>");
        assert_eq!(
            kept.description.as_deref(),
            Some("line one\nline two\ttabbed")
        );

        let attr =
            channels("<tv><channel id=\"a\u{7}b\"><display-name>X</display-name></channel></tv>")
                .remove(0);
        assert_eq!(attr.tvg_id, "ab");
    }

    #[test]
    fn first_value_wins_for_repeated_elements() {
        let p = one_programme(
            "<title>First</title><title>Second</title>\
             <sub-title>Sub A</sub-title><sub-title>Sub B</sub-title>\
             <desc>Desc A</desc><desc>Desc B</desc>\
             <icon src=\"http://a\" /><icon src=\"http://b\" />",
        );
        assert_eq!(p.title, "First");
        assert_eq!(p.sub_title.as_deref(), Some("Sub A"));
        assert_eq!(p.description.as_deref(), Some("Desc A"));
        assert_eq!(p.custom_properties["icon"], json!("http://a"));
    }

    #[test]
    fn empty_elements_do_not_become_fields() {
        let p = one_programme("<title></title><sub-title/><desc/><icon/>");
        assert_eq!(p.title, "");
        assert!(p.sub_title.is_none());
        assert!(p.description.is_none());
        assert_eq!(p.custom_properties, Json::Null);
    }

    #[test]
    fn empty_and_truncated_documents() {
        assert!(items("").is_empty());
        assert!(items("<tv></tv>").is_empty());
        assert!(items(r#"<?xml version="1.0"?><tv/>"#).is_empty());

        // Cut mid-element: what was already read is still yielded.
        let truncated = items(
            r#"<tv><channel id="a"><display-name>A</display-name></channel>
               <programme start="20260911153000" stop="20260911160000" channel="a">
               <title>Half"#,
        );
        assert_eq!(truncated.len(), 2);
    }

    #[test]
    fn unbalanced_tags_do_not_abort_the_document() {
        let got = items(
            r#"<tv><channel id="a"><display-name>A</display-name></wrong></channel>
               <channel id="b"><display-name>B</display-name></channel></tv>"#,
        );
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn nested_markup_inside_every_element_reader_is_stepped_over() {
        // Each of these puts an unexpected child element one level deeper than
        // the reader expects, which is where guides most often go off-format.
        let p = one_programme(
            r#"<title>T<b><i>x</i></b></title>
               <rating system="MPAA"><extra><deep/></extra><value>PG</value></rating>
               <video><present>yes</present><extra><deep/></extra></video>
               <credits><extra><deep/></extra><director>Dee</director></credits>
               <icon src="http://i/1.jpg"><dimension w="1"/></icon>"#,
        );
        assert_eq!(p.title, "Tx");
        assert_eq!(p.custom_properties["rating"], json!("PG"));
        assert_eq!(p.custom_properties["video"], json!({"present": "yes"}));
        assert_eq!(p.custom_properties["credits"], json!({"director": ["Dee"]}));
        assert_eq!(p.custom_properties["icon"], json!("http://i/1.jpg"));

        // The same, one level deeper inside a `<programme>` child that is not a
        // recognised element at all.
        let unknown = one_programme("<title>T</title><junk><nested><deeper/></nested></junk>");
        assert_eq!(unknown.title, "T");
    }

    #[test]
    fn comments_and_processing_instructions_inside_text_are_dropped() {
        let p = one_programme("<title>A<!-- note -->B</title><desc>C<?pi x?>D</desc>");
        assert_eq!(p.title, "AB");
        assert_eq!(p.description.as_deref(), Some("CD"));
    }

    #[test]
    fn truncation_inside_each_element_reader_ends_cleanly() {
        // Every one of these is cut mid-element; the parser must stop at EOF
        // rather than spin, and must still yield whatever it had.
        let cases = [
            "<tv><channel id=\"a\"><display-name>A",
            "<tv><channel id=\"a\"><other><deeper>",
            "<tv><programme start=\"20260911153000\" stop=\"20260911160000\" channel=\"a\">",
            "<tv><programme start=\"20260911153000\" stop=\"20260911160000\" channel=\"a\">\
             <title>T</title><rating><value>PG",
            "<tv><programme start=\"20260911153000\" stop=\"20260911160000\" channel=\"a\">\
             <title>T</title><video><present>yes",
            "<tv><programme start=\"20260911153000\" stop=\"20260911160000\" channel=\"a\">\
             <title>T</title><credits><director>Dee",
            "<tv><programme start=\"20260911153000\" stop=\"20260911160000\" channel=\"a\">\
             <title>T</title><junk><deeper>",
        ];
        for case in cases {
            assert_eq!(items(case).len(), 1, "{case}");
        }
    }

    #[test]
    fn a_programme_icon_written_as_a_start_tag_is_still_read() {
        let p = one_programme("<title>T</title><icon src=\"http://i/1.jpg\"></icon>");
        assert_eq!(p.custom_properties["icon"], json!("http://i/1.jpg"));
    }

    #[test]
    fn an_unrecoverable_error_inside_any_element_propagates() {
        // An unterminated comment is unrecoverable wherever it appears, so it
        // reaches every element reader's error path in turn.
        let programme = r#"<programme start="20260911153000" stop="20260911160000" channel="a">"#;
        let cases = [
            "<tv><channel id=\"a\"><!-- x".to_string(),
            "<tv><channel id=\"a\"><display-name><!-- x".to_string(),
            format!("<tv>{programme}<!-- x"),
            format!("<tv>{programme}<title><!-- x"),
            format!("<tv>{programme}<sub-title><!-- x"),
            format!("<tv>{programme}<desc><!-- x"),
            format!("<tv>{programme}<icon src=\"x\"><!-- x"),
            format!("<tv>{programme}<credits><!-- x"),
            format!("<tv>{programme}<credits><director><!-- x"),
            format!("<tv>{programme}<rating><!-- x"),
            format!("<tv>{programme}<rating><value><!-- x"),
            format!("<tv>{programme}<video><!-- x"),
            format!("<tv>{programme}<video><present><!-- x"),
            format!("<tv>{programme}<category><!-- x"),
        ];
        for case in cases {
            let mut reader = from_bytes(case.as_bytes()).unwrap();
            assert!(reader.next_item().is_err(), "{case}");
        }
    }

    #[test]
    fn markup_that_cannot_be_recovered_is_an_error() {
        // An unterminated comment swallows the rest of the file, so there is no
        // partial result to salvage.
        let mut reader = from_bytes(b"<tv><!-- never ends").unwrap();
        let err = reader.next_item().unwrap_err();
        assert!(err.to_string().starts_with("xmltv:"), "{err}");

        let mut unterminated = from_bytes(b"<tv><channel id=\"a").unwrap();
        assert!(unterminated.next_item().is_err());
    }

    #[test]
    fn non_utf8_bytes_do_not_abort_the_document() {
        // 0xE9 is Latin-1 'e-acute'; quick-xml surfaces it as a decode failure
        // for that one text node, and the rest of the guide still parses.
        let xml = b"<tv><channel id=\"a\"><display-name>Caf\xe9</display-name></channel>\
                    <channel id=\"b\"><display-name>B</display-name></channel></tv>";
        let got: Vec<_> = from_bytes(xml).unwrap().collect();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn reads_compressed_input() {
        use std::io::Write;
        let xml = r#"<tv><channel id="a"><display-name>A</display-name></channel></tv>"#;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(xml.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();

        let from_slice: Vec<_> = from_bytes(&gz).unwrap().map(|i| i.unwrap()).collect();
        let from_stream: Vec<_> = from_reader(Cursor::new(gz.clone()))
            .unwrap()
            .map(|i| i.unwrap())
            .collect();
        assert_eq!(from_slice, from_stream);
        assert_eq!(from_slice.len(), 1);

        // `from_bytes` decompresses eagerly, so a corrupt payload fails there.
        assert!(from_bytes(&[0x1f, 0x8b, 0x00]).is_err());
        // `from_reader` is lazy for gzip, so the same corruption surfaces on
        // first read; xz is decoded up front, so it fails at construction.
        let mut lazy = from_reader(Cursor::new(vec![0x1f, 0x8b, 0x00])).unwrap();
        assert!(lazy.next_item().is_err());
        let truncated_xz = vec![0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04];
        assert!(from_reader(Cursor::new(truncated_xz)).map(|_| ()).is_err());
    }

    #[test]
    fn converts_into_domain_rows() {
        let xml = r#"<tv><channel id="a.us"><display-name>A</display-name></channel>
            <programme start="20260911153000" stop="20260911160000" channel="a.us">
            <title>Show</title></programme></tv>"#;
        let channel = channels(xml).remove(0);
        let programme = programmes(xml).remove(0);

        let epg = channel.into_epg_data(7, Some(3));
        assert_eq!(
            (epg.id, epg.epg_source_id, epg.name),
            (7, Some(3), "A".into())
        );
        assert_eq!(epg.tvg_id.as_deref(), Some("a.us"));

        let program = programme.into_program(11, 7);
        assert_eq!((program.id, program.epg_data_id), (11, 7));
        assert_eq!(program.tvg_id.as_deref(), Some("a.us"));
        assert_eq!(program.title, "Show");
    }

    #[test]
    fn a_channel_without_a_display_name_falls_back_to_its_id() {
        let channel = ParsedChannel {
            tvg_id: "a.us".into(),
            display_name: None,
            icon_url: None,
        };
        assert_eq!(channel.into_epg_data(1, None).name, "a.us");
    }
}
