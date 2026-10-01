//! Reader for `pg_dump`'s custom-format archive, and for the Dispatcharr
//! backup zip that carries one.
//!
//! A Dispatcharr backup is `database.dump` — `pg_dump -Fc` output — plus a
//! small `metadata.json` declaring the format version, which must be
//! [`KNOWN_BACKUP_VERSION`]. Reading it here is what lets the backup zip be the
//! importer's only input: the alternative is telling a migrating user to
//! install PostgreSQL and `pg_restore` into it, and a throwaway database stood
//! up by hand is the step of a migration most likely to be got wrong — the one
//! step whose failure mode is a half-restored instance.
//!
//! The format is a binary TOC followed by one data block per table, each block
//! a chunked zlib stream whose plaintext is a `COPY ... FROM stdin` body. It is
//! read here exactly as `pg_backup_archiver.c` and `pg_backup_custom.c` write
//! it, and nothing here interprets a value: everything comes out as the text
//! `COPY` wrote, which is what keeps this a parser.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use crate::{Error, Result};

const MAGIC: &[u8] = b"PGDMP";

/// `archFormat` for the custom archive, as opposed to tar or directory.
const FORMAT_CUSTOM: u8 = 1;

/// Versions this reader understands. 1.16 is what PostgreSQL 17 writes and the
/// floor is well below anything a Dispatcharr install can have been dumped by.
/// The bound is checked rather than assumed because every field below is
/// positional, so a version that added one would silently shift the rest.
const MIN_MINOR: u8 = 12;
const MAX_MINOR: u8 = 16;

const BLK_DATA: u8 = 1;
const BLK_BLOBS: u8 = 3;

const OFFSET_NOT_SET: u8 = 1;
const OFFSET_SET: u8 = 2;
const OFFSET_NO_DATA: u8 = 3;

/// A parsed archive: the TOC, and the data block belonging to each table.
pub struct Archive {
    version: (u8, u8, u8),
    compression: Compression,
    tables: BTreeMap<String, Table>,
}

struct Table {
    columns: Arc<[String]>,
    /// This table's data block with its chunk framing removed: one zlib
    /// stream, or the `COPY` body verbatim when the dump is uncompressed.
    data: Vec<u8>,
}

/// One `COPY` line, keyed by the column list from the `COPY` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRow {
    columns: Arc<[String]>,
    values: Vec<Option<String>>,
}

impl TextRow {
    /// `None` when the dump has no such column, `Some(None)` when the value is
    /// `\N`. The caller needs those apart: a missing column is a schema that
    /// moved under us, a NULL is data.
    pub fn get(&self, column: &str) -> Option<Option<&str>> {
        let index = self.columns.iter().position(|c| c == column)?;
        Some(self.values[index].as_deref())
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Zlib,
}

impl Archive {
    pub fn parse(bytes: &[u8]) -> Result<Archive> {
        let mut reader = Reader::new(bytes);

        if reader.take(MAGIC.len())? != MAGIC {
            return Err(Error::invalid(
                "not a pg_dump archive: the file does not start with PGDMP",
            ));
        }

        let version = (reader.byte()?, reader.byte()?, reader.byte()?);
        if version.0 != 1 || !(MIN_MINOR..=MAX_MINOR).contains(&version.1) {
            return Err(Error::invalid(format!(
                "pg_dump archive version {}.{} is not supported; this reader handles 1.{MIN_MINOR} \
                 through 1.{MAX_MINOR}",
                version.0, version.1
            )));
        }

        reader.int_size = usize::from(reader.byte()?);
        if reader.int_size == 0 || reader.int_size > 8 {
            return Err(Error::invalid(format!(
                "pg_dump archive declares {}-byte integers, which this reader cannot represent",
                reader.int_size
            )));
        }
        reader.offset_size = usize::from(reader.byte()?);

        let format = reader.byte()?;
        if format != FORMAT_CUSTOM {
            return Err(Error::invalid(format!(
                "pg_dump archive format {format} is not the custom format; re-take the dump with \
                 `pg_dump -Fc`"
            )));
        }

        let compression = if version.1 >= 15 {
            // 1.15 replaced the zlib compression *level* with an algorithm id,
            // because lz4 and zstd became possible.
            match reader.byte()? {
                0 => Compression::None,
                1 => Compression::Zlib,
                2 => return Err(unsupported_compression("lz4")),
                3 => return Err(unsupported_compression("zstd")),
                other => {
                    return Err(Error::invalid(format!(
                        "pg_dump archive uses compression algorithm {other}, which this reader \
                         does not know"
                    )));
                }
            }
        } else {
            // Negative means "the default", which was always zlib.
            match reader.int()? {
                0 => Compression::None,
                _ => Compression::Zlib,
            }
        };

        // Creation date, as seven ints, then the identity of the source.
        for _ in 0..7 {
            reader.int()?;
        }
        reader.string()?;
        reader.string()?;
        reader.string()?;

        let entries = read_toc(&mut reader, version)?;
        let mut blocks = read_data_blocks(&mut reader)?;

        let mut tables = BTreeMap::new();
        for entry in &entries {
            // Offsets are written only when pg_dump's output was seekable, so
            // their absence is normal (`pg_dump -Fc > file`). When they are
            // present they are a free check that the sequential walk above
            // stayed in step with the framing — the failure this catches is
            // one table's bytes being read as another's.
            if entry.data_state == OFFSET_SET {
                match blocks.get(&entry.dump_id) {
                    Some(block) if block.position == entry.data_offset => {}
                    Some(block) => {
                        return Err(Error::invalid(format!(
                            "pg_dump archive is inconsistent: the TOC puts block {} at offset {}, \
                             but it was found at {}",
                            entry.dump_id, entry.data_offset, block.position
                        )));
                    }
                    None => {
                        return Err(Error::invalid(format!(
                            "pg_dump archive is truncated: the TOC declares a data block for \
                             `{}` that the file does not contain",
                            entry.tag
                        )));
                    }
                }
            }

            if entry.description != "TABLE DATA" {
                continue;
            }

            let statement = entry.copy_statement.as_deref().unwrap_or_default();
            let Some((name, columns)) = parse_copy_statement(statement) else {
                return Err(Error::invalid(format!(
                    "the data for `{}` is not a COPY body; a dump taken with --inserts cannot be \
                     read here",
                    entry.tag
                )));
            };

            let Some(block) = blocks.remove(&entry.dump_id) else {
                return Err(Error::invalid(format!(
                    "pg_dump archive is truncated: it has no data block for `{name}`"
                )));
            };

            let table = Table {
                columns: columns.into(),
                data: block.data,
            };
            if tables.insert(name.clone(), table).is_some() {
                return Err(Error::invalid(format!(
                    "the dump contains `{name}` in more than one schema, so a table name does not \
                     identify a table"
                )));
            }
        }

        Ok(Archive {
            version,
            compression,
            tables,
        })
    }

