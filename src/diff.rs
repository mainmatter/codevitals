//! `diff`: how each metric changes from one commit to another, and the files and
//! functions that account for the change.
//!
//! It needs no history scan: an in-memory database gets a two-commit history, the
//! whole tree of BASE (seq 0), then BASE's changes to HEAD (seq 1). Spans come from
//! the scanned database where it has them, so with a scan that reached BASE only the
//! blobs that differ are parsed; without one, BASE's tree is parsed too. The metric
//! views then run unchanged on that history.
//!
//! A metric `scale * N / T` (decisions per 100 tokens, nesting per function token,
//! ...) changes by `scale * (dN - v0 * dT) / T1` where v0 is BASE's value: each file
//! or function contributes `scale * (dN_u - v0 * dT_u) / T1`, and the contributions
//! add up to the change exactly. A sum (size) changes by the sum of its units'
//! changes. Units are functions (identified as in `units.functions`) and each file's
//! code outside functions.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use duckdb::types::{TimeUnit, Value};
use duckdb::{Connection, params};
use tracing::info;

use crate::attributes::Attributes;
use crate::walk::{Row, diff_trees, path_text};

/// Metrics that `cv validate` supports (item 15 of the plan decides). Every other
/// metric's values are marked unvalidated.
pub const VALIDATED: &[&str] = &[];

/// How a metric decomposes over units: `scale * sum(numerator) / sum(denominator)`
/// over the unit rows of its languages, or `scale * sum(numerator)` without a
/// denominator. Columns of `unit_rows`.
struct Spec {
    metric: &'static str,
    scale: f64,
    numerator: &'static str,
    denominator: Option<&'static str>,
    langs: Option<&'static str>,
}

const SPECS: &[Spec] = &[
    Spec {
        metric: "size",
        scale: 1.0,
        numerator: "tokens",
        denominator: None,
        langs: None,
    },
    Spec {
        metric: "encapsulation",
        scale: 1.0,
        numerator: "private_tokens",
        denominator: Some("fn_tokens"),
        langs: None,
    },
    Spec {
        metric: "concentration",
        scale: 1.0,
        numerator: "large_tokens",
        denominator: Some("fn_tokens"),
        langs: None,
    },
    Spec {
        metric: "decisions",
        scale: 100.0,
        numerator: "decisions",
        denominator: Some("tokens"),
        langs: None,
    },
    Spec {
        metric: "nesting",
        scale: 1.0,
        numerator: "nested_tokens",
        denominator: Some("fn_tokens"),
        langs: None,
    },
    Spec {
        metric: "global_state",
        scale: 1000.0,
        numerator: "global_state",
        denominator: Some("tokens"),
        langs: None,
    },
    Spec {
        metric: "exceptions",
        scale: 1000.0,
        numerator: "throws",
        denominator: Some("tokens"),
        langs: None,
    },
    Spec {
        metric: "invariants",
        scale: 1000.0,
        numerator: "invariants",
        denominator: Some("tokens"),
        langs: None,
    },
    Spec {
        metric: "unhandled",
        scale: 1000.0,
        numerator: "unhandled",
        denominator: Some("tokens"),
        langs: None,
    },
    Spec {
        metric: "unfinished",
        scale: 1000.0,
        numerator: "unfinished",
        denominator: Some("tokens"),
        langs: None,
    },
    Spec {
        metric: "goto",
        scale: 1000.0,
        numerator: "gotos",
        denominator: Some("tokens"),
        langs: Some("('c', 'cpp')"),
    },
];

/// Metrics `diff` reports without contributions: their units are not additive.
const WHOLE: &[&str] = &["duplication"];

/// Tail metrics of a per-function measure (a column of `unit_rows`): not additive, so
/// the functions listed are those whose measure changed most, by that change.
const TAILS: &[(&str, &str)] = &[
    ("function_decisions", "decisions"),
    ("function_nesting", "nesting"),
    ("function_params", "params"),
];

/// One unit's share of a metric's change.
struct Contribution {
    kind: String,
    id: String,
    path: String,
    name: String,
    value: f64,
}

/// A metric's change from BASE to HEAD.
struct Change {
    metric: String,
    /// Whether contributions add up to the change.
    additive: bool,
    base: Option<f64>,
    head: Option<f64>,
    functions: Vec<Contribution>,
    files: Vec<Contribution>,
}

/// The output formats of `cv diff`.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Format {
    Json,
    Text,
}

