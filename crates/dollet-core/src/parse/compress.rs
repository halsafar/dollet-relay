//! Transparent decompression of provider payloads.
//!
//! The same M3U or XMLTV document is served as plain text, gzip, or xz by
//! different providers, and neither the URL extension nor the `Content-Type`
//! header can be trusted to say which. The encoding is therefore sniffed from
//! the leading magic bytes.
//!
//! Every path is bounded. An EPG or M3U URL is settings-controlled but its
//! payload is provider-controlled, and this process owns every active stream:
//! an OOM here is not a failed refresh, it is a total outage that no amount of
//! failover logic in `dollet-stream` can recover from.

use std::io::{self, BufRead, Cursor, Read, Write};

use crate::{Error, Result};

const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
const XZ_MAGIC: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0x00];
const UTF16_BE_BOM: &[u8] = &[0xfe, 0xff];
const UTF16_LE_BOM: &[u8] = &[0xff, 0xfe];

/// Enough bytes to identify the longest magic number above.
const MAGIC_LEN: usize = 6;

/// Ceiling on any decompressed payload.
///
/// Not a working-set figure — both parsers stream — but the point past which a
/// payload is assumed hostile or corrupt rather than merely large. A full
/// multi-thousand-channel XMLTV year runs a few hundred MB, so this leaves real
/// feeds room while keeping a decompression bomb from killing the process.
pub const MAX_DECOMPRESSED: u64 = 512 * 1024 * 1024;

/// Ceiling on a *compressed* xz payload, which has to be buffered whole before
/// its index can be read. xz compresses XML 20-50x, so this is far more slack
/// than [`MAX_DECOMPRESSED`] needs.
const MAX_XZ_COMPRESSED: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Plain,
    Gzip,
    Xz,
    /// Recognised only so it can be refused with a useful message.
    Utf16,
}

pub fn detect(head: &[u8]) -> Encoding {
    if head.starts_with(GZIP_MAGIC) {
        Encoding::Gzip
    } else if head.starts_with(XZ_MAGIC) {
        Encoding::Xz
    } else if head.starts_with(UTF16_BE_BOM) || head.starts_with(UTF16_LE_BOM) {
        Encoding::Utf16
    } else {
        Encoding::Plain
    }
}

/// Decompress a whole in-memory payload, or return it unchanged when plain.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    match detect(input) {
        // Plain input goes through the same cap rather than a length check, so
        // there is one bound to reason about instead of two.
        Encoding::Plain => read_capped(input),
        Encoding::Gzip => {
            // gzip's trailer declares the uncompressed size, so the common bomb
            // is refused without allocating. It is only accurate mod 2^32 and
            // describes the last member of a multi-member stream, so `Capped`
            // below stays the real bound rather than an afterthought.
            if let Some(declared) = gzip_declared_size(input) {
                oversize_check(declared)?;
            }
            read_capped(flate2::read::GzDecoder::new(input))
        }
        Encoding::Xz => xz_decompress(input),
        Encoding::Utf16 => Err(utf16_error()),
    }
}

/// Wrap a reader so the caller reads plaintext regardless of the encoding.
///
/// gzip and plain input stream through a cap that errors rather than truncates;
/// xz is buffered whole because `lzma-rs` exposes no incremental reader.
pub fn reader<R: Read + 'static>(mut input: R) -> Result<Box<dyn BufRead>> {
    let mut head = [0u8; MAGIC_LEN];
    let n = read_head(&mut input, &mut head)?;
    let head = head[..n].to_vec();
    let encoding = detect(&head);
    let rejoined = Cursor::new(head).chain(input);

    match encoding {
        Encoding::Plain => Ok(Box::new(io::BufReader::new(capped(rejoined)))),
        Encoding::Gzip => Ok(Box::new(io::BufReader::new(capped(
            flate2::read::GzDecoder::new(rejoined),
        )))),
        Encoding::Xz => {
            // The whole compressed stream is needed before its index can be
            // read, and the index is what makes the cost knowable up front.
            let mut compressed = Vec::new();
            rejoined
                .take(MAX_XZ_COMPRESSED as u64 + 1)
                .read_to_end(&mut compressed)?;
            if compressed.len() > MAX_XZ_COMPRESSED {
                return Err(Error::invalid(format!(
                    "compressed xz payload exceeds {MAX_XZ_COMPRESSED} bytes"
                )));
            }
            Ok(Box::new(Cursor::new(xz_decompress(&compressed)?)))
        }
        Encoding::Utf16 => Err(utf16_error()),
    }
}