    /// `1.16`, say. Only interesting when reporting what was read.
    pub fn version(&self) -> String {
        let (major, minor, revision) = self.version;
        format!("{major}.{minor}.{revision}")
    }

    pub fn tables(&self) -> impl Iterator<Item = &str> {
        self.tables.keys().map(String::as_str)
    }

    /// The column list from the table's `COPY` statement, in `COPY` order.
    pub fn columns(&self, table: &str) -> Option<&[String]> {
        self.tables.get(table).map(|table| &*table.columns)
    }

    pub fn rows(&self, table: &str) -> Result<Vec<TextRow>> {
        let Some(entry) = self.tables.get(table) else {
            return Err(Error::invalid(format!(
                "the dump contains no table `{table}`"
            )));
        };

        match self.compression {
            Compression::None => decode_copy(table, &entry.data, &entry.columns),
            Compression::Zlib => {
                let mut plain = Vec::new();
                flate2::read::ZlibDecoder::new(&entry.data[..])
                    .read_to_end(&mut plain)
                    .map_err(|e| {
                        Error::invalid(format!("the data for `{table}` will not decompress: {e}"))
                    })?;
                decode_copy(table, &plain, &entry.columns)
            }
        }
    }
}

impl std::fmt::Debug for Archive {
    // The data blocks are hundreds of kilobytes of compressed bytes, and what
    // anyone reading a failure wants is what the archive claims to be.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archive")
            .field("version", &self.version())
            .field("compression", &self.compression)
            .field("tables", &self.tables.keys().collect::<Vec<_>>())
            .finish()
    }
}

fn unsupported_compression(algorithm: &str) -> Error {
    Error::invalid(format!(
        "the dump is {algorithm}-compressed, which this reader does not support; take a fresh \
         backup from Dispatcharr's Settings -> Backups, which writes zlib, or re-dump it with \
         `pg_dump -Fc -Z gzip`"
    ))
}

/// The only backup format version this build imports.
///
/// It is the *container* version — what the zip holds and what it is called —
/// not Dispatcharr's own version, which a backup does not state anywhere.
///
/// Anything else is **refused**, before a single row is read. The importer is
/// the one component that runs once, against someone's only copy of their
/// data, irreversibly, so the failure that matters is not a refusal: it is a
/// half-right instance that imports cleanly, gets curated on top of, and turns
/// out weeks later to have dropped a column nobody checked. A backup this
/// build has never seen is not something to guess at.
///
/// When Dispatcharr ships a new format, this is the constant to branch on:
/// raise it once the new shape is understood, or read both.
pub const KNOWN_BACKUP_VERSION: i64 = 2;

/// Default name of the dump inside a backup zip, when the metadata does not
/// say.
const DEFAULT_DATABASE_FILE: &str = "database.dump";

/// What a backup zip's `metadata.json` says about itself.
///
/// Every field is optional: this describes the backup rather than the data, so
/// a field that is missing or newly spelled must not stop an import that would
/// otherwise work.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackupMetadata {
    /// `dispatcharr-backup` on every backup seen so far.
    pub format: Option<String>,
    /// The container version. See [`KNOWN_BACKUP_VERSION`].
    pub version: Option<i64>,
    pub database_type: Option<String>,
    /// Which entry in the zip holds the dump. Honoured rather than assumed, so
    /// a future backup that renames it still reads.
    pub database_file: Option<String>,
    pub created_at: Option<String>,
}

/// A Dispatcharr backup: the archive, and whatever the zip said about itself.
#[derive(Debug)]
pub struct Backup {
    pub archive: Archive,
    /// `metadata.json`, when the input was a backup zip rather than a bare
    /// `database.dump`.
    pub metadata: Option<BackupMetadata>,
}

