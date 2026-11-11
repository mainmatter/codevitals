use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use clap::{CommandFactory, Parser};
use duckdb::types::{TimeUnit, Value};
use duckdb::{Connection, OptionalExt, params};
use gix::ObjectId;
use gix::object::tree::diff::ChangeDetached as Change;
use rayon::prelude::*;
use tally::{Lang, Span, extract};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

mod graph;
mod metrics;

/// Extractor version. Bump whenever `extract` output could change for any input.
const V: i32 = 6;
const MAX_BLOB_BYTES: usize = 1 << 20;
/// Commits or blobs per transaction. A transaction costs milliseconds whatever its
/// size, so batching dominates throughput; an interrupt loses at most one batch.
const BATCH: usize = 500;
/// Warn about any blob whose extraction takes longer than this.
const SLOW_BLOB: Duration = Duration::from_secs(1);
/// DuckDB's memory ceiling and spill-to-disk ceiling for every query. Above
/// them a query fails instead of exhausting the machine's memory or disk.
const MEMORY_LIMIT: &str = "4GB";
const SPILL_LIMIT: &str = "4GB";
const DIVERGED: &str = "history diverged from the stored chain; reset the walk with: duckdb <db> \"delete from commits; delete from changes\" (parsed spans are kept), or delete the database file, then scan again";

/// Record facts about a git repository's history into a DuckDB file.
///
/// Logs go to stderr; set `RUST_LOG` to `debug` for one line per commit and blob,
/// or to `off` for silence. The default is `info`: phases and progress.
#[derive(Parser)]
enum Cli {
    /// Record REV's first-parent history, then parse every new blob.
    Scan {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = "tally.duckdb")]
        db: PathBuf,
        #[arg(default_value = "HEAD")]
        rev: String,
    },
    /// Compute metrics into one JSON object: each metric's name maps to an array
    /// of row objects.
    Metrics {
        #[arg(long, default_value = "tally.duckdb")]
        db: PathBuf,
        /// File to write, replaced atomically; `-` writes to stdout.
        output: PathBuf,
        /// Metrics to compute; all of them when omitted.
        #[arg(value_name = "METRIC")]
        names: Vec<String>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .init();
    // Listing the metrics means binding metrics.sql, so the command that lists them
    // (in `--help` and as the valid values of METRIC) is built only to report help
    // or an error.
    let cli = Cli::try_parse().unwrap_or_else(|_| usage());
    match cli {
        Cli::Scan { repo, db, rev } => {
            let repo = gix::open(&repo).with_context(|| format!("open repository {repo:?}"))?;
            let mut db = open(&db)?;
            let (commits, changes) = walk(&mut db, &repo, &rev)?;
            let blobs = parse(&mut db, &repo)?;
            eprintln!("scanned {commits} commits, {changes} changes; parsed {blobs} blobs");
        }
        Cli::Metrics { db, output, names } => {
            let mut db = open(&db)?;
            ensure!(
                !metrics::exists(&db, &output.to_string_lossy())?,
                "{output:?} is a metric; give the output file first, e.g. `tally metrics - {}`",
                output.display()
            );
            for name in &names {
                if !metrics::exists(&db, name)? {
                    usage();
                }
            }
            let json = metrics::export(&mut db, &names)?;
            if output.as_os_str() == "-" {
                println!("{json}");
            } else {
                // Write beside the target, then rename: readers never see a partial file.
                let partial = output.with_extension("partial");
                std::fs::write(&partial, json + "\n")
                    .with_context(|| format!("write {partial:?}"))?;
                std::fs::rename(&partial, &output)
                    .with_context(|| format!("replace {output:?}"))?;
                info!(?output, "metrics written");
            }
        }
    }
    Ok(())
}

/// Parses the arguments again with the full command, which lists every metric, to
/// print help or a usage error, and exits.
fn usage() -> ! {
    Cli::command()
        .mut_subcommand("metrics", |c| {
            c.mut_arg("names", |a| a.value_parser(metrics::names()))
        })
        .get_matches();
    unreachable!("the arguments fail to parse with the full command too")
}

