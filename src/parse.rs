//! The parse: every changed blob with a known language into `spans`.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use codevitals::{Incremental, Lang, Span};
use duckdb::arrow::array::{
    ArrayBuilder, ArrayRef, Int32Array, Int32Builder, StringBuilder, UInt64Builder,
};
use duckdb::arrow::record_batch::RecordBatch;
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
    let mut paths: Vec<(String, String)> = db
        .prepare(
            "select distinct blob, path from changes
             where blob in (select blob from parse_candidates) and blob not in (select blob from spans)",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    // DuckDB returns a large result out of order when it need not preserve insertion
    // order, even under ORDER BY: sort here.
    paths.sort_unstable();
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
    let mut versions: Vec<(String, i32, String)> = db
        .prepare(
            "select lineage, seq, blob from lineages
             where blob in (select blob from parse_candidates) and blob not in (select blob from spans)",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    // Sorted here, not by ORDER BY (see `parse`).
    versions.sort_unstable();
    let mut seen = HashSet::new();
    let mut chains = Vec::new();
    for lineage in versions.chunk_by(|a, b| a.0 == b.0) {
        let blobs: Vec<&str> = lineage
            .iter()
            .filter_map(|(_, _, blob)| langs.get_key_value(blob.as_str()).map(|(b, _)| *b))
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

/// Connections appending spans at once. One is slower than the workers extract
/// (on RediSearch, parse 5.3 s with one, 3.4 s with two); more do not help, as the
/// parse then uses every core.
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

/// Append `parsed` in one transaction, as Arrow record batches (a column at a time,
/// several times faster than appending row by row); return how many blobs it held.
fn append<'a>(db: &mut Connection, parsed: impl Iterator<Item = Parsed<'a>>) -> Result<usize> {
    let (mut spans, mut fingerprints) = (SpanColumns::default(), FingerprintColumns::default());
    let mut written = 0;
    for (blob, lang, content) in parsed {
        written += 1;
        match content {
            Content::Code(extracted) => {
                for s in &extracted {
                    match s.kind {
                        "fingerprint" => fingerprints.push(blob, s),
                        _ => spans.push(blob, lang, s),
                    }
                }
            }
            // Blobs that are not parsed get rows with NULL counts.
            Content::Generated => {
                spans.push_unparsed(blob, lang, "file");
                spans.push_unparsed(blob, lang, "generated");
            }
            Content::Skipped => spans.push_unparsed(blob, lang, "file"),
        }
    }
    let tx = db.transaction()?;
    tx.appender("spans")?.append_record_batch(spans.finish()?)?;
    tx.appender("fingerprints")?
        .append_record_batch(fingerprints.finish()?)?;
    tx.commit()?;
    Ok(written)
}

/// The columns of `spans` rows, as `schema.sql` declares them.
#[derive(Default)]
struct SpanColumns {
    blob: StringBuilder,
    lang: StringBuilder,
    kind: StringBuilder,
    name: StringBuilder,
    line: Int32Builder,
    end_line: Int32Builder,
    tokens: Int32Builder,
    comments: Int32Builder,
    params: Int32Builder,
    hash: UInt64Builder,
    errors: Int32Builder,
    start_byte: Int32Builder,
    end_byte: Int32Builder,
    depth: Int32Builder,
}

impl SpanColumns {
    fn push(&mut self, blob: &str, lang: Lang, s: &Span) {
        self.blob.append_value(blob);
        self.lang.append_value(lang.name);
        self.kind.append_value(s.kind);
        self.name.append_option(s.name.as_deref());
        self.line.append_value(s.line);
        self.end_line.append_value(s.end_line);
        self.tokens.append_value(s.tokens);
        self.comments.append_value(s.comments);
        self.params.append_option(s.params);
        // The structural hash of functions only.
        self.hash.append_option((s.kind == "fn").then_some(s.hash));
        self.errors.append_option(s.errors);
        self.start_byte.append_value(s.start_byte);
        self.end_byte.append_value(s.end_byte);
        self.depth.append_value(s.depth);
    }

    /// A row of a blob that is not parsed: its kind, line 1, NULL counts.
    fn push_unparsed(&mut self, blob: &str, lang: Lang, kind: &str) {
        self.blob.append_value(blob);
        self.lang.append_value(lang.name);
        self.kind.append_value(kind);
        self.line.append_value(1);
        self.name.append_null();
        self.hash.append_null();
        for c in [
            &mut self.end_line,
            &mut self.tokens,
            &mut self.comments,
            &mut self.params,
            &mut self.errors,
            &mut self.start_byte,
            &mut self.end_byte,
            &mut self.depth,
        ] {
            c.append_null();
        }
    }

    fn finish(mut self) -> Result<RecordBatch> {
        let rows = self.blob.len();
        let v: ArrayRef = Arc::new(Int32Array::from(vec![V; rows]));
        let string = |b: &mut StringBuilder| -> ArrayRef { Arc::new(b.finish()) };
        let int = |b: &mut Int32Builder| -> ArrayRef { Arc::new(b.finish()) };
        Ok(RecordBatch::try_from_iter([
            ("blob", string(&mut self.blob)),
            ("v", v),
            ("lang", string(&mut self.lang)),
            ("kind", string(&mut self.kind)),
            ("name", string(&mut self.name)),
            ("line", int(&mut self.line)),
            ("end_line", int(&mut self.end_line)),
            ("tokens", int(&mut self.tokens)),
            ("comments", int(&mut self.comments)),
            ("params", int(&mut self.params)),
            ("hash", Arc::new(self.hash.finish()) as ArrayRef),
            ("errors", int(&mut self.errors)),
            ("start_byte", int(&mut self.start_byte)),
            ("end_byte", int(&mut self.end_byte)),
            ("depth", int(&mut self.depth)),
        ])?)
    }
}

/// The columns of `fingerprints` rows.
#[derive(Default)]
struct FingerprintColumns {
    blob: StringBuilder,
    hash: UInt64Builder,
    start_byte: Int32Builder,
    end_byte: Int32Builder,
}

impl FingerprintColumns {
    fn push(&mut self, blob: &str, s: &Span) {
        self.blob.append_value(blob);
        self.hash.append_value(s.hash);
        self.start_byte.append_value(s.start_byte);
        self.end_byte.append_value(s.end_byte);
    }

    fn finish(mut self) -> Result<RecordBatch> {
        Ok(RecordBatch::try_from_iter([
            ("blob", Arc::new(self.blob.finish()) as ArrayRef),
            ("hash", Arc::new(self.hash.finish())),
            ("start_byte", Arc::new(self.start_byte.finish())),
            ("end_byte", Arc::new(self.end_byte.finish())),
        ])?)
    }
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