impl Backup {
    /// Read either a `dispatcharr-backup-*.zip` or the `database.dump` inside
    /// one. Which it is, is decided by the bytes rather than by the file name:
    /// a backup that was unzipped and re-named still imports.
    pub fn read(bytes: &[u8]) -> Result<Backup> {
        if bytes.starts_with(MAGIC) {
            return Ok(Backup {
                archive: Archive::parse(bytes)?,
                metadata: None,
            });
        }

        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
            Error::invalid(format!(
                "not a Dispatcharr backup: it is neither a pg_dump archive nor a zip ({e})"
            ))
        })?;

        // Read first, because it names the entry the dump is in. Absent or
        // unparseable metadata is not fatal: it describes the backup, and the
        // dump is the backup.
        let metadata: Option<BackupMetadata> = zip
            .by_name("metadata.json")
            .ok()
            .and_then(|entry| serde_json::from_reader(std::io::BufReader::new(entry)).ok());

        // Checked before the dump is read, so an unsupported backup costs
        // nothing and cannot half-import. A bare `database.dump` carries no
        // metadata and no version, and stays readable: there is nothing to
        // disagree with.
        if let Some(version) = metadata.as_ref().and_then(|m| m.version)
            && version != KNOWN_BACKUP_VERSION
        {
            return Err(Error::invalid(format!(
                "this backup declares format version {version}; this build imports version \
                 {KNOWN_BACKUP_VERSION} only. Importing it would risk a partial migration from a \
                 layout that has never been read here, so it is refused rather than guessed at."
            )));
        }

        let file = metadata
            .as_ref()
            .and_then(|m| m.database_file.as_deref())
            .unwrap_or(DEFAULT_DATABASE_FILE);

        let mut dump = Vec::new();
        zip.by_name(file)
            .map_err(|e| {
                Error::invalid(format!(
                    "the backup zip has no `{file}` ({e}); only a backup carrying a PostgreSQL \
                     dump can be imported"
                ))
            })?
            .read_to_end(&mut dump)
            .map_err(|e| Error::invalid(format!("reading `{file}` from the backup: {e}")))?;

        Ok(Backup {
            archive: Archive::parse(&dump)?,
            metadata,
        })
    }
}

struct TocEntry {
    dump_id: i64,
    tag: String,
    description: String,
    copy_statement: Option<String>,
    data_state: u8,
    data_offset: u64,
}

fn read_toc(reader: &mut Reader<'_>, version: (u8, u8, u8)) -> Result<Vec<TocEntry>> {
    let count = reader.int()?;
    if count < 0 {
        return Err(Error::invalid("pg_dump archive declares a negative TOC"));
    }

    let mut entries = Vec::new();
    for _ in 0..count {
        let dump_id = reader.int()?;
        reader.int()?; // hadDumper
        reader.string()?; // tableoid
        reader.string()?; // oid
        let tag = reader.string()?.unwrap_or_default();
        let description = reader.string()?.unwrap_or_default();
        reader.int()?; // section, 1.11 and up, which is every version here
        reader.string()?; // defn
        reader.string()?; // dropStmt
        let copy_statement = reader.string()?;
        reader.string()?; // namespace
        reader.string()?; // tablespace
        if version.1 >= 14 {
            reader.string()?; // tableam
        }
        if version.1 >= 16 {
            reader.int()?; // relkind
        }
        reader.string()?; // owner
        reader.string()?; // withOids, still written as the text "false"

        while reader.string()?.is_some() {} // dependencies, NULL-terminated

        let (data_state, data_offset) = reader.data_offset()?;

        entries.push(TocEntry {
            dump_id,
            tag,
            description,
            copy_statement,
            data_state,
            data_offset,
        });
    }
    Ok(entries)
}

struct DataBlock {
    /// Where the block's type byte sits, for the TOC cross-check.
    position: u64,
    data: Vec<u8>,
}

/// Walk the data blocks in file order.
///
/// Sequential rather than seeking to the TOC's offsets, because a dump written
/// to a pipe has no offsets at all — and walking means the offsets that *are*
/// there become a check rather than the only way in.
fn read_data_blocks(reader: &mut Reader<'_>) -> Result<BTreeMap<i64, DataBlock>> {
    let mut blocks = BTreeMap::new();
    while !reader.at_end() {
        let position = reader.position();
        let kind = reader.byte()?;
        let dump_id = reader.int()?;
        match kind {
            BLK_DATA => {
                let data = reader.chunks()?;
                blocks.insert(dump_id, DataBlock { position, data });
            }
            // Large objects. Out of scope — Dispatcharr stores none — but they
            // have to be walked past to reach the blocks that follow.
            BLK_BLOBS => {
                blocks.insert(
                    dump_id,
                    DataBlock {
                        position,
                        data: Vec::new(),
                    },
                );
                loop {
                    if reader.int()? == 0 {
                        break;
                    }
                    reader.chunks()?;
                }
            }
            other => {
                return Err(Error::invalid(format!(
                    "unknown data block type {other} at offset {position} in the pg_dump archive"
                )));
            }
        }
    }
    Ok(blocks)
}

/// `COPY public.core_useragent (id, name) FROM stdin;` -> the table's own name
/// and its columns, both unquoted.
fn parse_copy_statement(statement: &str) -> Option<(String, Vec<String>)> {
    let rest = statement.strip_prefix("COPY ")?;

    let open = find_outside_quotes(rest, '(')?;
    let close = find_outside_quotes(&rest[open + 1..], ')')? + open + 1;

    // Qualified names only ever have a schema in front, and the schema is the
    // one part a caller here never wants: the importer asks for a table.
    let qualified = rest[..open].trim();
    let name = match find_last_outside_quotes(qualified, '.') {
        Some(dot) => unquote(&qualified[dot + 1..]),
        None => unquote(qualified),
    };
    if name.is_empty() {
        return None;
    }

    let columns = split_outside_quotes(&rest[open + 1..close], ',')
        .into_iter()
        .map(|column| unquote(column.trim()))
        .collect();
    Some((name, columns))
}

fn find_outside_quotes(text: &str, needle: char) -> Option<usize> {
    let mut quoted = false;
    for (index, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c == needle && !quoted => return Some(index),
            _ => {}
        }
    }
    None
}

fn find_last_outside_quotes(text: &str, needle: char) -> Option<usize> {
    let mut quoted = false;
    let mut found = None;
    for (index, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c == needle && !quoted => found = Some(index),
            _ => {}
        }
    }
    found
}

fn split_outside_quotes(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (index, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c == separator && !quoted => {
                parts.push(&text[start..index]);
                start = index + c.len_utf8();
            }
            _ => {}
        }
    }
    if !text[start..].trim().is_empty() || !parts.is_empty() {
        parts.push(&text[start..]);
    }
    parts
}

fn unquote(identifier: &str) -> String {
    match identifier
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        Some(inner) => inner.replace("\"\"", "\""),
        None => identifier.to_owned(),
    }
}