fn open(path: &Path) -> Result<Connection> {
    let db = Connection::open(path).with_context(|| format!("open database {path:?}"))?;
    db.execute_batch(&format!(
        "set memory_limit = '{MEMORY_LIMIT}'; set max_temp_directory_size = '{SPILL_LIMIT}'"
    ))?;
    // Views and macros are rebuilt from the SQL files whenever they differ from the
    // ones the database was last opened with (`tally_sql()` holds their hash), so
    // ones removed from the files disappear from existing databases too. Binding
    // them costs more than a small query, so an unchanged database skips it.
    let sql = format!("{}\n{}", include_str!("schema.sql"), metrics::SQL);
    let hash = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(sql.as_bytes()));
    let applied: Option<String> = db
        .query_row(
            "select function_name from duckdb_functions() where function_name = 'tally_sql'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .map(|()| db.query_row("select tally_sql()", [], |r| r.get(0)))
        .transpose()?;
    if applied.as_deref() != Some(hash.as_str()) {
        let stale: Vec<String> = db
            .prepare(
                "select 'drop view ' || view_name from duckdb_views() where not internal
                 union all
                 select 'drop macro ' || if(function_type = 'table_macro', 'table ', '') || function_name
                 from duckdb_functions() where function_type in ('macro', 'table_macro') and not internal",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        db.execute_batch(&stale.join(";\n"))?;
        db.execute_batch(include_str!("schema.sql"))?;
        let current: bool = db.query_row(
            "select count(*) = 1 from duckdb_columns() where table_name = 'spans' and column_name = 'depth'",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            current,
            "{path:?} was created by an older tally; delete it and scan again"
        );
        db.execute_batch(metrics::SQL)
            .context("apply metrics.sql")?;
        db.execute_batch(&format!("create macro tally_sql() as '{hash}'"))?;
    }
    Ok(db)
}

/// Extend `commits` and `changes` with REV's first-parent chain past the stored tip.
fn walk(db: &mut Connection, repo: &gix::Repository, rev: &str) -> Result<(usize, usize)> {
    let tip = db
        .query_row(
            "select seq, id from commits order by seq desc limit 1",
            [],
            |r| Ok((r.get::<_, i32>(0)?, oid(&r.get::<_, String>(1)?))),
        )
        .optional()?;

    // New commits, newest first.
    let mut chain = Vec::new();
    let mut next = Some(repo.rev_parse_single(rev)?.object()?.peel_to_commit()?.id);
    while let Some(id) = next.filter(|id| tip.is_none_or(|(_, t)| t != *id)) {
        chain.push(id);
        next = repo
            .find_commit(id)?
            .parent_ids()
            .next()
            .map(|p| p.detach());
    }
    ensure!(tip.is_none() || next.is_some(), DIVERGED);
    let started = Instant::now();
    info!(new = chain.len(), stored = tip.map_or(0, |(seq, _)| seq + 1), %rev, "walk");

    let mut changes = 0;
    let mut seq = tip.map_or(0, |(seq, _)| seq + 1);
    for (b, batch) in chain.rchunks(BATCH).enumerate() {
        // Appenders join the open transaction, so each commit row and its changes land together.
        let tx = db.transaction()?;
        let mut commits = tx.appender("commits")?;
        let mut changed = tx.appender("changes")?;
        for &id in batch.iter().rev() {
            let diffing = Instant::now();
            let commit = repo.find_commit(id)?;
            // The previous commit on the chain is the first parent; the root diffs against the empty tree.
            let prev = match commit.parent_ids().next() {
                Some(p) => p.object()?.into_commit().tree()?,
                None => repo.empty_tree(),
            };
            let mut rows = BTreeMap::new();
            for c in
                repo.diff_tree_to_tree(&prev, &commit.tree()?, gix::diff::Options::default())?
            {
                let (old, new) = match c {
                    Change::Addition { .. } => (None, file(c.entry_mode_and_id())),
                    Change::Deletion { .. } => (file(c.entry_mode_and_id()), None),
                    Change::Modification { .. } => (
                        file(c.source_entry_mode_and_id()),
                        file(c.entry_mode_and_id()),
                    ),
                    Change::Rewrite { .. } => unreachable!("rename tracking is off"),
                };
                if old != new {
                    let dup = rows.insert(c.location().to_string(), new);
                    assert!(dup.is_none(), "two changes for {} in {id}", c.location());
                }
            }
            let time = Value::Timestamp(TimeUnit::Second, commit.time()?.seconds);
            let author = commit.author()?;
            let author = format!("{} <{}>", author.name, author.email);
            let message = commit.message_raw()?.to_string();
            commits.append_row(params![seq, id.to_string(), time, author, message])?;
            for (path, blob) in &rows {
                changed.append_row(params![seq, path, blob])?;
            }
            debug!(seq, %id, changes = rows.len(), elapsed = ?diffing.elapsed(), "commit");
            seq += 1;
            changes += rows.len();
        }
        commits.flush()?;
        changed.flush()?;
        drop((commits, changed));
        tx.commit()?;
        info!(
            "walk: {}/{} commits",
            (b * BATCH + batch.len()),
            chain.len()
        );
    }
    info!(commits = chain.len(), changes, elapsed = ?started.elapsed(), "walk done");
    Ok((chain.len(), changes))
}

/// `B(tree, path)`: the blob id when the entry is a regular or executable file.
fn file((mode, id): (gix::object::tree::EntryMode, &gix::oid)) -> Option<String> {
    mode.is_blob().then(|| id.to_string())
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.as_bytes()).expect("stored ids are 40 hex characters")
}

