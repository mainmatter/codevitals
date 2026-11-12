//! `units`: the per-file and per-function values (`sql/units.sql`) at one commit.

use anyhow::{Context, Result};
use duckdb::{Connection, OptionalExt, params};

use crate::util::Format;

/// One JSON object with the commit (`seq`, `id`) and the production `files` and
/// `functions` live at the stored commit `rev` (an id or prefix of one; the newest
/// when `None`), each an array of row objects.
pub fn export(db: &mut Connection, rev: Option<&str>) -> Result<String> {
    let (seq, id): (i32, String) = match rev {
        Some(rev) => db
            .query_row(
                "select seq, id from commits where starts_with(id, lower($1)) order by seq",
                params![rev],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .with_context(|| format!("{rev:?} is no scanned commit; pass a commit id (or a prefix of one) that `cv scan` recorded"))?,
        None => crate::db::tip(db)?,
    };
    crate::metrics::cache_helpers(db, &["units.files".into(), "units.functions".into()])?;
    let rows = |view: &str, order: &str| -> Result<String> {
        Ok(db.query_row(
            &format!(
                "select coalesce(to_json(list(t order by {order})), '[]') from (
                   select * exclude (since, until) from units.{view}
                   where since <= $1 and $1 < until) t"
            ),
            params![seq],
            |r| r.get(0),
        )?)
    };
    let files = rows("files", "path")?;
    let functions = rows("functions", "path, start_byte")?;
    Ok(format!(
        r#"{{"seq":{seq},"id":"{id}","files":{files},"functions":{functions}}}"#
    ))
}

/// The `top` hotspots at the newest scanned commit (`units.hotspots`), as JSON
/// (`{"seq":..,"id":..,"days":..,"validated":false,"files":[..]}`) or as text.
pub fn hotspots(db: &mut Connection, top: usize, format: Format) -> Result<String> {
    crate::metrics::cache_helpers(db, &["units.hotspots".into()])?;
    let (seq, id) = crate::db::tip(db)?;
    let days: i32 = db.query_row("select hotspot_days()", [], |r| r.get(0))?;
    let query =
        format!("select * from units.hotspots where score > 0 order by rank, path limit {top}");
    if matches!(format, Format::Json) {
        let files: String = db.query_row(
            &format!(
                "select coalesce(to_json(list(t order by t.rank, t.path)), '[]') from ({query}) t"
            ),
            [],
            |r| r.get(0),
        )?;
        return Ok(format!(
            r#"{{"seq":{seq},"id":"{id}","days":{days},"validated":false,"files":{files}}}"#
        ));
    }
    let mut out = format!(
        "hotspots at {}: changes in the last {days} days x decision points (unvalidated)\n",
        &id[..10]
    );
    let rows: Vec<(i64, String, i64, i64, i64)> = db
        .prepare(&format!(
            "select rank, path, changes, decisions, score from ({query})"
        ))?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    for (rank, path, changes, decisions, score) in rows {
        out.push_str(&format!(
            "{rank:>4}  {score:>7}  {changes:>4} changes x {decisions:>4} decisions  {path}\n"
        ));
    }
    Ok(out)
}