/// Each metric's change from `base` to `head` in `repo`, with the `top` functions and
/// files that account for most of it, as JSON or text. Spans come from the database
/// at `scanned` when it exists.
pub fn diff(
    repo: &gix::Repository,
    scanned: &Path,
    base: &str,
    head: &str,
    top: usize,
    format: Format,
) -> Result<String> {
    let resolve = |rev: &str| -> Result<gix::Commit<'_>> {
        repo.rev_parse_single(rev)
            .map_err(anyhow::Error::from)
            .and_then(|r| Ok(r.object()?.peel_to_commit()?))
            .with_context(|| format!("resolve {rev:?} to a commit"))
    };
    let (base, head) = (resolve(base)?, resolve(head)?);
    let mut db = Connection::open_in_memory()?;
    db.execute_batch(codevitals::SQL)
        .context("apply codevitals' SQL")?;
    record(&mut db, repo, &[&base, &head])?;
    let reused = reuse_spans(&db, scanned)?;
    let parsed = crate::parse::parse(&mut db, repo)?;
    info!(reused, parsed, "blobs");
    // Materializes the helper views once for every metric, deriving the cones too.
    crate::metrics::cache_helpers(&mut db)?;

    let mut changes = Vec::new();
    db.execute_batch(UNIT_ROWS)?;
    for metric in crate::metrics::list(&db)? {
        let values: Vec<(i32, Option<f64>)> = db
            .prepare(&format!(
                "select seq, value::double from metrics.{metric} where seq in (0, 1) order by seq"
            ))?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let at = |seq| values.iter().find(|v| v.0 == seq).and_then(|v| v.1);
        let (b, h) = (at(0), at(1));
        if let Some((_, measure)) = TAILS.iter().find(|t| t.0 == metric) {
            if b.is_some() || h.is_some() {
                let functions = measure_changes(&db, measure, top)?;
                changes.push(Change {
                    metric,
                    additive: false,
                    base: b,
                    head: h,
                    functions,
                    files: vec![],
                });
            }
            continue;
        }
        let Some(spec) = SPECS.iter().find(|s| s.metric == metric) else {
            if WHOLE.contains(&metric.as_str()) && (b.is_some() || h.is_some()) {
                changes.push(Change {
                    metric,
                    additive: false,
                    base: b,
                    head: h,
                    functions: vec![],
                    files: vec![],
                });
            }
            continue;
        };
        if b.is_none() && h.is_none() {
            continue;
        }
        let (functions, files) = contributions(&db, spec, top)?;
        changes.push(Change {
            metric,
            additive: true,
            base: b,
            head: h,
            functions,
            files,
        });
    }
    Ok(match format {
        Format::Json => json(&base, &head, &changes),
        Format::Text => text(&base, &head, &changes),
    })
}

/// Writes `commits` and `changes` for a two-commit history: every file of the first
/// commit's tree at seq 0, the changes to the second's at seq 1.
fn record(db: &mut Connection, repo: &gix::Repository, commits: &[&gix::Commit; 2]) -> Result<()> {
    let tx = db.transaction()?;
    {
        let mut rows = tx.appender("commits")?;
        for (seq, c) in commits.iter().enumerate() {
            let author = c.author()?;
            rows.append_row(params![
                seq,
                c.id.to_string(),
                Value::Timestamp(TimeUnit::Second, c.time()?.seconds),
                author.name.to_string(),
                author.email.to_string(),
                c.message_raw()?.to_string()
            ])?;
        }
    }
    let (base, head) = (commits[0].tree()?, commits[1].tree()?);
    let mut changes = tx.appender("changes")?;
    let files = |tree: &gix::Tree| -> Result<Vec<(String, String)>> {
        let mut recorder = gix::traverse::tree::Recorder::default();
        tree.traverse().breadthfirst(&mut recorder)?;
        Ok(recorder
            .records
            .into_iter()
            .filter(|e| e.mode.is_blob())
            .map(|e| (path_text(&e.filepath), e.oid.to_string()))
            .collect())
    };
    let attributes = |files: &[(String, String)]| -> Result<Attributes> {
        let mut a = Attributes::default();
        for (path, blob) in files.iter().filter(|(p, _)| Attributes::is_file(p)) {
            a.set(
                path,
                Some(repo.find_blob(crate::db::oid(blob))?.take_data()),
            );
        }
        Ok(a)
    };
    let base_files = files(&base)?;
    let a = attributes(&base_files)?;
    for (path, blob) in &base_files {
        let lines = None::<i32>; // BASE's files are no change: no lines counted
        changes.append_row(params![
            0,
            path,
            blob,
            a.role(path)?,
            a.test_features(path),
            None::<String>,
            lines,
            lines
        ])?;
    }
    let a = attributes(&files(&head)?)?;
    for (path, row) in diff_trees(repo, &base, &head)? {
        let Row {
            blob,
            source,
            lines,
            ..
        } = row;
        let (role, features) = match blob {
            Some(_) => (a.role(&path)?, a.test_features(&path)),
            None => (None, None),
        };
        let (added, deleted) = lines.unzip();
        changes.append_row(params![
            1, path, blob, role, features, source, added, deleted
        ])?;
    }
    drop(changes);
    tx.commit()?;
    Ok(())
}