/// UTF-16 is a documented gap rather than a silent one: both parsers assume
/// UTF-8, and a UTF-16 document parses as an empty guide with no other signal.
fn utf16_error() -> Error {
    Error::invalid("UTF-16 input is not supported; re-encode the source as UTF-8")
}

fn oversize_check(len: u64) -> Result<()> {
    if len > MAX_DECOMPRESSED {
        return Err(Error::invalid(format!(
            "decompressed payload exceeds {MAX_DECOMPRESSED} bytes"
        )));
    }
    Ok(())
}

fn capped<R: Read>(inner: R) -> Capped<R> {
    Capped {
        inner,
        budget: MAX_DECOMPRESSED + 1,
    }
}

fn read_capped<R: Read>(inner: R) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    capped(inner).read_to_end(&mut out)?;
    Ok(out)
}

/// Refuses to yield more than [`MAX_DECOMPRESSED`] bytes. Erroring rather than
/// truncating matters: a silently short guide looks like a provider that
/// dropped channels, which is the kind of bug nobody finds for months.
struct Capped<R> {
    inner: R,
    /// Counts one past the cap, so overflow is detectable rather than a
    /// legitimate payload of exactly the cap being rejected.
    budget: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.budget == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("decompressed payload exceeds {MAX_DECOMPRESSED} bytes"),
            ));
        }
        let limit = usize::try_from(self.budget)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let n = self.inner.read(&mut buf[..limit])?;
        self.budget -= n as u64;
        Ok(n)
    }
}

/// The `ISIZE` field of a gzip trailer: uncompressed length modulo 2^32.
fn gzip_declared_size(data: &[u8]) -> Option<u64> {
    let start = data.len().checked_sub(4)?;
    let trailer = &data[start..];
    Some(u64::from(u32::from_le_bytes([
        trailer[0], trailer[1], trailer[2], trailer[3],
    ])))
}

/// Decompress xz after proving from its own index that the result fits.
///
/// A counting writer would not help here: `lzma-rs` accumulates the entire
/// output in an internal buffer and only flushes to the sink at the end
/// (`decode/lzma2.rs` builds an `LzAccumBuffer` with a `usize::MAX` memory
/// limit), so by the time a sink could object the memory is already spent.
/// The xz container records every block's uncompressed size in a mandatory
/// index before the stream footer, which makes the cost knowable beforehand.
fn xz_decompress(input: &[u8]) -> Result<Vec<u8>> {
    oversize_check(xz_declared_size(input)?)?;

    let mut out = CappedWriter {
        inner: Vec::new(),
        budget: MAX_DECOMPRESSED,
    };
    let mut src = input;
    lzma_rs::xz_decompress(&mut src, &mut out)
        .map_err(|e| Error::invalid(format!("xz decode failed: {e}")))?;
    Ok(out.inner)
}

