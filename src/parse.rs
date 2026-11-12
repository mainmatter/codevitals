//! The parse: every changed blob with a known language into `spans`.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use codevitals::{Incremental, Lang, Span};
use duckdb::{Connection, params};
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::db::{BATCH, SCHEMA, V, oid};

const MAX_BLOB_BYTES: usize = 1 << 20;
/// Warn about any blob whose extraction takes longer than this.
const SLOW_BLOB: Duration = Duration::from_secs(1);

/// Make `spans` complete at version `V` for every changed blob with a known language.
pub fn parse(db: &mut Connection, repo: &gix::Repository) -> Result<usize> {
    let started = Instant::now();
    let stale = db.execute("delete from spans where v <> ?", [V])?;
    // Fingerprints of blobs whose spans are gone are reparsed with them.
    db.execute(
        "delete from fingerprints where blob not in (select blob from spans)",
        [],
    )?;
    let git_dir =
        std::fs::canonicalize(repo.git_dir()).unwrap_or_else(|_| repo.git_dir().to_path_buf());
    db.execute("delete from codevitals", [])?;
    db.execute(
        "insert into codevitals values (?, ?, ?)",
        params![SCHEMA, V, git_dir.to_string_lossy()],
    )?;
    // Every path a blob ever had, in order: the blob is parsed in the language of the
    // first one with a known extension (an empty __init__.py shares its blob with
    // every other empty file).
    let paths: Vec<(String, String)> = db
        .prepare(
            "select distinct blob, path from changes
             where blob in (select blob from parse_candidates) and blob not in (select blob from spans)
             order by blob, path",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let candidates: Vec<&[(String, String)]> = paths.chunk_by(|a, b| a.0 == b.0).collect();
    let langs: HashMap<&str, (&str, Lang)> = candidates
        .iter()
        .filter_map(|paths| {
            paths
                .iter()
                .find_map(|(blob, path)| Some((blob.as_str(), (path.as_str(), Lang::of(path)?))))
        })
        .collect();
    let chains = chains(db, &langs)?;
    info!(
        blobs = langs.len(),
        chains = chains.len(),
        unknown_language = candidates.len() - langs.len(),
        stale_spans = stale,
        threads = rayon::current_num_threads(),
        listed = ?started.elapsed(),
        "parse"
    );

    // Workers read and extract chains of blobs in parallel, each with its own
    // repository handle, while `WRITERS` writers, each with its own connection, append
    // them as they arrive. A message is one whole blob, so a writer's transactions
    // never split a blob.
    let repo = repo.clone().into_sync();
    let total = langs.len();
    let connections = (0..WRITERS)
        .map(|_| db.try_clone())
        .collect::<Result<Vec<_>, _>>()?;
    let (send, recv) = sync_channel(BATCH);
    let (recv, progress) = (Mutex::new(Some(recv)), AtomicUsize::new(0));
    let (recv, progress) = (&recv, &progress);
    let written = std::thread::scope(|s| {
        let writers: Vec<_> = connections
            .into_iter()
            .map(|mut db| s.spawn(move || write_spans(&mut db, recv, progress, total)))
            .collect();
        let produced = chains.par_iter().with_max_len(1).try_for_each_init(
            || (repo.to_thread_local(), send.clone()),
            |(repo, send), chain| -> Result<()> {
                let mut incremental = Incremental::new();
                for &blob in chain {
                    let (path, lang) = langs[blob];
                    let src = repo
                        .find_blob(oid(blob))
                        .with_context(|| {
                            format!(
                                "read blob {blob} ({path}); a partial clone (--filter) lacks \
                                 file contents: clone without a filter"
                            )
                        })?
                        .take_data();
                    let content = parse_blob(blob, path, lang, &src, &mut incremental)?;
                    send.send((blob, lang, content))
                        .map_err(|_| anyhow!("the writer stopped"))?;
                }
                Ok(())
            },
        );
        drop(send);
        // A failed writer drops the receiver, which stops the workers: report its error first.
        let mut written = 0;
        for writer in writers {
            written += writer.join().expect("writers do not panic")?;
        }
        produced.map(|()| written)
    })?;
    assert_eq!(written, total, "every listed blob is written");
    info!(blobs = written, elapsed = ?started.elapsed(), "parse done");
    Ok(written)
}

/// Most blobs a chain holds. Each chain starts with a parse from scratch, and the
/// longest chain bounds how long the parse takes, so a file's long history is split.
const CHAIN: usize = 32;

/// The blobs of `langs` in chains to parse in order, each blob once: a chain holds
/// successive versions of a file across renames (a lineage), so each but the first
/// is parsed as an edit of the one before. A blob in several lineages joins the first.
fn chains<'a>(
    db: &Connection,
    langs: &HashMap<&'a str, (&str, Lang)>,
) -> Result<Vec<Vec<&'a str>>> {
    let versions: Vec<(String, String)> = db
        .prepare(
            "select lineage, blob from lineages
             where blob in (select blob from parse_candidates) and blob not in (select blob from spans)
             order by lineage, seq",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut seen = HashSet::new();
    let mut chains = Vec::new();
    for lineage in versions.chunk_by(|a, b| a.0 == b.0) {
        let blobs: Vec<&str> = lineage
            .iter()
            .filter_map(|(_, blob)| langs.get_key_value(blob.as_str()).map(|(b, _)| *b))
            .filter(|b| seen.insert(*b))
            .collect();
        chains.extend(blobs.chunks(CHAIN).map(<[_]>::to_vec));
    }
    // Every blob is in a lineage; any that is not still gets parsed.
    let mut rest: Vec<&str> = langs
        .keys()
        .copied()
        .filter(|b| !seen.contains(b))
        .collect();
    rest.sort_unstable();
    chains.extend(rest.into_iter().map(|b| vec![b]));
    Ok(chains)
}