/// Copies the spans of this history's blobs from the scanned database at `path`, if
/// it exists and was parsed by this extractor version. Returns the blobs copied.
fn reuse_spans(db: &Connection, path: &Path) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let quoted = path.to_string_lossy().replace('\'', "''");
    db.execute_batch(&format!("attach '{quoted}' as scanned (read_only)"))
        .with_context(|| format!("attach {path:?}"))?;
    let copied = db.execute(
        "insert into spans select s.* from scanned.spans s
         where s.v = ? and s.blob in (select blob from changes where blob is not null)",
        [crate::db::V],
    )?;
    db.execute_batch("detach scanned")?;
    let blobs: usize = db.query_row("select count(distinct blob) from spans", [], |r| r.get(0))?;
    ensure!(copied == 0 || blobs > 0, "copied spans belong to blobs");
    Ok(blobs)
}

/// Per-unit rows at seq 0 and 1: each production function, and each production
/// file's code outside functions, with the columns the metrics sum.
const UNIT_ROWS: &str = "
create temp table unit_rows as
with s as (select unnest([0, 1]) as s),
f as (select s.s, f.* from s join units.files f on f.since <= s.s and s.s < f.until),
fn as (select s.s, f.* from s join units.functions f on f.since <= s.s and s.s < f.until),
sums as (select s, lineage, sum(own_tokens) as tokens, sum(decisions) as decisions,
                sum(throws) as throws, sum(gotos) as gotos, sum(invariants) as invariants,
                sum(unhandled) as unhandled, sum(unfinished) as unfinished
         from fn group by all)
select s, 'function' as kind, function as id, lineage, path, qualified as name, lang,
       own_tokens as tokens, tokens as fn_tokens, decisions, throws, gotos, 0 as global_state,
       tokens * nesting as nested_tokens, if(tokens > large_tokens(), tokens, 0) as large_tokens,
       if(public, 0, tokens) as private_tokens, nesting, params, invariants, unhandled, unfinished
from fn
union all
select f.s, 'file', f.lineage || ' (outside functions)', f.lineage, f.path, '(outside functions)', f.lang,
       f.tokens - coalesce(u.tokens, 0), 0, f.decisions - coalesce(u.decisions, 0),
       f.throws - coalesce(u.throws, 0), f.gotos - coalesce(u.gotos, 0), f.global_state, 0, 0, 0,
       null, null, f.invariants - coalesce(u.invariants, 0), f.unhandled - coalesce(u.unhandled, 0),
       f.unfinished - coalesce(u.unfinished, 0)
from f left join sums u using (s, lineage);
";