/// Total uncompressed size declared by a single-stream xz payload's index.
///
/// Concatenated streams are refused rather than summed: only the last stream's
/// index is reachable from the footer, so accepting them would mean trusting a
/// number that describes part of the file.
fn xz_declared_size(data: &[u8]) -> Result<u64> {
    let malformed = || Error::invalid("malformed xz stream");

    // 12-byte stream header, 12-byte stream footer.
    if data.len() < 24 || !data.starts_with(XZ_MAGIC) {
        return Err(malformed());
    }
    let footer = &data[data.len() - 12..];
    if &footer[10..12] != b"YZ" {
        return Err(malformed());
    }
    let backward = u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]);
    let index_size = (u64::from(backward) + 1) * 4;
    let index_start = (data.len() as u64 - 12)
        .checked_sub(index_size)
        .filter(|start| *start >= 12)
        .ok_or_else(malformed)? as usize;

    let index = &data[index_start..data.len() - 12];
    if index.first() != Some(&0x00) {
        return Err(malformed());
    }

    let mut pos = 1;
    let count = xz_varint(index, &mut pos).ok_or_else(malformed)?;
    let mut declared: u64 = 0;
    let mut blocks: u64 = 0;
    for _ in 0..count {
        let unpadded = xz_varint(index, &mut pos).ok_or_else(malformed)?;
        let uncompressed = xz_varint(index, &mut pos).ok_or_else(malformed)?;
        declared = declared.checked_add(uncompressed).ok_or_else(malformed)?;
        // Blocks are padded to a four-byte boundary. `next_multiple_of` panics
        // on overflow, and the value it is rounding comes from the file.
        blocks = unpadded
            .checked_next_multiple_of(4)
            .and_then(|padded| blocks.checked_add(padded))
            .ok_or_else(malformed)?;
    }

    // Header + blocks + index + footer must account for the file exactly, which
    // is true of a single stream and false of a concatenation.
    let accounted = 12u64
        .checked_add(blocks)
        .and_then(|v| v.checked_add(index_size))
        .and_then(|v| v.checked_add(12))
        .ok_or_else(malformed)?;
    if accounted != data.len() as u64 {
        return Err(Error::invalid(
            "multi-stream or padded xz payloads are not supported",
        ));
    }
    Ok(declared)
}

/// xz's little-endian base-128 integer. Non-minimal encodings are invalid.
fn xz_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    for i in 0..9 {
        let byte = *data.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return (i == 0 || byte != 0).then_some(value);
        }
    }
    None
}

/// Bounds the output `Vec` even if an index turns out to have lied.
struct CappedWriter {
    inner: Vec<u8>,
    budget: u64,
}

impl Write for CappedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() as u64 > self.budget {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("decompressed payload exceeds {MAX_DECOMPRESSED} bytes"),
            ));
        }
        self.budget -= buf.len() as u64;
        self.inner.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `Read::read` is allowed to return short; the magic number must not be split