/// Make `spans` complete at version `V` for every changed blob with a known language.
fn parse(db: &mut Connection, repo: &gix::Repository) -> Result<usize> {
    let started = Instant::now();
    let stale = db.execute("delete from spans where v <> ?", [V])?;
    let candidates: Vec<(String, String)> = db
        .prepare(
            "select blob, min(path) as path from changes
             where blob is not null and blob not in (select blob from spans)
             group by blob",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let todo: Vec<_> = candidates
        .iter()
        .filter_map(|(blob, path)| Some((blob, path, Lang::of(path)?)))
        .collect();
    let unknown = candidates.len() - todo.len();
    let threads = rayon::current_num_threads();
    let listed = started.elapsed();
    info!(
        blobs = todo.len(),
        unknown_language = unknown,
        stale_spans = stale,
        threads,
        ?listed,
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
                    let src = repo.find_blob(oid(blob))?.take_data();
                    Ok((blob, lang, parse_blob(blob, path, lang, &src)))
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

/// A blob id, its language, and its spans (`None` if it exceeds `MAX_BLOB_BYTES`).
type Parsed<'a> = (&'a str, Lang, Option<Vec<Span>>);

/// Append parsed blobs as they arrive, committing every `BATCH` blobs.
fn write_spans(db: &mut Connection, parsed: Receiver<Parsed>, total: usize) -> Result<usize> {
    let mut parsed = parsed.into_iter().peekable();
    let mut written = 0;
    while parsed.peek().is_some() {
        let tx = db.transaction()?;
        let mut spans = tx.appender("spans")?;
        for (blob, lang, extracted) in parsed.by_ref().take(BATCH) {
            written += 1;
            let Some(extracted) = extracted else {
                let null = None::<i32>;
                spans.append_row(params![
                    blob, V, lang.name, "file", null, 1, null, null, null, null, null, null, null,
                    null, null
                ])?;
                continue;
            };
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
        }
        spans.flush()?;
        drop(spans);
        tx.commit()?;
        info!("parse: {written}/{total} blobs");
    }
    Ok(written)
}

/// The spans of one blob, or `None` when it exceeds `MAX_BLOB_BYTES` and gets only
/// a `file` row with NULL counts.
fn parse_blob(blob: &str, path: &str, lang: Lang, src: &[u8]) -> Option<Vec<Span>> {
    debug!(%blob, %path, lang = lang.name, bytes = src.len(), "blob");
    if src.len() > MAX_BLOB_BYTES {
        debug!(%blob, %path, "blob exceeds MAX_BLOB_BYTES; storing only its file span");
        return None;
    }
    let started = Instant::now();
    let spans = extract(lang, src);
    let elapsed = started.elapsed();
    if elapsed > SLOW_BLOB {
        warn!(%blob, %path, bytes = src.len(), ?elapsed, "slow blob");
    }
    debug!(%blob, spans = spans.len(), ?elapsed, "extracted");
    Some(spans)
}
