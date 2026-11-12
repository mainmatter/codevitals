//! `units`: the per-file and per-function values (`sql/units.sql`) at one commit.

use anyhow::{Context, Result};
use duckdb::{Connection, OptionalExt, params};

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
        None => db.query_row(
            "select seq, id from commits order by seq desc limit 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?,
    };
    crate::metrics::cache_helpers(db)?;
    let rows = |view: &str, order: &str| -> Result<String> {
        Ok(db.query_row(
            &format!(
                "select coalesce(json_group_array(t), '[]') from (
                   select * exclude (since, until) from units.{view}
                   where since <= $1 and $1 < until order by {order}) t"
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