/// A blob id, its language, and what parsing it gave.
type Parsed<'a> = (&'a str, Lang, Content);

/// What a blob holds.
enum Content {
    /// Source code, with its spans.
    Code(Vec<Span>),
    /// Generated code: a `file` row with NULL counts and a `generated` span.
    Generated,
    /// Not source code to parse (too large, binary, an LFS pointer): a `file` row
    /// with NULL counts.
    Skipped,
}

/// Connections appending spans at once. One appends about two million rows a
/// second, slower than the workers extract them; two keep up (on RediSearch, parse
/// 7.7 s with one, 4.7 s with two, no faster with more).
const WRITERS: usize = 2;

/// Append parsed blobs as they arrive, committing every `BATCH` blobs; return how
/// many this writer appended. On error, drop the shared receiver, which stops the
/// workers and the other writers.
fn write_spans(
    db: &mut Connection,
    receiver: &Mutex<Option<Receiver<Parsed>>>,
    progress: &AtomicUsize,
    total: usize,
) -> Result<usize> {
    let next = || {
        let receiver = receiver
            .lock()
            .expect("no writer panics holding the receiver");
        receiver.as_ref().and_then(|r| r.recv().ok())
    };
    let mut parsed = std::iter::from_fn(next).peekable();
    let mut written = 0;
    while parsed.peek().is_some() {
        let appended = append(db, parsed.by_ref().take(BATCH));
        let Ok(n) = appended else {
            drop(parsed);
            receiver.lock().map(|mut r| r.take()).ok();
            return appended;
        };
        written += n;
        let done = progress.fetch_add(n, Relaxed) + n;
        info!("parse: {done}/{total} blobs");
    }
    Ok(written)
}

/// Append `parsed` in one transaction; return how many blobs it held.
fn append<'a>(db: &mut Connection, parsed: impl Iterator<Item = Parsed<'a>>) -> Result<usize> {
    let mut written = 0;
    let tx = db.transaction()?;
    let mut spans = tx.appender("spans")?;
    let mut fingerprints = tx.appender("fingerprints")?;
    for (blob, lang, content) in parsed {
        written += 1;
        // Blobs that are not parsed get rows with NULL counts.
        let unparsed: &[&str] = match content {
            Content::Code(extracted) => {
                for s in extracted {
                    if s.kind == "fingerprint" {
                        fingerprints.append_row(params![blob, s.hash, s.start_byte, s.end_byte])?;
                        continue;
                    }
                    spans.append_row(params![
                        blob,
                        V,
                        lang.name,
                        s.kind,
                        s.name,
                        s.line,
                        s.end_line,
                        s.tokens,
                        s.comments,
                        s.params,
                        (s.kind == "fn").then_some(s.hash),
                        s.errors,
                        s.start_byte,
                        s.end_byte,
                        s.depth
                    ])?;
                }
                &[]
            }
            Content::Generated => &["file", "generated"],
            Content::Skipped => &["file"],
        };
        let null = None::<i32>;
        for kind in unparsed {
            spans.append_row(params![
                blob, V, lang.name, kind, null, 1, null, null, null, null, null, null, null, null,
                null
            ])?;
        }
    }
    spans.flush()?;
    fingerprints.flush()?;
    drop((spans, fingerprints));
    tx.commit()?;
    Ok(written)
}

