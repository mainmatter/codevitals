//! Metrics are the views named `m_*` in `metrics.sql`, applied to every database
//! tally opens. Their comments, stored in the database, explain them.

use anyhow::Result;
use clap::builder::{PossibleValue, PossibleValuesParser};
use duckdb::Connection;

pub const SQL: &str = include_str!("metrics.sql");

/// Each metric view's name and explanation, in definition order.
fn catalog(db: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = db.prepare(
        "select view_name, comment from duckdb_views()
         where starts_with(view_name, 'm_') order by view_oid",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Metric names for clap, so `--help` lists every metric with its explanation.
pub fn names() -> PossibleValuesParser {
    let db = Connection::open_in_memory().expect("in-memory database opens");
    db.execute_batch(include_str!("schema.sql"))
        .expect("schema applies");
    db.execute_batch(SQL).expect("metrics.sql applies");
    let metrics = catalog(&db).expect("metric views are listed");
    PossibleValuesParser::new(
        metrics
            .into_iter()
            .map(|(name, about)| PossibleValue::new(name).help(about)),
    )
}

/// One JSON object mapping each selected metric (all when `names` is empty) to its
/// rows, each with the commit `time` (ISO 8601 UTC) after `seq`.
pub fn export(db: &Connection, names: &[String]) -> Result<String> {
    let names = match names.is_empty() {
        true => catalog(db)?.into_iter().map(|(name, _)| name).collect(),
        false => names.to_vec(),
    };
    let fields: Vec<_> = names
        .iter()
        .map(|m| {
            let rows = format!(
                "select m.seq, strftime(c.time, '%Y-%m-%dT%H:%M:%SZ') as time, m.* exclude (seq)
                 from {m} m join commits c using (seq) order by m.seq"
            );
            format!("'{m}', (select coalesce(json_group_array(t), '[]') from ({rows}) t)")
        })
        .collect();
    let sql = format!("select json_object({})", fields.join(", "));
    Ok(db.query_row(&sql, [], |r| r.get(0))?)
}