/// The `top` functions and files contributing most (by absolute value) to the
/// change of `spec`'s metric.
fn contributions(
    db: &Connection,
    spec: &Spec,
    top: usize,
) -> Result<(Vec<Contribution>, Vec<Contribution>)> {
    let Spec {
        scale,
        numerator: n,
        denominator,
        langs,
        ..
    } = spec;
    let filter = langs.map_or(String::new(), |l| format!("and lang in {l}"));
    let d = denominator.unwrap_or("1");
    let share = match denominator {
        Some(_) => format!("{scale} * (dn - v0 * dd) / nullif(t1, 0)"),
        None => format!("{scale} * dn"),
    };
    let query = format!(
        "with totals as (
           select sum({n}) filter (s = 0) / nullif(sum({d}) filter (s = 0), 0) as v0,
                  sum({d}) filter (s = 1) as t1
           from unit_rows where true {filter}),
         units as (
           select kind, id, lineage, coalesce(arg_max(path, s), '') as path, any_value(name) as name,
                  coalesce(sum({n}) filter (s = 1), 0) - coalesce(sum({n}) filter (s = 0), 0) as dn,
                  coalesce(sum({d}) filter (s = 1), 0) - coalesce(sum({d}) filter (s = 0), 0) as dd
           from unit_rows where true {filter} group by kind, id, lineage),
         shares as (select u.*, coalesce({share}, 0) as share from units u, totals)
         select 'function', id, path, name, share from (
           select * from shares where share <> 0 order by abs(share) desc, id limit {top})
         union all
         select 'file', lineage, path, path, share from (
           select lineage, any_value(path) filter (kind = 'file') as path, sum(share) as share
           from shares group by lineage having sum(share) <> 0
           order by abs(sum(share)) desc, lineage limit {top})"
    );
    let rows: Vec<Contribution> = db
        .prepare(&query)?
        .query_map([], |r| {
            Ok(Contribution {
                kind: r.get(0)?,
                id: r.get(1)?,
                path: r.get(2)?,
                name: r.get(3)?,
                value: r.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    // Functions include each file's code outside functions, as `(outside functions)`.
    Ok(rows.into_iter().partition(|c| c.kind == "function"))
}

/// The `top` functions whose `measure` changed most, by that change (a function
/// added counts from 0, one removed to 0).
fn measure_changes(db: &Connection, measure: &str, top: usize) -> Result<Vec<Contribution>> {
    let query = format!(
        "select id, coalesce(arg_max(path, s), ''), any_value(name),
                coalesce(max({measure}) filter (s = 1), 0) - coalesce(max({measure}) filter (s = 0), 0) as change
         from unit_rows where kind = 'function' group by id
         having change <> 0 order by abs(change) desc, id limit {top}"
    );
    let rows = db
        .prepare(&query)?
        .query_map([], |r| {
            Ok(Contribution {
                kind: "function".into(),
                id: r.get(0)?,
                path: r.get(1)?,
                name: r.get(2)?,
                value: r.get::<_, i64>(3)? as f64,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

fn json_text(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).expect("write to String"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_number(v: Option<f64>) -> String {
    match v {
        Some(v) if v.is_finite() => format!("{v}"),
        _ => "null".into(),
    }
}

fn relative(c: &Change) -> Option<f64> {
    let (b, h) = (c.base?, c.head?);
    (b != 0.0).then(|| (h - b) / b.abs())
}

/// The change as one JSON object: base and head commits, then per metric its values
/// at both, the change, the relative change, whether the metric is validated, whether
/// contributions add up to the change (else they are each function's own change in
/// the measure), and the contributing functions and files.
fn json(base: &gix::Commit, head: &gix::Commit, changes: &[Change]) -> String {
    let units = |list: &[Contribution], function: bool| {
        let items: Vec<String> = list
            .iter()
            .map(|c| {
                let name = match function {
                    true => format!(
                        ",\"function\":{},\"name\":{}",
                        json_text(&c.id),
                        json_text(&c.name)
                    ),
                    false => format!(",\"lineage\":{}", json_text(&c.id)),
                };
                format!(
                    "{{\"path\":{}{name},\"contribution\":{}}}",
                    json_text(&c.path),
                    json_number(Some(c.value))
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    };
    let metrics: Vec<String> = changes
        .iter()
        .map(|c| {
            let change = c.base.zip(c.head).map(|(b, h)| h - b);
            format!(
                "{{\"metric\":{},\"validated\":{},\"additive\":{},\"base\":{},\"head\":{},\"change\":{},\"relative\":{},\"functions\":{},\"files\":{}}}",
                json_text(&c.metric),
                VALIDATED.contains(&c.metric.as_str()),
                c.additive,
                json_number(c.base),
                json_number(c.head),
                json_number(change),
                json_number(relative(c)),
                units(&c.functions, true),
                units(&c.files, false)
            )
        })
        .collect();
    format!(
        "{{\"base\":{},\"head\":{},\"metrics\":[{}]}}",
        json_text(&base.id.to_string()),
        json_text(&head.id.to_string()),
        metrics.join(",")
    )
}

/// The change as text: per metric that changed, a line with its values and relative
/// change, then its largest contributions (as a share of BASE's value; for tail
/// metrics, each function's own change).
fn text(base: &gix::Commit, head: &gix::Commit, changes: &[Change]) -> String {
    let short = |c: &gix::Commit| c.id.to_string()[..10].to_owned();
    let mut out = format!("{} -> {}\n", short(base), short(head));
    let fmt = |v: Option<f64>| v.map_or("-".to_owned(), |v| format!("{v:.4}"));
    let width = changes.iter().map(|c| c.metric.len()).max().unwrap_or(0);
    for c in changes {
        let rel = relative(c).map_or(String::new(), |r| format!(" ({:+.1}%)", 100.0 * r));
        let mark = match VALIDATED.contains(&c.metric.as_str()) {
            true => "",
            false => "  unvalidated",
        };
        let same = c.base == c.head;
        writeln!(
            out,
            "{:width$}  {} -> {}{rel}{mark}",
            c.metric,
            fmt(c.base),
            fmt(c.head)
        )
        .expect("write to String");
        if same {
            continue;
        }
        let base = c.base.filter(|b| *b != 0.0 && c.additive);
        let share = |v: f64| match base {
            Some(b) => format!("{:+.1}%", 100.0 * v / b.abs()),
            None => format!("{v:+.4}"),
        };
        for f in c.functions.iter().take(3) {
            writeln!(out, "  {:>8}  {}  {}", share(f.value), f.path, f.name)
                .expect("write to String");
        }
        for f in c.files.iter().take(3) {
            writeln!(out, "  {:>8}  {} (file)", share(f.value), f.path).expect("write to String");
        }
    }
    out
}