/// across two calls or the payload is mis-identified as plain.
fn read_head<R: Read>(input: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    /// `lzma-rs` only decompresses, so the fixture is a checked-in byte string:
    /// `printf 'hello xz\n' | xz -c`.
    const XZ_HELLO: &[u8] = &[
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04, 0xe6, 0xd6, 0xb4, 0x46, 0x04, 0xc0, 0x0d,
        0x09, 0x21, 0x01, 0x16, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5f, 0x4f,
        0x33, 0xe4, 0x01, 0x00, 0x08, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x78, 0x7a, 0x0a, 0x00,
        0x00, 0x00, 0x00, 0xc1, 0x49, 0x3a, 0xfa, 0x63, 0x52, 0x14, 0x5a, 0x00, 0x01, 0x29, 0x09,
        0x64, 0x92, 0x1c, 0x1d, 0x1f, 0xb6, 0xf3, 0x7d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x59,
        0x5a,
    ];

    /// Assemble a payload with a structurally valid xz frame and index but no
    /// real block data. `xz_declared_size` rejects on the index alone, so an
    /// oversize claim never reaches the decoder and the fixture stays tiny.
    fn xz_claiming(block_len: usize, declared: u64) -> Vec<u8> {
        fn varint(mut value: u64, out: &mut Vec<u8>) {
            while value >= 0x80 {
                out.push((value as u8 & 0x7f) | 0x80);
                value >>= 7;
            }
            out.push(value as u8);
        }

        assert!(
            block_len.is_multiple_of(4),
            "block area must already be padded"
        );
        let mut index = vec![0x00];
        varint(1, &mut index); // one record
        varint(block_len as u64, &mut index); // unpadded size
        varint(declared, &mut index); // uncompressed size
        while !(index.len() + 4).is_multiple_of(4) {
            index.push(0x00); // index padding
        }
        index.extend_from_slice(&[0; 4]); // index CRC32, never checked here

        let mut data = Vec::new();
        data.extend_from_slice(XZ_MAGIC);
        data.extend_from_slice(&[0x00, 0x04, 0, 0, 0, 0]); // flags + header CRC32
        data.resize(12 + block_len, 0);
        data.extend_from_slice(&index);

        let backward = (index.len() / 4 - 1) as u32;
        data.extend_from_slice(&[0; 4]); // footer CRC32
        data.extend_from_slice(&backward.to_le_bytes());
        data.extend_from_slice(&[0x00, 0x04]);
        data.extend_from_slice(b"YZ");
        data
    }

    #[test]
    fn detects_encodings() {
        let cases: &[(&[u8], Encoding)] = &[
            (b"", Encoding::Plain),
            (b"#EXTM3U", Encoding::Plain),
            (&[0x1f], Encoding::Plain),
            (&[0x1f, 0x8b, 0x08], Encoding::Gzip),
            (XZ_MAGIC, Encoding::Xz),
            (&[0xfd, b'7', b'z'], Encoding::Plain),
            (&[0xfe, 0xff, b'<'], Encoding::Utf16),
            (&[0xff, 0xfe, b'<'], Encoding::Utf16),
            (&[0xfe], Encoding::Plain),
        ];
        for (input, want) in cases {
            assert_eq!(detect(input), *want, "input {input:?}");
        }
    }

    #[test]
    fn decompresses_each_encoding() {
        assert_eq!(decompress(b"plain").unwrap(), b"plain");
        assert_eq!(decompress(&gzip(b"squeezed")).unwrap(), b"squeezed");
        assert_eq!(decompress(XZ_HELLO).unwrap(), b"hello xz\n");
    }

    #[test]
    fn truncated_gzip_is_an_error() {
        let full = gzip(b"squeezed");
        let err = decompress(&full[..full.len() - 4]).unwrap_err();
        assert!(format!("{err:?}").starts_with("Io("), "{err:?}");
    }

    #[test]
    fn truncated_xz_is_an_error() {
        let err = decompress(&XZ_HELLO[..20]).unwrap_err();
        assert!(err.to_string().contains("malformed xz"), "{err}");
        // `Box<dyn BufRead>` is not `Debug`; discard it so `unwrap_err` works.
        let err = reader(Cursor::new(XZ_HELLO[..20].to_vec()))
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("malformed xz"), "{err}");
    }

    #[test]
    fn xz_index_declares_the_decompressed_size() {
        assert_eq!(xz_declared_size(XZ_HELLO).unwrap(), 9);
        assert_eq!(xz_declared_size(&xz_claiming(8, 4096)).unwrap(), 4096);
    }

    #[test]
    fn a_structurally_broken_xz_index_is_refused() {
        let cases: &[(&str, Vec<u8>)] = &[
            ("too short", vec![0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]),
            ("no footer magic", {
                let mut d = xz_claiming(8, 16);
                let n = d.len();
                d[n - 1] = b'X';
                d
            }),
            ("backward size past the header", {
                let mut d = xz_claiming(8, 16);
                let n = d.len();
                d[n - 8..n - 4].copy_from_slice(&u32::MAX.to_le_bytes());
                d
            }),
            ("index indicator not zero", {
                let mut d = xz_claiming(8, 16);
                d[20] = 0x01;
                d
            }),
            ("record varint runs off the end", {
                let mut d = xz_claiming(8, 16);
                d[21] = 0x7f; // claim 127 records
                d
            }),
        ];
        for (label, data) in cases {
            let err = xz_declared_size(data).map(|_| ()).unwrap_err();
            assert!(err.to_string().contains("malformed xz"), "{label}: {err}");
        }
    }

    /// Wrap a hand-built index in a valid frame, so `xz_declared_size` reaches
    /// the record loop with exactly these bytes.
    fn xz_with_index(index: &[u8]) -> Vec<u8> {
        let mut index = index.to_vec();
        while !index.len().is_multiple_of(4) {
            index.push(0x00);
        }
        let mut data = Vec::new();
        data.extend_from_slice(XZ_MAGIC);
        data.extend_from_slice(&[0x00, 0x04, 0, 0, 0, 0]);
        data.extend_from_slice(&index);
        let backward = (index.len() / 4 - 1) as u32;
        data.extend_from_slice(&[0; 4]);
        data.extend_from_slice(&backward.to_le_bytes());
        data.extend_from_slice(&[0x00, 0x04]);
        data.extend_from_slice(b"YZ");
        data
    }

    fn varint_bytes(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        while value >= 0x80 {
            out.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
        out
    }

    /// `(unpadded, uncompressed)` records wrapped in an index body.
    fn xz_index(records: &[(u64, u64)]) -> Vec<u8> {
        let mut index = vec![0x00];
        index.extend_from_slice(&varint_bytes(records.len() as u64));
        for (unpadded, uncompressed) in records {
            index.extend_from_slice(&varint_bytes(*unpadded));
            index.extend_from_slice(&varint_bytes(*uncompressed));
        }
        index
    }

    #[test]
    fn an_index_that_cannot_be_summed_is_refused_rather_than_panicking() {
        // The largest value a nine-byte varint can carry.
        let max = u64::MAX >> 1;
        let cases: &[(&str, Vec<u8>)] = &[
            // Block sizes that sum past u64::MAX.
            ("block total", xz_index(&[(max, 1), (max, 1), (max, 1)])),
            // Uncompressed sizes that sum past u64::MAX.
            ("declared total", xz_index(&[(4, max), (4, max), (4, max)])),
            // Blocks that fit, but not once the header and footer are added.
            ("frame total", xz_index(&[(max, 1), (max - 7, 1)])),
            // A second record whose uncompressed size runs off the end. The
            // trailing continuation bits matter: index padding is zero bytes,
            // which would otherwise parse as a perfectly valid varint.
            (
                "record cut in half",
                vec![0x00, 0x02, 0x08, 0x10, 0x08, 0x80, 0x80, 0x80],
            ),
        ];
        for (label, index) in cases {
            let err = xz_declared_size(&xz_with_index(index))
                .map(|_| ())
                .unwrap_err();
            assert!(err.to_string().contains("malformed xz"), "{label}: {err}");
        }
    }

    #[test]
    fn a_nine_byte_varint_round_trips() {
        let index = xz_index(&[(4, u64::MAX >> 1)]);
        assert_eq!(varint_bytes(u64::MAX >> 1).len(), 9);
        // Structurally sound, so it is refused for its shape, not its arithmetic.
        let err = xz_declared_size(&xz_with_index(&index))
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("multi-stream"), "{err}");
    }

    #[test]
    fn a_varint_longer_than_nine_bytes_is_refused() {
        let index = vec![
            0x00, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80,
        ];
        assert!(xz_declared_size(&xz_with_index(&index)).is_err());
    }

    #[test]
    fn a_source_that_errors_mid_xz_propagates() {
        struct MagicThenBroken(usize);
        impl Read for MagicThenBroken {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 < XZ_MAGIC.len() {
                    let n = buf.len().min(XZ_MAGIC.len() - self.0);
                    buf[..n].copy_from_slice(&XZ_MAGIC[self.0..self.0 + n]);
                    self.0 += n;
                    return Ok(n);
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset mid-stream",
                ))
            }
        }

        let err = reader(MagicThenBroken(0)).map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("reset mid-stream"), "{err}");
    }

    #[test]
    fn a_non_minimal_varint_is_refused() {
        // 0x80 0x00 encodes zero in two bytes, which xz forbids.
        let mut data = xz_claiming(8, 16);
        data[21] = 0x80;
        data[22] = 0x00;
        assert!(xz_declared_size(&data).is_err());
    }

    #[test]
    fn concatenated_xz_streams_are_refused() {
        let mut doubled = XZ_HELLO.to_vec();
        doubled.extend_from_slice(XZ_HELLO);
        let err = decompress(&doubled).unwrap_err();
        assert!(err.to_string().contains("multi-stream"), "{err}");
    }

    #[test]
    fn an_xz_index_claiming_more_than_the_cap_is_refused_before_decoding() {
        let bomb = xz_claiming(8, MAX_DECOMPRESSED + 1);
        assert!(
            bomb.len() < 64,
            "the refusal must not depend on payload size"
        );
        let err = decompress(&bomb).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");

        let err = reader(Cursor::new(bomb)).map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");

        // Exactly at the cap is a legitimate payload, so only the decode fails.
        let at_cap = xz_claiming(8, MAX_DECOMPRESSED);
        let err = decompress(&at_cap).unwrap_err();
        assert!(err.to_string().contains("xz decode failed"), "{err}");
    }

    #[test]
    fn an_oversized_compressed_xz_payload_is_refused() {
        let err = reader(Cursor::new(vec![0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]))
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("malformed xz"), "{err}");

        struct Endless(usize);
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                for (i, slot) in buf.iter_mut().enumerate() {
                    *slot = if self.0 + i < XZ_MAGIC.len() {
                        XZ_MAGIC[self.0 + i]
                    } else {
                        0
                    };
                }
                self.0 += buf.len();
                Ok(buf.len())
            }
        }

        let err = reader(Endless(0)).map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("compressed xz payload"), "{err}");
    }

    #[test]
    fn the_decompressed_cap_errors_rather_than_truncating() {
        // Exercised with a small budget: a real 512 MB fixture would cost more
        // to build than the guarantee is worth proving at that exact size.
        let mut capped = Capped {
            inner: Cursor::new(vec![b'x'; 8]),
            budget: 5,
        };
        let mut out = Vec::new();
        let err = capped.read_to_end(&mut out).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        let mut exact = Capped {
            inner: Cursor::new(vec![b'x'; 4]),
            budget: 5,
        };
        let mut out = Vec::new();
        exact.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"xxxx");

        let mut writer = CappedWriter {
            inner: Vec::new(),
            budget: 4,
        };
        writer.write_all(b"abcd").unwrap();
        writer.flush().unwrap();
        assert!(writer.write_all(b"e").is_err());
        assert_eq!(writer.inner, b"abcd");
    }

    #[test]
    fn the_oversize_predicate_is_inclusive_of_the_cap() {
        assert!(oversize_check(MAX_DECOMPRESSED).is_ok());
        let err = oversize_check(MAX_DECOMPRESSED + 1).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[test]
    fn a_gzip_trailer_claiming_more_than_the_cap_is_refused_before_inflating() {
        let mut bomb = gzip(b"small");
        let n = bomb.len();
        bomb[n - 4..].copy_from_slice(&u32::MAX.to_le_bytes());

        let started = std::time::Instant::now();
        let err = decompress(&bomb).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "the refusal must not depend on inflating anything"
        );

        assert_eq!(gzip_declared_size(b"abc"), None);
        assert_eq!(gzip_declared_size(&gzip(b"squeezed")), Some(8));
    }

    #[test]
    fn utf16_input_is_a_named_gap_rather_than_an_empty_guide() {
        let utf16 = [0xff, 0xfe, b'<', 0x00, b't', 0x00, b'v', 0x00, b'>', 0x00];
        let err = decompress(&utf16).unwrap_err();
        assert!(err.to_string().contains("UTF-16"), "{err}");

        let err = reader(Cursor::new(utf16.to_vec())).map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("UTF-16"), "{err}");
    }

    #[test]
    fn reader_handles_each_encoding() {
        let cases: Vec<Vec<u8>> = vec![
            b"hello xz\n".to_vec(),
            gzip(b"hello xz\n"),
            XZ_HELLO.to_vec(),
        ];
        for case in cases {
            let mut out = String::new();
            reader(Cursor::new(case))
                .unwrap()
                .read_to_string(&mut out)
                .unwrap();
            assert_eq!(out, "hello xz\n");
        }
    }

    #[test]
    fn reader_reassembles_a_magic_number_split_across_reads() {
        struct OneByteAtATime(Vec<u8>, usize);
        impl Read for OneByteAtATime {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match (self.0.get(self.1), buf.first_mut()) {
                    (Some(&byte), Some(slot)) => {
                        *slot = byte;
                        self.1 += 1;
                        Ok(1)
                    }
                    _ => Ok(0),
                }
            }
        }

        for payload in [gzip(b"dripped"), b"dripped".to_vec()] {
            let mut out = String::new();
            reader(OneByteAtATime(payload, 0))
                .unwrap()
                .read_to_string(&mut out)
                .unwrap();
            assert_eq!(out, "dripped");
        }
    }

    #[test]
    fn a_reader_that_errors_while_sniffing_propagates() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset",
                ))
            }
        }

        let err = reader(Broken).map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("reset"), "{err}");
    }

    #[test]
    fn reader_handles_input_shorter_than_a_magic_number() {
        let mut out = String::new();
        reader(Cursor::new(b"ab".to_vec()))
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "ab");
    }
}
