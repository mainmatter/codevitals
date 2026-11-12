//! Metrics are the views of schema `metrics`, defined in `sql/metrics.sql` and applied
//! to every database codevitals opens. Their comments, stored in the database, explain them.

use std::time::Instant;

use anyhow::{Context, Result};
use clap::builder::{PossibleValue, PossibleValuesParser};
use duckdb::{Connection, params};
use tracing::{debug, info, warn};

/// A view's position in `codevitals::SQL` (`$1`): 0 for a view codevitals does not
/// define, such as one a user added. Ordering by it puts each view after the views it
/// reads; `view_oid` will not do, as a database reloaded from disk renumbers its views.
const POSITION: &str = "strpos($1, 'create or replace view '
    || if(schema_name = 'main', '', schema_name || '.') || view_name || ' as')";

/// The views codevitals defines in `schema`, with their comments, in definition order.
fn views(db: &Connection, schema: &str) -> Result<Vec<(String, Option<String>)>> {
    let mut stmt = db.prepare(&format!(
        "select view_name, comment from duckdb_views()
         where not internal and schema_name = $2 and {POSITION} > 0 order by {POSITION}"
    ))?;
    let views = stmt.query_map(params![codevitals::SQL, schema], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?;
    Ok(views.collect::<Result<_, _>>()?)
}

/// Each metric view's name and explanation, in definition order.
fn catalog(db: &Connection) -> Result<Vec<(String, String)>> {
    Ok(views(db, "metrics")?
        .into_iter()
        .map(|(name, about)| {
            let about = about
                .unwrap_or_else(|| panic!("metric view {name} has no comment in sql/metrics.sql"));
            (name, about)
        })
        .collect())
}

/// Every view's name in `schema` (`metrics` or `experimental`), in definition order.
pub fn list(db: &Connection, schema: &str) -> Result<Vec<String>> {
    Ok(views(db, schema)?
        .into_iter()
        .map(|(name, _)| name)
        .collect())
}

/// Metric names for clap, so `--help` lists every metric with its title: the first
/// line of its comment (`CATEGORY: Title`), without parenthetical asides, which the
/// dashboard keeps.
pub fn names() -> PossibleValuesParser {
    let db = Connection::open_in_memory().expect("in-memory database opens");
    db.execute_batch(codevitals::SQL)
        .expect("codevitals' SQL applies");
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

/// Whether `text` names `name` as a whole identifier.
fn mentions(text: &str, name: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(name)
        .any(|(i, _)| !text[..i].ends_with(ident) && !text[i + name.len()..].starts_with(ident))
}

/// Materializes the helper views (schema `main`) that the views `roots` (qualified,
/// e.g. `metrics.size`, or a helper's bare name) read, directly or through other
/// views and macros, in definition order, each as a temp table of the same name. The
/// temp table shadows the view for this connection only, so the views that share a
/// helper compute it once. `cones` derive from `import_edges`, so they are rebuilt
/// as soon as it is cached, before the helpers that read them.
pub fn cache_helpers(db: &mut Connection, roots: &[String]) -> Result<()> {
    let started = Instant::now();
    let all = views(db, "main")?;
    let definitions: Vec<(String, String)> = db
        .prepare(
            "select if(schema_name = 'main', '', schema_name || '.') || view_name, sql
             from duckdb_views() where not internal
             union all
             select function_name, macro_definition from duckdb_functions()
             where function_type in ('macro', 'table_macro') and not internal",
        )?
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })?
        .collect::<Result<_, _>>()?;
    // Everything the roots read, transitively: grow the text until no new name shows.
    let mut used: Vec<bool> = definitions.iter().map(|(n, _)| roots.contains(n)).collect();
    let mut text: String = roots.iter().map(|r| format!(" {r} ")).collect();
    for (i, (_, sql)) in definitions.iter().enumerate() {
        if used[i] {
            text.push_str(sql);
        }
    }
    loop {
        let found: Vec<usize> = (0..definitions.len())
            .filter(|&i| !used[i] && mentions(&text, &definitions[i].0))
            .collect();
        if found.is_empty() {
            break;
        }
        for i in found {
            used[i] = true;
            text.push_str(&definitions[i].1);
        }
    }
    let helpers: Vec<&String> = all
        .iter()
        .map(|(view, _)| view)
        .filter(|view| {
            definitions
                .iter()
                .zip(&used)
                .any(|((n, _), u)| *u && n == *view)
        })
        .collect();
    for view in &helpers {
        let caching = Instant::now();
        db.execute_batch(&format!(
            "create temp table \"{view}\" as select * from main.\"{view}\""
        ))
        .with_context(|| format!("compute {view}"))?;
        debug!(%view, elapsed = ?caching.elapsed(), "cached");
        if *view == "import_edges" {
            crate::graph::derive(db, crate::graph::Cones::Temp)?;
        }
    }
    info!(helpers = helpers.len(), elapsed = ?started.elapsed(), "helpers cached");
    Ok(())
}

/// One JSON object mapping each view of `schema` in `names` to its rows, each with the
/// commit `time` (ISO 8601 UTC) after `seq`.
pub fn export(db: &mut Connection, schema: &str, names: &[String]) -> Result<String> {
    let roots: Vec<String> = names.iter().map(|m| format!("{schema}.{m}")).collect();
    cache_helpers(db, &roots)?;
    let (code_commits, windows, size, weeks): (i64, i64, i64, i64) = db.query_row(
        "select (select count(*) from code_commits), (select count(distinct win) from windows),
                window_commits(), window_max_weeks()",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if windows == 0 {
        warn!(
            code_commits,
            "the windowed metrics are empty: each needs {size} commits that touch production \
             code within {weeks} weeks, and this history has none"
        );
    }
    let mut fields = Vec::new();
    for (i, m) in names.iter().enumerate() {
        assert!(
            m.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "metric names are identifiers"
        );
        let started = Instant::now();
        let rows = format!(
            "select m.seq, strftime(c.time, '%Y-%m-%dT%H:%M:%SZ') as time, m.* exclude (seq)
             from {schema}.{m} m join commit_times c using (seq) order by m.seq"
        );
        let (count, json): (usize, String) = db
            .query_row(
                &format!(
                    "select count(*), coalesce(to_json(list(t order by t.seq)), '[]') from ({rows}) t"
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .with_context(|| format!("compute {m}"))?;
        info!(metric = %m, rows = count, elapsed = ?started.elapsed(), "metric {}/{}", i + 1, names.len());
        fields.push(format!("\"{m}\":{json}"));
    }
    Ok(format!("{{{}}}", fields.join(",")))
}
