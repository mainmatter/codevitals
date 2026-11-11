//! Metrics are the views named `m_*` in `metrics.sql`, applied to every database
//! tally opens. Their comments, stored in the database, explain them.

use std::time::Instant;

use anyhow::{Context, Result};
use clap::builder::{PossibleValue, PossibleValuesParser};
use duckdb::Connection;
use tracing::{debug, info};

pub const SQL: &str = include_str!("metrics.sql");

/// Orders views from `duckdb_views()` as `metrics.sql` (the parameter) defines them,
/// so each comes after the views it reads; the views of `schema.sql` come first.
/// `view_oid` will not do: a database reloaded from disk renumbers its views.
const DEFINITION_ORDER: &str = "strpos(?, 'create or replace view ' || view_name || ' as')";

/// Each metric view's name and explanation, in definition order.
fn catalog(db: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = db.prepare(&format!(
        "select view_name, comment from duckdb_views()
         where starts_with(view_name, 'm_') order by {DEFINITION_ORDER}"
    ))?;
    let rows = stmt.query_map([SQL], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let rows: Vec<_> = rows.collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(name, about)| {
            let about =
                about.unwrap_or_else(|| panic!("metric view {name} has no comment in metrics.sql"));
            (name, about)
        })
        .collect())
}

/// Whether `name` is a metric view.
pub fn exists(db: &Connection, name: &str) -> Result<bool> {
    Ok(catalog(db)?.iter().any(|(m, _)| m == name))
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

/// Materializes every helper view, in definition order, as a temp table of the same
/// name. The temp table shadows the view for this connection only, so metrics that
/// share a helper compute it once. `cones` derive from `import_edges`, so they are
/// rebuilt as soon as it is cached, before the helpers that read them.
fn cache_helpers(db: &mut Connection) -> Result<()> {
    let started = Instant::now();
    let helpers: Vec<String> = db
        .prepare(&format!(
            "select view_name from duckdb_views()
             where not internal and schema_name = 'main' and not starts_with(view_name, 'm_')
             order by {DEFINITION_ORDER}"
        ))?
        .query_map([SQL], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for view in &helpers {
        let caching = Instant::now();
        db.execute_batch(&format!(
            "create temp table {view} as select * from main.{view}"
        ))
        .with_context(|| format!("compute {view}"))?;
        debug!(%view, elapsed = ?caching.elapsed(), "cached");
        if view == "import_edges" {
            crate::graph::derive(db)?;
        }
    }
    info!(helpers = helpers.len(), elapsed = ?started.elapsed(), "helpers cached");
    Ok(())
}

/// One JSON object mapping each selected metric (all when `names` is empty) to its
/// rows, each with the commit `time` (ISO 8601 UTC) after `seq`.
pub fn export(db: &mut Connection, names: &[String]) -> Result<String> {
    cache_helpers(db)?;
    let names = match names.is_empty() {
        true => catalog(db)?.into_iter().map(|(name, _)| name).collect(),
        false => names.to_vec(),
    };
    let mut fields = Vec::new();
    for (i, m) in names.iter().enumerate() {
        assert!(
            m.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "metric names are identifiers"
        );
        let started = Instant::now();
        let rows = format!(
            "select m.seq, strftime(c.time, '%Y-%m-%dT%H:%M:%SZ') as time, m.* exclude (seq)
             from {m} m join commits c using (seq) order by m.seq"
        );
        let (count, json): (usize, String) = db
            .query_row(
                &format!("select count(*), coalesce(json_group_array(t), '[]') from ({rows}) t"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .with_context(|| format!("compute {m}"))?;
        info!(metric = %m, rows = count, elapsed = ?started.elapsed(), "metric {}/{}", i + 1, names.len());
        fields.push(format!("\"{m}\":{json}"));
    }
    Ok(format!("{{{}}}", fields.join(",")))
}