/// Decode a `COPY ... FROM stdin` body: tab-separated fields, newline-separated
/// rows, terminated by a line holding `\.`.
fn decode_copy(table: &str, body: &[u8], columns: &Arc<[String]>) -> Result<Vec<TextRow>> {
    let mut rows = Vec::new();
    for (index, line) in body.split(|&b| b == b'\n').enumerate() {
        // pg_dump writes "\\.\n\n\n", so there is always something after the
        // terminator and it is never a row.
        if line == br"\." {
            return Ok(rows);
        }
        if line.is_empty() {
            continue;
        }

        let fields: Vec<&[u8]> = line.split(|&b| b == b'\t').collect();
        if fields.len() != columns.len() {
            return Err(Error::invalid(format!(
                "`{table}` line {} has {} fields where the COPY statement names {}",
                index + 1,
                fields.len(),
                columns.len()
            )));
        }

        let mut values = Vec::with_capacity(fields.len());
        for (field, column) in fields.iter().zip(columns.iter()) {
            values.push(decode_field(table, column, field)?);
        }
        rows.push(TextRow {
            columns: Arc::clone(columns),
            values,
        });
    }

    Err(Error::invalid(format!(
        "the COPY data for `{table}` ends without its `\\.` terminator, so the dump is truncated"
    )))
}

fn decode_field(table: &str, column: &str, field: &[u8]) -> Result<Option<String>> {
    // Tested against the raw field, before unescaping, exactly as COPY does:
    // that is what keeps a value of the literal two characters `\N` — written
    // `\\N` — distinguishable from NULL.
    if field == br"\N" {
        return Ok(None);
    }

    let decoded = unescape(field);
    String::from_utf8(decoded)
        .map(Some)
        .map_err(|_| Error::invalid(format!("`{table}`.`{column}` is not valid UTF-8")))
}

/// COPY's text-format escapes, per `CopyReadAttributesText`.
///
/// Octal and hex escapes yield *bytes*, which is why this works on bytes: two
/// of them can be one character, and rejecting each half as invalid UTF-8
/// would corrupt a value that is perfectly well formed.
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        let byte = field[index];
        index += 1;
        if byte != b'\\' || index == field.len() {
            out.push(byte);
            continue;
        }

        let escape = field[index];
        index += 1;
        match escape {
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'0'..=b'7' => {
                let mut value = u32::from(escape - b'0');
                for _ in 0..2 {
                    match field.get(index) {
                        Some(digit @ b'0'..=b'7') => {
                            value = (value << 3) | u32::from(digit - b'0');
                            index += 1;
                        }
                        _ => break,
                    }
                }
                out.push(value as u8);
            }
            b'x' if field.get(index).is_some_and(u8::is_ascii_hexdigit) => {
                let mut value = 0u32;
                for _ in 0..2 {
                    match field.get(index) {
                        Some(digit) if digit.is_ascii_hexdigit() => {
                            value = (value << 4) | hex_value(*digit);
                            index += 1;
                        }
                        _ => break,
                    }
                }
                out.push(value as u8);
            }
            // Including `\\`, `\.` and `\x` with no digits after it: COPY's
            // rule for an unrecognised escape is the character itself.
            other => out.push(other),
        }
    }
    out
}

fn hex_value(digit: u8) -> u32 {
    u32::from(match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        _ => digit - b'A' + 10,
    })
}

/// The archive's primitives: a sign-prefixed little-endian integer, a
/// length-prefixed string, and the chunked framing around a data block.
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
    int_size: usize,
    offset_size: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 0,
            // Until the header says otherwise. Nothing is read with these.
            int_size: 4,
            offset_size: 8,
        }
    }

    fn at_end(&self) -> bool {
        self.position >= self.bytes.len()
    }

    fn position(&self) -> u64 {
        self.position as u64
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(count).ok_or_else(truncated)?;
        let slice = self.bytes.get(self.position..end).ok_or_else(truncated)?;
        self.position = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn int(&mut self) -> Result<i64> {
        let sign = self.byte()?;
        let mut value: i64 = 0;
        for (shift, byte) in self.take(self.int_size)?.iter().enumerate() {
            value |= i64::from(*byte) << (8 * shift);
        }
        Ok(if sign != 0 { -value } else { value })
    }

    fn string(&mut self) -> Result<Option<String>> {
        let length = self.int()?;
        if length < 0 {
            return Ok(None);
        }
        let bytes = self.take(usize::try_from(length).map_err(|_| truncated())?)?;
        String::from_utf8(bytes.to_vec())
            .map(Some)
            .map_err(|_| Error::invalid("a pg_dump archive header string is not valid UTF-8"))
    }

    fn data_offset(&mut self) -> Result<(u8, u64)> {
        let state = self.byte()?;
        if !matches!(state, OFFSET_NOT_SET | OFFSET_SET | OFFSET_NO_DATA) {
            return Err(Error::invalid(format!(
                "unexpected data offset flag {state} in the pg_dump archive"
            )));
        }
        let mut offset: u64 = 0;
        for (shift, byte) in self.take(self.offset_size)?.iter().enumerate() {
            if shift < 8 {
                offset |= u64::from(*byte) << (8 * shift);
            } else if *byte != 0 {
                return Err(Error::invalid(
                    "a file offset in the pg_dump archive is too large to represent",
                ));
            }
        }
        Ok((state, offset))
    }

    /// A data block: length-prefixed chunks until a zero length. The chunks are
    /// one stream, not one per chunk, so they are concatenated.
    fn chunks(&mut self) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            let length = self.int()?;
            if length == 0 {
                return Ok(data);
            }
            let length = usize::try_from(length).map_err(|_| truncated())?;
            data.extend_from_slice(self.take(length)?);
        }
    }
}

