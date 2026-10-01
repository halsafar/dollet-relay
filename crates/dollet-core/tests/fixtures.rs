//! Leak scans over every fixture.
//!
//! Every fixture is edited by hand, and hand editing is the moment a real URL
//! gets pasted in from a browser. So each is checked on every run: no host
//! outside the allowlist, and no credential in a stream path but the
//! fixture's own. An allowlist rather than a search for known secrets, because
//! a denylist has to name the provider's hostname in order to look for it,
//! which puts the very string it guards against into the repository.
//!
//! `golden.rs` and `ingest.rs` scan their own corpora beside the tests that
//! read them.

mod common;

use std::path::{Path, PathBuf};

use common::hosts_referenced;
use dollet_core::parse::pgdump::{Archive, Backup};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// The Dispatcharr backup the importer is tested against, read through the
/// project's own reader: a scan that treated the zip as opaque bytes would
/// walk straight past the one binary fixture in the tree.
#[test]
fn the_backup_fixture_references_no_host_outside_the_allowlist() {
    const ALLOWED: &[&str] = &[
        "ipx.test",
        "provider.example",
        "logos.example",
        "guide.example",
        "github.com",
        "localhost",
        "127.0.0.1",
    ];

    let bytes = std::fs::read(fixtures().join("import/dispatcharr-backup.zip"))
        .expect("reading the backup fixture");
    let backup = Backup::read(&bytes).expect("the fixture is a backup this build reads");
    let text = every_value(&backup.archive);
    assert!(
        text.len() > 10_000,
        "the backup is empty — this guard would pass against nothing"
    );

    let mut stray: Vec<String> = hosts_referenced(&text)
        .into_iter()
        .filter(|host| !ALLOWED.contains(&host.as_str()))
        .collect();
    stray.sort();
    stray.dedup();
    assert_eq!(stray, Vec::<String>::new());
    assert_eq!(credentials_in_stream_paths(&text), Vec::<String>::new());
}

/// `fixtures/sample.sql` is the seed most integration tests run on, so it gets
/// the same scan as the other corpora.
#[test]
fn the_sample_instance_references_no_host_outside_the_allowlist() {
    const ALLOWED: &[&str] = &["provider.example", "logos.example", "guide.example"];

    let text = std::fs::read_to_string(fixtures().join("sample.sql"))
        .expect("reading fixtures/sample.sql");
    assert!(
        text.len() > 1000,
        "the sample is empty -- this guard would pass against nothing"
    );

    let mut stray: Vec<String> = hosts_referenced(&text)
        .into_iter()
        .filter(|host| !ALLOWED.contains(&host.as_str()))
        .collect();
    stray.sort();
    stray.dedup();
    assert_eq!(stray, Vec::<String>::new());
    assert_eq!(credentials_in_stream_paths(&text), Vec::<String>::new());
}

/// Walks subdirectories, because the provider payloads live one level down,
/// and decompresses as it goes: a hostname inside a gzip member is still a
/// hostname.
///
/// `fixtures/synthetic/` is written by hand, so the risk is a URL pasted from a
/// browser while debugging. Same allowlist as the backup, plus
/// `xtream.example`, which only the synthetic Xtream account uses.
#[test]
fn the_synthetic_corpus_references_no_host_outside_the_allowlist() {
    const ALLOWED: &[&str] = &[
        "ipx.test",
        "provider.example",
        "xtream.example",
        "logos.example",
        "guide.example",
        "github.com",
        "localhost",
        "127.0.0.1",
    ];

    let root = fixtures().join("synthetic");
    let mut files = Vec::new();
    collect_files(&root, &mut files);
    assert!(
        files.len() > 1,
        "the synthetic corpus is empty — this guard would pass against nothing"
    );

    let mut stray = Vec::new();
    for path in files {
        if path.extension().is_some_and(|e| e == "md") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let name = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();

        for text in readable_forms(&bytes) {
            for host in hosts_referenced(&text) {
                if !ALLOWED.contains(&host.as_str()) {
                    stray.push(format!("{name}: {host}"));
                }
            }
        }
    }

    stray.sort();
    stray.dedup();
    assert_eq!(stray, Vec::<String>::new());
}

/// Stream URLs are `/live/<user>/<pass>/<id>`; anything but the fixture's own
/// pair in those segments is a credential.
fn credentials_in_stream_paths(text: &str) -> Vec<String> {
    let mut leaked = Vec::new();
    for (index, _) in text.match_indices("/live/") {
        let mut parts = text[index + "/live/".len()..].split('/');
        if let (Some(user), Some(password)) = (parts.next(), parts.next())
            && (user, password) != ("fixtureuser", "fixturepass")
        {
            leaked.push(format!("/live/{user}/{password}/"));
        }
    }
    leaked.sort();
    leaked.dedup();
    leaked
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Everything in these bytes that can be read as text: the bytes themselves,
/// and whatever they decompress or transcode to.
///
/// A truncated payload is deliberately part of this corpus, so a decompression
/// that fails is not a reason to skip the file — the readable prefix is still
/// scanned.
fn readable_forms(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(text) = std::str::from_utf8(bytes) {
        out.push(text.to_owned());
    }

    match dollet_core::parse::compress::detect(bytes) {
        dollet_core::parse::compress::Encoding::Utf16 => {
            let big_endian = bytes[0] == 0xfe;
            let units: Vec<u16> = bytes[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| match big_endian {
                    true => u16::from_be_bytes(*pair),
                    false => u16::from_le_bytes(*pair),
                })
                .collect();
            out.push(String::from_utf16_lossy(&units));
        }
        dollet_core::parse::compress::Encoding::Plain => {}
        _ => {
            if let Ok(plain) = dollet_core::parse::compress::decompress(bytes) {
                out.push(String::from_utf8_lossy(&plain).into_owned());
            }
        }
    }
    out
}

/// Every value in a `pg_dump` archive, one per line.
fn every_value(archive: &Archive) -> String {
    let mut text = String::new();
    for table in archive.tables() {
        for row in archive.rows(table).expect("the committed dump decodes") {
            for column in row.columns() {
                if let Some(Some(value)) = row.get(column) {
                    text.push_str(value);
                    text.push('\n');
                }
            }
        }
    }
    text
}
