//! Metrics are the views of schema `metrics`, defined in `sql/metrics.sql` and applied
//! to every database tally opens. Their comments, stored in the database, explain them.

use std::time::Instant;

use anyhow::{Context, Result};
use clap::builder::{PossibleValue, PossibleValuesParser};
use duckdb::Connection;
use tracing::{debug, info};

/// A view's position in `tally::SQL` (the parameter): 0 for a view tally does not
/// define, such as one a user added. Ordering by it puts each view after the views it
/// reads; `view_oid` will not do, as a database reloaded from disk renumbers its views.
const POSITION: &str = "strpos(?, 'create or replace view '
    || if(schema_name = 'main', '', schema_name || '.') || view_name || ' as')";

/// Each metric view's name and explanation, in definition order.
fn catalog(db: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = db.prepare(&format!(
        "select view_name, comment from duckdb_views()
         where schema_name = 'metrics' and {POSITION} > 0 order by {POSITION}"
    ))?;
    let rows = stmt.query_map([tally::SQL, tally::SQL], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let rows: Vec<_> = rows.collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(name, about)| {
            let about = about
                .unwrap_or_else(|| panic!("metric view {name} has no comment in sql/metrics.sql"));
            (name, about)
        })
        .collect())
}

/// Whether `name` is a metric view.
pub fn exists(db: &Connection, name: &str) -> Result<bool> {
    Ok(catalog(db)?.iter().any(|(m, _)| m == name))
}

/// Metric names for clap, so `--help` lists every metric with its title: the first
/// line of its comment (`CATEGORY: Title`), without parenthetical asides, which the
/// dashboard keeps.
pub fn names() -> PossibleValuesParser {
    let db = Connection::open_in_memory().expect("in-memory database opens");
    db.execute_batch(tally::SQL).expect("tally's SQL applies");
    let metrics = catalog(&db).expect("metric views are listed");
    PossibleValuesParser::new(metrics.into_iter().map(|(name, about)| {
        let (_, title) = about
            .lines()
            .next()
            .and_then(|line| line.split_once(": "))
            .unwrap_or_else(|| panic!("{name}'s comment starts with `CATEGORY: Title`"));
        let mut short = String::new();
        let mut depth = 0;
        for c in title.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                c if depth == 0 => short.push(c),
                _ => {}
            }
        }
        PossibleValue::new(name).help(short.split_whitespace().collect::<Vec<_>>().join(" "))
    }))
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
             where not internal and schema_name = 'main'
               and {POSITION} > 0
             order by {POSITION}"
        ))?
        .query_map([tally::SQL, tally::SQL], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for view in &helpers {
        let caching = Instant::now();
        db.execute_batch(&format!(
            "create temp table \"{view}\" as select * from main.\"{view}\""
        ))
        .with_context(|| format!("compute {view}"))?;
        debug!(%view, elapsed = ?caching.elapsed(), "cached");
        if view == "import_edges" {
            crate::graph::derive(db, crate::graph::Cones::Temp)?;
        }
    }
    info!(helpers = helpers.len(), elapsed = ?started.elapsed(), "helpers cached");
    Ok(())
}

/// One JSON object mapping each selected metric (all when `names` is empty) to its
/// rows, each with the commit `time` (ISO 8601 UTC) after `seq`.
pub fn export(db: &mut Connection, names: &[String]) -> Result<String> {
    cache_helpers(db)?;
    let (code_commits, windows, size, weeks): (i64, i64, i64, i64) = db.query_row(
        "select (select count(*) from code_commits), (select count(distinct win) from windows),
                window_commits(), window_max_weeks()",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if windows == 0 {
        tracing::warn!(
            code_commits,
            "the windowed metrics are empty: each needs {size} commits that touch production \
             code within {weeks} weeks, and this history has none"
        );
    }
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
             from metrics.{m} m join commit_times c using (seq) order by m.seq"
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
