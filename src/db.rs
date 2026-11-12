//! The database: opening it, its versions, and bringing its views up to date.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use duckdb::{AccessMode, Connection, OptionalExt};
use gix::ObjectId;
use xxhash_rust::xxh3::xxh3_64;

/// Extractor version. Bump whenever `extract` output could change for any input.
pub const V: i32 = 2;
/// Database layout version. Bump whenever a table changes; older databases are refused.
pub const SCHEMA: i32 = 3;
/// Commits or blobs per transaction. A transaction costs milliseconds whatever its
/// size, so batching dominates throughput; an interrupt loses at most one batch.
pub const BATCH: usize = 500;
/// DuckDB's memory ceiling and spill-to-disk ceiling for every query. Above
/// them a query fails instead of exhausting the machine's memory or disk.
const MEMORY_LIMIT: &str = "4GB";
const SPILL_LIMIT: &str = "4GB";

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
    let connect = |access| -> Result<Connection> {
        let config = duckdb::Config::default().access_mode(access)?;
        let db = Connection::open_with_flags(path, config).with_context(|| {
            format!(
                "open database {path:?} (another codevitals may be using it; DuckDB allows one writer)"
            )
        })?;
        db.execute_batch(&format!(
            "set memory_limit = '{MEMORY_LIMIT}'; set max_temp_directory_size = '{SPILL_LIMIT}'"
        ))?;
        Ok(db)
    };
    let mut db = connect(access.clone())?;
    check_version(&db, path)?;
    let hash = format!("{:016x}", xxh3_64(codevitals::SQL.as_bytes()));
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
        return Ok(db);
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
    db.execute_batch(codevitals::SQL)
        .context("apply codevitals' SQL")?;
    db.execute_batch(&format!("create macro codevitals_sql() as '{hash}'"))?;
    Ok(db)
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