fn truncated() -> Error {
    Error::invalid("the pg_dump archive ends mid-record, so the file is truncated")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// Writes the archive as `pg_backup_archiver.c` does, so the reader is
    /// tested against the format rather than against one pg_dump build's
    /// output. Everything defaults to 1.16 with zlib, the shape PostgreSQL 17
    /// produces and the one a Dispatcharr backup is in.
    struct Builder {
        out: Vec<u8>,
        minor: u8,
    }

    impl Builder {
        fn new(minor: u8, compression: u8) -> Self {
            let mut builder = Self {
                out: Vec::new(),
                minor,
            };
            builder.out.extend_from_slice(MAGIC);
            builder
                .out
                .extend_from_slice(&[1, minor, 0, 4, 8, FORMAT_CUSTOM]);
            if minor >= 15 {
                builder.out.push(compression);
            } else {
                builder.int(i64::from(compression));
            }
            for _ in 0..7 {
                builder.int(0);
            }
            builder.string(Some("dispatcharr"));
            builder.string(Some("17.11"));
            builder.string(Some("17.11"));
            builder
        }

        fn byte(&mut self, byte: u8) {
            self.out.push(byte);
        }

        fn int(&mut self, value: i64) {
            let (sign, mut magnitude) = if value < 0 {
                (1u8, -value)
            } else {
                (0u8, value)
            };
            self.out.push(sign);
            for _ in 0..4 {
                self.out.push((magnitude & 0xFF) as u8);
                magnitude >>= 8;
            }
        }

        fn string(&mut self, value: Option<&str>) {
            match value {
                None => self.int(-1),
                Some(text) => {
                    self.int(text.len() as i64);
                    self.out.extend_from_slice(text.as_bytes());
                }
            }
        }

        fn toc_entry(&mut self, dump_id: i64, description: &str, copy: Option<&str>, state: u8) {
            self.int(dump_id);
            self.int(if copy.is_some() { 1 } else { 0 });
            self.string(Some("1259"));
            self.string(Some("16400"));
            self.string(Some("a_table"));
            self.string(Some(description));
            self.int(1);
            self.string(Some("CREATE TABLE ..."));
            self.string(Some("DROP TABLE ..."));
            self.string(copy);
            self.string(Some("public"));
            self.string(Some(""));
            if self.minor >= 14 {
                self.string(Some(""));
            }
            if self.minor >= 16 {
                self.int(114);
            }
            self.string(Some("dispatch"));
            self.string(Some("false"));
            self.string(Some("2"));
            self.string(None);
            self.byte(state);
            // Filled in by `patch_offset` once the block has been written.
            self.out.extend_from_slice(&[0u8; 8]);
        }

        /// Where the last entry's offset field starts, so a test can pin it.
        fn last_offset_field(&self) -> usize {
            self.out.len() - 8
        }

        fn patch_offset(&mut self, at: usize, offset: u64) {
            self.out[at..at + 8].copy_from_slice(&offset.to_le_bytes());
        }

        fn data_block(&mut self, dump_id: i64, payload: &[u8], compressed: bool) -> u64 {
            let position = self.out.len() as u64;
            self.byte(BLK_DATA);
            self.int(dump_id);

            let body = if compressed {
                let mut encoder =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(payload).unwrap();
                encoder.finish().unwrap()
            } else {
                payload.to_vec()
            };

            // Deliberately split: real archives chunk at 4 kB, and a reader
            // that decompresses each chunk on its own passes a one-chunk test.
            for chunk in body.chunks(7) {
                self.int(chunk.len() as i64);
                self.out.extend_from_slice(chunk);
            }
            self.int(0);
            position
        }

        fn blobs_block(&mut self, dump_id: i64) -> u64 {
            let position = self.out.len() as u64;
            self.byte(BLK_BLOBS);
            self.int(dump_id);
            for oid in [17001i64, 17002] {
                self.int(oid);
                self.int(4);
                self.out.extend_from_slice(b"blob");
                self.int(0);
            }
            self.int(0);
            position
        }
    }

    const COPY: &str = "COPY public.core_useragent (id, name, note) FROM stdin;\n";

    /// Every escape COPY can emit or accept, plus a NULL and an empty string.
    const BODY: &str = concat!(
        "1\tVLC\tone\\ttab and a \\nnewline\n",
        "2\tPlex\t\\\\ \\b \\f \\r \\v \\101\\102 \\x41\\x7a \\q \\.\n",
        "3\t\t\\N\n",
        "4\tnon-ascii\téñ — \\303\\251\n",
        "\\.\n\n\n",
    );

    fn sample_archive() -> Vec<u8> {
        let mut builder = Builder::new(16, 1);
        builder.int(3); // three TOC entries
        builder.toc_entry(1, "TABLE", None, OFFSET_NO_DATA);
        builder.toc_entry(2, "TABLE DATA", Some(COPY), OFFSET_NOT_SET);
        builder.toc_entry(3, "BLOBS", None, OFFSET_NOT_SET);
        builder.blobs_block(3);
        builder.data_block(2, BODY.as_bytes(), true);
        builder.out
    }

    #[test]
    fn a_hand_built_archive_reads_back_exactly() {
        let archive = Archive::parse(&sample_archive()).unwrap();

        assert_eq!(archive.version(), "1.16.0");
        assert_eq!(archive.tables().collect::<Vec<_>>(), ["core_useragent"]);
        assert_eq!(
            archive.columns("core_useragent").unwrap(),
            ["id", "name", "note"]
        );
        assert!(archive.columns("nothing").is_none());

        let rows = archive.rows("core_useragent").unwrap();
        assert_eq!(rows.len(), 4);

        assert_eq!(rows[0].get("id"), Some(Some("1")));
        assert_eq!(rows[0].get("name"), Some(Some("VLC")));
        assert_eq!(
            rows[0].get("note"),
            Some(Some("one\ttab and a \nnewline")),
            "the escapes a programme description is full of"
        );

        assert_eq!(
            rows[1].get("note"),
            Some(Some("\\ \u{8} \u{c} \r \u{b} AB Az q ."))
        );

        // An empty string and a NULL are different values, and for half the
        // columns the importer reads the difference is the whole question.
        assert_eq!(rows[2].get("name"), Some(Some("")));
        assert_eq!(rows[2].get("note"), Some(None));

        assert_eq!(rows[3].get("note"), Some(Some("éñ — é")));

        // A column the dump does not have, which the importer must be able to
        // tell from a NULL.
        assert_eq!(rows[3].get("missing"), None);
        assert_eq!(rows[3].columns(), ["id", "name", "note"]);
    }

    #[test]
    fn an_uncompressed_archive_reads_the_same() {
        let mut builder = Builder::new(16, 0);
        builder.int(1);
        builder.toc_entry(2, "TABLE DATA", Some(COPY), OFFSET_NOT_SET);
        builder.data_block(2, BODY.as_bytes(), false);

        let archive = Archive::parse(&builder.out).unwrap();
        let rows = archive.rows("core_useragent").unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].get("name"), Some(Some("VLC")));
    }

    #[test]
    fn a_pre_1_15_archive_reads_its_compression_as_a_level() {
        for (minor, level) in [(12u8, 0u8), (13, 9), (14, 6)] {
            let mut builder = Builder::new(minor, level);
            builder.int(1);
            builder.toc_entry(2, "TABLE DATA", Some(COPY), OFFSET_NOT_SET);
            builder.data_block(2, BODY.as_bytes(), level != 0);

            let archive = Archive::parse(&builder.out).unwrap();
            assert_eq!(archive.version(), format!("1.{minor}.0"));
            assert_eq!(
                archive.rows("core_useragent").unwrap().len(),
                4,
                "1.{minor}"
            );
        }
    }

    #[test]
    fn the_offsets_in_the_toc_are_checked_against_the_walk() {
        let mut builder = Builder::new(16, 1);
        builder.int(1);
        builder.toc_entry(2, "TABLE DATA", Some(COPY), OFFSET_SET);
        let field = builder.last_offset_field();
        let position = builder.data_block(2, BODY.as_bytes(), true);

        let mut good = builder.out.clone();
        good[field..field + 8].copy_from_slice(&position.to_le_bytes());
        assert_eq!(
            Archive::parse(&good)
                .unwrap()
                .rows("core_useragent")
                .unwrap()
                .len(),
            4
        );

        // A wrong offset means the sequential walk and the TOC disagree about
        // where a block starts, which is one table's bytes being read as
        // another's. Louder than a warning, because this is a migration.
        builder.patch_offset(field, position + 1);
        let error = Archive::parse(&builder.out).unwrap_err().to_string();
        assert!(error.contains("inconsistent"), "{error}");

        // The TOC promising a block the file does not contain is the same
        // failure seen from the other side.
        let mut truncated = builder.out.clone();
        truncated.truncate(position as usize);
        let error = Archive::parse(&truncated).unwrap_err().to_string();
        assert!(error.contains("truncated"), "{error}");
    }

    #[test]
    fn an_unreadable_archive_says_what_is_wrong_with_it() {
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (b"not a dump at all".to_vec(), "PGDMP"),
            (b"PGD".to_vec(), "truncated"),
        ];
        for (bytes, expected) in cases {
            let error = Archive::parse(&bytes).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }

        // Versions on either side of the supported range, named in the error.
        for minor in [11u8, 17] {
            let mut bytes = MAGIC.to_vec();
            bytes.extend_from_slice(&[1, minor, 0, 4, 8, FORMAT_CUSTOM, 1]);
            let error = Archive::parse(&bytes).unwrap_err().to_string();
            assert!(error.contains(&format!("1.{minor}")), "{error}");
            assert!(error.contains("1.12 through 1.16"), "{error}");
        }

        // A tar or directory archive, which is a different reader entirely.
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&[1, 16, 0, 4, 8, 3, 1]);
        let error = Archive::parse(&bytes).unwrap_err().to_string();
        assert!(error.contains("pg_dump -Fc"), "{error}");

        // Integers this reader cannot hold.
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&[1, 16, 0, 16, 8, FORMAT_CUSTOM, 1]);
        let error = Archive::parse(&bytes).unwrap_err().to_string();
        assert!(error.contains("16-byte integers"), "{error}");
    }

    #[test]
    fn the_compressions_this_reader_cannot_read_are_named() {
        for (algorithm, name) in [(2u8, "lz4"), (3, "zstd")] {
            let mut bytes = MAGIC.to_vec();
            bytes.extend_from_slice(&[1, 16, 0, 4, 8, FORMAT_CUSTOM, algorithm]);
            let error = Archive::parse(&bytes).unwrap_err().to_string();
            assert!(error.contains(name), "{error}");
            // The message has to say what to do instead, because the only
            // input this accepts is a backup zip.
            assert!(error.contains("Settings -> Backups"), "{error}");
        }

        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&[1, 16, 0, 4, 8, FORMAT_CUSTOM, 7]);
        let error = Archive::parse(&bytes).unwrap_err().to_string();
        assert!(error.contains("compression algorithm 7"), "{error}");
    }

    #[test]
    fn a_data_block_that_is_not_copy_text_is_refused() {
        // `--inserts` puts SQL where the COPY body would be, and reading it as
        // one would produce rows that look plausible and are not.
        let mut builder = Builder::new(16, 1);
        builder.int(1);
        builder.toc_entry(
            2,
            "TABLE DATA",
            Some("INSERT INTO x VALUES (1);\n"),
            OFFSET_NOT_SET,
        );
        builder.data_block(2, b"INSERT INTO x VALUES (1);\n", true);

        let error = Archive::parse(&builder.out).unwrap_err().to_string();
        assert!(error.contains("--inserts"), "{error}");
    }

    #[test]
    fn a_body_that_stops_before_its_terminator_is_refused() {
        let mut builder = Builder::new(16, 1);
        builder.int(1);
        builder.toc_entry(2, "TABLE DATA", Some(COPY), OFFSET_NOT_SET);
        builder.data_block(2, b"1\tVLC\tnote\n", true);

        let archive = Archive::parse(&builder.out).unwrap();
        let error = archive.rows("core_useragent").unwrap_err().to_string();
        assert!(error.contains("terminator"), "{error}");
    }

    #[test]
    fn a_row_with_the_wrong_number_of_fields_is_refused() {
        let mut builder = Builder::new(16, 1);
        builder.int(1);
        builder.toc_entry(2, "TABLE DATA", Some(COPY), OFFSET_NOT_SET);
        builder.data_block(2, b"1\tVLC\n\\.\n", true);

        let archive = Archive::parse(&builder.out).unwrap();
        let error = archive.rows("core_useragent").unwrap_err().to_string();
        assert!(
            error.contains("2 fields where the COPY statement names 3"),
            "{error}"
        );
    }

    #[test]
    fn asking_for_a_table_the_dump_does_not_have_says_so() {
        let archive = Archive::parse(&sample_archive()).unwrap();
        let error = archive.rows("accounts_user").unwrap_err().to_string();
        assert!(error.contains("no table `accounts_user`"), "{error}");
    }

    #[test]
    fn corrupt_framing_is_refused_rather_than_guessed_at() {
        let mut builder = Builder::new(16, 1);
        builder.int(1);
        builder.toc_entry(2, "TABLE DATA", Some(COPY), OFFSET_NOT_SET);
        builder.byte(9); // not BLK_DATA or BLK_BLOBS
        builder.int(2);
        let error = Archive::parse(&builder.out).unwrap_err().to_string();
        assert!(error.contains("unknown data block type 9"), "{error}");

        // Data that is not a zlib stream at all.
        let mut builder = Builder::new(16, 1);
        builder.int(1);
        builder.toc_entry(2, "TABLE DATA", Some(COPY), OFFSET_NOT_SET);
        builder.data_block(2, BODY.as_bytes(), false);
        let archive = Archive::parse(&builder.out).unwrap();
        let error = archive.rows("core_useragent").unwrap_err().to_string();
        assert!(error.contains("will not decompress"), "{error}");
    }

    #[test]
    fn two_schemas_holding_the_same_table_name_is_refused() {
        let mut builder = Builder::new(16, 1);
        builder.int(2);
        builder.toc_entry(2, "TABLE DATA", Some(COPY), OFFSET_NOT_SET);
        builder.toc_entry(
            3,
            "TABLE DATA",
            Some("COPY other.core_useragent (id, name, note) FROM stdin;\n"),
            OFFSET_NOT_SET,
        );
        builder.data_block(2, BODY.as_bytes(), true);
        builder.data_block(3, BODY.as_bytes(), true);

        let error = Archive::parse(&builder.out).unwrap_err().to_string();
        assert!(error.contains("more than one schema"), "{error}");
    }

    #[test]
    fn quoted_identifiers_survive_the_copy_statement() {
        let (table, columns) = parse_copy_statement(
            "COPY public.\"m3u filter\" (id, \"order\", \"a\"\"b\") FROM stdin;\n",
        )
        .unwrap();
        assert_eq!(table, "m3u filter");
        assert_eq!(columns, ["id", "order", "a\"b"]);

        // No schema qualification, and a table with no columns at all.
        let (table, columns) = parse_copy_statement("COPY plain () FROM stdin;\n").unwrap();
        assert_eq!(table, "plain");
        assert!(columns.is_empty());

        assert!(parse_copy_statement("SELECT 1").is_none());
        assert!(parse_copy_statement("COPY public.x FROM stdin;\n").is_none());
        assert!(parse_copy_statement("COPY  (id) FROM stdin;\n").is_none());
    }

    // --- The committed backup fixture ----------------------------------------

    /// `fixtures/import/dispatcharr-backup.zip`, as a user hands it over.
    fn fixture_zip() -> Vec<u8> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/import/dispatcharr-backup.zip");
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    }

    /// The `database.dump` inside it, for tests that wrap it in a zip of
    /// their own.
    fn fixture_dump() -> Vec<u8> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(fixture_zip())).unwrap();
        let mut dump = Vec::new();
        std::io::Read::read_to_end(&mut zip.by_name("database.dump").unwrap(), &mut dump).unwrap();
        dump
    }

    #[test]
    fn the_committed_dump_of_the_fixture_database_reads() {
        let archive = Archive::parse(&fixture_dump()).unwrap();
        assert_eq!(archive.version(), "1.16.0");

        // Every table the importer reads is in it.
        let tables: Vec<&str> = archive.tables().collect();
        for table in [
            "accounts_user",
            "accounts_user_channel_profiles",
            "core_coresettings",
            "core_outputprofile",
            "core_streamprofile",
            "core_useragent",
            "dispatcharr_channels_channel",
            "dispatcharr_channels_channelgroup",
            "dispatcharr_channels_channelgroupm3uaccount",
            "dispatcharr_channels_channeloverride",
            "dispatcharr_channels_channelprofile",
            "dispatcharr_channels_channelprofilemembership",
            "dispatcharr_channels_channelstream",
            "dispatcharr_channels_logo",
            "dispatcharr_channels_stream",
            "epg_epgdata",
            "epg_epgsource",
            "epg_programdata",
            "m3u_m3uaccount",
            "m3u_m3uaccountprofile",
            "m3u_m3ufilter",
            "m3u_servergroup",
        ] {
            assert!(
                tables.contains(&table),
                "{table} is missing from {tables:?}"
            );
        }

        // 12 channels, 20 streams, 17 failover links, 9 logos, 240
        // programmes: what `fixtures/import/README.md` says is in it.
        assert_eq!(
            archive.rows("dispatcharr_channels_channel").unwrap().len(),
            12
        );
        assert_eq!(
            archive.rows("dispatcharr_channels_stream").unwrap().len(),
            20
        );
        assert_eq!(
            archive
                .rows("dispatcharr_channels_channelstream")
                .unwrap()
                .len(),
            17
        );
        assert_eq!(archive.rows("dispatcharr_channels_logo").unwrap().len(), 9);
        assert_eq!(archive.rows("epg_programdata").unwrap().len(), 240);

        // A quoted column in the COPY statement, which is the one place the
        // importer's column names and the dump's differ.
        let links = archive.rows("dispatcharr_channels_channelstream").unwrap();
        assert!(
            archive
                .columns("dispatcharr_channels_channelstream")
                .unwrap()
                .contains(&"order".to_string())
        );
        assert!(links[0].get("order").unwrap().is_some());

        // A timestamptz, a jsonb and a NULL, in the shapes the importer decodes.
        let users = archive.rows("accounts_user").unwrap();
        let admin = users
            .iter()
            .find(|row| row.get("id") == Some(Some("1")))
            .unwrap();
        assert_eq!(admin.get("username"), Some(Some("fixtureadmin")));
        assert_eq!(admin.get("is_superuser"), Some(Some("t")));
        assert!(admin.get("date_joined").unwrap().unwrap().ends_with("+00"));
        assert_eq!(admin.get("custom_properties"), Some(Some("{}")));
        assert_eq!(admin.get("api_key"), Some(None));
    }

    #[test]
    fn a_backup_zip_is_read_without_being_unpacked_first() {
        let dump = fixture_dump();
        // The committed zip, as a user hands it over: every field a real
        // backup carries.
        let backup = Backup::read(&fixture_zip()).unwrap();
        assert_eq!(
            backup.metadata,
            Some(BackupMetadata {
                format: Some("dispatcharr-backup".into()),
                version: Some(KNOWN_BACKUP_VERSION),
                database_type: Some("postgresql".into()),
                database_file: Some("database.dump".into()),
                created_at: Some("2026-09-11T03:00:00.139910+00:00".into()),
            })
        );
        assert_eq!(
            backup
                .archive
                .rows("dispatcharr_channels_channel")
                .unwrap()
                .len(),
            12
        );

        // The bare dump, for an operator who already unpacked it.
        let backup = Backup::read(&dump).unwrap();
        assert!(backup.metadata.is_none());
        assert_eq!(
            backup
                .archive
                .rows("dispatcharr_channels_channel")
                .unwrap()
                .len(),
            12
        );
    }

    /// A zip of the named entries, in order.
    fn zip_backup(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn the_metadata_names_the_entry_the_dump_is_in() {
        // Honoured rather than assumed: a backup that renames the dump still
        // reads. The file name here is deliberately not `database.dump`.
        let dump = fixture_dump();
        let zipped = zip_backup(&[
            ("db.pgdump", &dump),
            (
                "metadata.json",
                br#"{"version": 2, "database_file": "db.pgdump"}"#,
            ),
        ]);

        let backup = Backup::read(&zipped).unwrap();
        assert_eq!(
            backup
                .archive
                .rows("dispatcharr_channels_channel")
                .unwrap()
                .len(),
            12
        );
    }

    #[test]
    fn a_backup_format_this_build_has_not_seen_is_refused() {
        // Refused before the dump is read, because the alternative is a
        // partial migration from a layout nobody here has ever parsed — and
        // the importer runs once, against the only copy.
        let dump = fixture_dump();
        for version in ["1", "3", "99"] {
            let metadata = format!(r#"{{"format": "dispatcharr-backup", "version": {version}}}"#);
            let zipped = zip_backup(&[
                ("database.dump", &dump),
                ("metadata.json", metadata.as_bytes()),
            ]);
            let error = Backup::read(&zipped).unwrap_err().to_string();
            assert!(error.contains(&format!("version {version}")), "{error}");
            assert!(error.contains("version 2 only"), "{error}");
        }

        // A bare dump has no metadata to disagree with, and still reads.
        assert!(Backup::read(&dump).unwrap().metadata.is_none());

        // Neither does a zip whose metadata omits the version.
        let zipped = zip_backup(&[
            ("database.dump", &dump),
            ("metadata.json", br#"{"format": "dispatcharr-backup"}"#),
        ]);
        assert!(Backup::read(&zipped).is_ok());
    }

    #[test]
    fn metadata_that_will_not_parse_does_not_stop_the_import() {
        // It describes the backup; the dump is the backup. A future field of a
        // type this struct does not expect must not cost someone their
        // migration.
        let dump = fixture_dump();
        let zipped = zip_backup(&[
            ("database.dump", &dump),
            ("metadata.json", b"{ this is not json"),
        ]);

        let backup = Backup::read(&zipped).unwrap();
        assert!(backup.metadata.is_none());
        assert_eq!(
            backup
                .archive
                .rows("dispatcharr_channels_channel")
                .unwrap()
                .len(),
            12
        );
    }

    #[test]
    fn a_backup_that_carries_no_database_says_so() {
        // A version this build accepts, so the failure is the missing dump
        // rather than the version. A *version 1* backup carries SQLite instead
        // of a pg_dump and is refused a step earlier, on the version, with a
        // message that says why — see the test above.
        let zipped = zip_backup(&[(
            "metadata.json",
            br#"{"format": "dispatcharr-backup", "version": 2}"#,
        )]);

        let error = Backup::read(&zipped).unwrap_err().to_string();
        assert!(error.contains("no `database.dump`"), "{error}");

        let error = Backup::read(b"neither one thing nor the other")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("neither a pg_dump archive nor a zip"),
            "{error}"
        );
    }
}
