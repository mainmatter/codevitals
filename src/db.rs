//! The database: opening it, its versions, and bringing its views up to date.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use duckdb::{AccessMode, Connection, OptionalExt};
use gix::ObjectId;

/// Extractor version. Bump whenever `extract` output could change for any input.
pub const V: i32 = 9;
/// Database layout version. Bump whenever a table changes; older databases are refused.
pub const SCHEMA: i32 = 8;
/// Commits or blobs per transaction. A transaction costs milliseconds whatever its
/// size, so batching dominates throughput; an interrupt loses at most one batch.
pub const BATCH: usize = 500;
/// DuckDB's memory ceiling and spill-to-disk ceiling for every query. Above
/// them a query fails instead of exhausting the machine's memory or disk.
const MEMORY_LIMIT: &str = "4GB";
const SPILL_LIMIT: &str = "4GB";

/// The repository a database was scanned from (its git directory), opened, unless
/// `over` names another; either way it must hold the newest scanned commit, whose
/// blobs the database describes.
pub fn scanned_repo(db: &Connection, over: Option<&Path>) -> Result<gix::Repository> {
    let recorded: Option<String> = db
        .query_row("select repo from codevitals", [], |r| r.get(0))
        .optional()?
        .flatten();
    let path = match (over, &recorded) {
        (Some(path), _) => path.to_path_buf(),
        (None, Some(recorded)) => recorded.into(),
        (None, None) => {
            anyhow::bail!("the database does not record the repository it scanned; pass --repo")
        }
    };
    let repo = gix::discover(&path).with_context(|| {
        format!("open the git repository {path:?}, which the database was scanned from; pass --repo if it moved")
    })?;
    let (_, tip) = tip(db)?;
    ensure!(
        repo.find_commit(oid(&tip)).is_ok(),
        "{:?} does not contain the newest scanned commit {tip}: the database was scanned from {}; \
         pass that repository with --repo, or rescan",
        repo.git_dir(),
        recorded.as_deref().unwrap_or("an unrecorded repository")
    );
    Ok(repo)
}

/// The newest scanned commit: its seq and id.
pub fn tip(db: &Connection) -> Result<(i32, String)> {
    stored_tip(db)?.context("the database has no commits")
}

