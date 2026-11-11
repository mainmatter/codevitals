//! The parse: every changed blob with a known language into `spans`.

use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use codevitals::{Lang, Span, extract};
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
    db.execute_batch(&format!(
        "delete from codevitals; insert into codevitals values ({SCHEMA}, {V})"
    ))?;
    // Every path a blob ever had, in order: the blob is parsed in the language of the
    // first one with a known extension (an empty __init__.py shares its blob with
    // every other empty file).
    let paths: Vec<(String, String)> = db
        .prepare(
            "select distinct blob, path from changes
             where blob is not null and blob not in (select blob from spans)
             order by blob, path",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let candidates: Vec<&[(String, String)]> = paths.chunk_by(|a, b| a.0 == b.0).collect();
    let todo: Vec<_> = candidates
        .iter()
        .filter_map(|paths| {
            paths
                .iter()
                .find_map(|(blob, path)| Some((blob, path, Lang::of(path)?)))
        })
        .collect();
    info!(
        blobs = todo.len(),
        unknown_language = candidates.len() - todo.len(),
        stale_spans = stale,
        threads = rayon::current_num_threads(),
        listed = ?started.elapsed(),
        "parse"
    );

    // Workers read and extract blobs in parallel, each with its own repository handle,
    // while one writer appends them as they arrive. A message is one whole blob, so the
    // writer's transactions never split a blob.
    let repo = repo.clone().into_sync();
    let total = todo.len();
    let written = std::thread::scope(|s| {
        let (send, recv) = sync_channel(BATCH);
        let writer = s.spawn(move || write_spans(db, recv, total));
        let produced = todo
            .par_iter()
            .map_init(
                || repo.to_thread_local(),
                |repo, &(blob, path, lang)| -> Result<Parsed<'_>> {
                    let src = repo
                        .find_blob(oid(blob))
                        .with_context(|| {
                            format!(
                                "read blob {blob} ({path}); a partial clone (--filter) lacks file \
                                 contents: clone without a filter"
                            )
                        })?
                        .take_data();
                    Ok((blob, lang, parse_blob(blob, path, lang, &src)?))
                },
            )
            .try_for_each_with(send, |send, parsed| {
                send.send(parsed?)
                    .map_err(|_| anyhow!("the writer stopped"))
            });
        // A failed writer drops its receiver, which stops the workers: report its error first.
        let written = writer.join().expect("the writer does not panic")?;
        produced.map(|()| written)
    })?;
    assert_eq!(written, total, "every listed blob is written");
    info!(blobs = written, elapsed = ?started.elapsed(), "parse done");
    Ok(written)
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

/// Append parsed blobs as they arrive, committing every `BATCH` blobs.
fn write_spans(db: &mut Connection, parsed: Receiver<Parsed>, total: usize) -> Result<usize> {
    let mut parsed = parsed.into_iter().peekable();
    let mut written = 0;
    while parsed.peek().is_some() {
        let tx = db.transaction()?;
        let mut spans = tx.appender("spans")?;
        for (blob, lang, content) in parsed.by_ref().take(BATCH) {
            written += 1;
            // Blobs that are not parsed get rows with NULL counts.
            let unparsed: &[&str] = match content {
                Content::Code(extracted) => {
                    for s in extracted {
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
                            s.hash,
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
                    blob, V, lang.name, kind, null, 1, null, null, null, null, null, null, null,
                    null, null
                ])?;
            }
        }
        spans.flush()?;
        drop(spans);
        tx.commit()?;
        info!("parse: {written}/{total} blobs");
    }
    Ok(written)
}

/// What one blob holds. Not parsed: blobs over `MAX_BLOB_BYTES`, binary (a NUL
/// byte), Git LFS pointers, and generated code (a marker such as "DO NOT EDIT" or
/// "@generated" in the first five lines, or minified JavaScript: lines averaging over
/// 110 bytes, as GitHub's Linguist decides). A panic in the parser becomes an error
/// naming the blob.
fn parse_blob(blob: &str, path: &str, lang: Lang, src: &[u8]) -> Result<Content> {
    debug!(%blob, %path, lang = lang.name, bytes = src.len(), "blob");
    let skip = if src.len() > MAX_BLOB_BYTES {
        Some("exceeds MAX_BLOB_BYTES")
    } else if src[..src.len().min(8192)].contains(&0) {
        Some("binary")
    } else if src.starts_with(b"version https://git-lfs.github.com/spec/") {
        Some("a Git LFS pointer")
    } else {
        None
    };
    if let Some(reason) = skip {
        debug!(%blob, %path, reason, "not parsed");
        return Ok(Content::Skipped);
    }
    if is_generated(lang, src) {
        debug!(%blob, %path, "generated; not parsed");
        return Ok(Content::Generated);
    }
    let started = Instant::now();
    let spans = std::panic::catch_unwind(|| extract(lang, src)).map_err(|panic| {
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