/// What one blob holds. Not parsed: blobs over `MAX_BLOB_BYTES`, binary (a NUL
/// byte), Git LFS pointers, and generated code (a marker such as "DO NOT EDIT" or
/// "@generated" in the first five lines, or minified JavaScript: lines averaging over
/// 110 bytes, as GitHub's Linguist decides). A panic in the parser becomes an error
/// naming the blob. `incremental` parses `src` as an edit of the blob it parsed last.
fn parse_blob(
    blob: &str,
    path: &str,
    lang: Lang,
    src: &[u8],
    incremental: &mut Incremental,
) -> Result<Content> {
    debug!(%blob, %path, lang = lang.name, bytes = src.len(), "blob");
    if let Some(reason) = skip(src) {
        debug!(%blob, %path, reason, "not parsed");
        return Ok(Content::Skipped);
    }
    if is_generated(lang, src) {
        debug!(%blob, %path, "generated; not parsed");
        return Ok(Content::Generated);
    }
    let started = Instant::now();
    let spans = std::panic::catch_unwind(AssertUnwindSafe(|| incremental.extract(lang, src)))
        .map_err(|panic| {
            let message = panic
                .downcast_ref::<&str>()
                .map(|m| m.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            anyhow!(
                "extracting {path} (blob {blob}, {}) panicked: {message}",
                lang.name
            )
        })?;
    let elapsed = started.elapsed();
    if elapsed > SLOW_BLOB {
        warn!(%blob, %path, bytes = src.len(), ?elapsed, "slow blob");
    }
    debug!(%blob, spans = spans.len(), ?elapsed, "extracted");
    Ok(Content::Code(spans))
}

/// Why `src` is not source code to parse, if it is not.
fn skip(src: &[u8]) -> Option<&'static str> {
    if src.len() > MAX_BLOB_BYTES {
        Some("exceeds MAX_BLOB_BYTES")
    } else if src[..src.len().min(8192)].contains(&0) {
        Some("binary")
    } else if src.starts_with(b"version https://git-lfs.github.com/spec/") {
        Some("a Git LFS pointer")
    } else {
        None
    }
}

/// Whether codevitals parses `src` as `lang`: source code, not generated.
pub fn parseable(lang: Lang, src: &[u8]) -> bool {
    skip(src).is_none() && !is_generated(lang, src)
}

/// Markers that code generators write near the top of their output: Go's `Code
/// generated ... DO NOT EDIT.`, `@generated`, and the "generated by"/"generated with"
/// headers of protoc, gRPC, Cython, cbindgen, bindgen, Lemon, jison, PEG.js and others.
const GENERATED_MARKERS: &[&str] = &[
    "do not edit",
    "@generated",
    "auto-generated",
    "autogenerated",
    "automatically generated",
    "generated by",
    "generated with",
];

/// Whether `src` is generated code: a marker in its first five lines, or, for
/// JavaScript and TypeScript, minified (lines averaging over 110 bytes).
fn is_generated(lang: Lang, src: &[u8]) -> bool {
    let marked = src.split(|&b| b == b'\n').take(5).any(|line| {
        let line = line.to_ascii_lowercase();
        GENERATED_MARKERS
            .iter()
            .any(|m| line.windows(m.len()).any(|w| w == m.as_bytes()))
    });
    let lines = src.iter().filter(|&&b| b == b'\n').count().max(1);
    let ecma = ["typescript", "tsx", "javascript"].contains(&lang.name);
    marked || (ecma && src.len() / lines > 110)
}