/// The newest scanned commit, if any.
pub fn stored_tip(db: &Connection) -> Result<Option<(i32, String)>> {
    Ok(db
        .query_row(
            "select seq, id from commits order by seq desc limit 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// A stored object id.
pub fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.as_bytes()).expect("stored ids are 40 hex characters")
}

/// Opens the database, refusing one written by another codevitals version, and brings
/// its views and macros up to date. Views and macros are rebuilt from the SQL files
/// whenever they differ from the ones the database was last opened with
/// (`codevitals_sql()` holds their hash), so ones removed from the files disappear
/// from existing databases too. `metrics` opens read-only and reopens for writing
/// only to rebuild them.
pub fn open(path: &Path, access: AccessMode) -> Result<Connection> {
    let (db, views) = open_with(path, access, false)?;
    assert!(views.is_none(), "nothing is deferred");
    Ok(db)
}

/// `open` for writing, for `scan`: views the scan does not read (`VIEWS_SQL`), when
/// they need rebuilding, are rebuilt on another connection in the background (most of
/// a second of binding), to be waited for with `Views::finish` before exiting.
pub fn open_for_scan(path: &Path) -> Result<(Connection, Option<Views>)> {
    let (db, views) = open_with(path, AccessMode::ReadWrite, true)?;
    // A scan appends millions of rows. Past the threshold of write-ahead log, a
    // commit checkpoints (compresses and writes every changed row group): at
    // DuckDB's 16 MB, many times during the parse. Once, on close, is enough.
    db.execute_batch(&format!("set checkpoint_threshold = '{SCAN_CHECKPOINT}'"))?;
    Ok((db, views))
}

/// Write-ahead log a scan accumulates before checkpointing.
const SCAN_CHECKPOINT: &str = "1GB";

/// Views being rebuilt in the background.
pub struct Views(std::thread::JoinHandle<Result<()>>);

impl Views {
    /// Waits until the views are rebuilt and marked current.
    pub fn finish(self) -> Result<()> {
        self.0.join().expect("rebuilding views does not panic")
    }
}

fn open_with(path: &Path, access: AccessMode, defer: bool) -> Result<(Connection, Option<Views>)> {
    let connect = |access| -> Result<Connection> {
        let config = duckdb::Config::default().access_mode(access)?;
        let db = Connection::open_with_flags(path, config).with_context(|| {
            format!(
                "open database {path:?} (another codevitals may be using it; DuckDB allows one writer)"
            )
        })?;
        // Not preserving insertion order saves memory materializing large views. It
        // has a cost: this DuckDB then returns a result of more than about 120,000 rows
        // out of order even under ORDER BY. So no reader relies on a large result's
        // order: it sorts or groups in Rust, or aggregates with `list(... order by)`.
        db.execute_batch(&format!(
            "set memory_limit = '{MEMORY_LIMIT}'; set max_temp_directory_size = '{SPILL_LIMIT}';
             set preserve_insertion_order = false"
        ))?;
        Ok(db)
    };
    let mut db = connect(access.clone())?;
    check_version(&db, path)?;
    let hash = codevitals::sql_hash();
    let applied: Option<String> = db
        .query_row(
            "select function_name from duckdb_functions() where function_name = 'codevitals_sql'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .map(|()| db.query_row("select codevitals_sql()", [], |r| r.get(0)))
        .transpose()?;
    if applied.as_deref() == Some(hash.as_str()) {
        return Ok((db, None));
    }
    if access == AccessMode::ReadOnly {
        drop(db);
        db = connect(AccessMode::ReadWrite).with_context(|| {
            format!("{path:?} needs this version's views; open it once with write access, e.g. `cv scan`")
        })?;
    }
    let stale: Vec<String> = db
        .prepare(
            "select 'drop view ' || schema_name || '.\"' || view_name || '\"' from duckdb_views() where not internal
             union all
             select 'drop macro ' || if(function_type = 'table_macro', 'table ', '') || function_name
             from duckdb_functions() where function_type in ('macro', 'table_macro') and not internal",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    db.execute_batch(&stale.join(";\n"))?;
    db.execute_batch(codevitals::SCAN_SQL)
        .context("apply codevitals' SQL")?;
    // The hash marks the views current only once all are applied: an interrupted
    // rebuild is redone.
    let rest = move |db: &Connection| -> Result<()> {
        db.execute_batch(codevitals::VIEWS_SQL)
            .context("apply codevitals' SQL")?;
        db.execute_batch(&codevitals::sql_mark())?;
        Ok(())
    };
    if !defer {
        rest(&db)?;
        return Ok((db, None));
    }
    let background = db.try_clone()?;
    let views = Views(std::thread::spawn(move || rest(&background)));
    Ok((db, Some(views)))
}

/// Refuses a database whose layout or spans come from another codevitals version: an
/// older layout cannot be read, and a newer extractor's spans would be deleted and
/// reparsed by this one. A database without a version row is new.
fn check_version(db: &Connection, path: &Path) -> Result<()> {
    let exists: bool = db.query_row(
        "select count(*) > 0 from duckdb_tables() where table_name = 'codevitals'",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let Some((schema, extractor)): Option<(i32, i32)> = db
        .query_row("select schema, extractor from codevitals", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?
    else {
        return Ok(());
    };
    ensure!(
        schema == SCHEMA,
        "{path:?} has layout version {schema}, this codevitals reads {SCHEMA}; {}",
        if schema > SCHEMA {
            "upgrade codevitals or use another database"
        } else {
            "delete it and scan again"
        }
    );
    ensure!(
        extractor <= V,
        "{path:?} was parsed by a newer codevitals (extractor {extractor}, this one is {V}); upgrade codevitals or use another database, or this one would reparse everything"
    );
    Ok(())
}
