//! `cv validate`: whether a metric measures what raters judge harder to change.
//!
//! For each per-function metric: of the decided pairs (left or right, not "can't
//! tell") whose two functions differ in the metric, the share in which the function
//! with the higher value is the one the rater would rather not change. 0.5 is
//! chance. Beside it the same share for token count, the baseline any metric must
//! beat, since longer code is harder to change by itself. Intervals are 95%
//! percentile bootstraps over judgments; "beats tokens" is a paired bootstrap of
//! the difference over the judgments both metrics decide. A metric is supported only
//! if its interval excludes 0.5 and the difference's interval lies above 0.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use duckdb::Connection;

use crate::util::{Rng, json_number, json_text};
use xxhash_rust::xxh3::xxh3_64;

/// Per-function metrics validated against labels: columns of `function_facts`, the
/// first being the baseline.
pub const METRICS: &[&str] = &[
    "tokens",
    "decisions",
    "nesting",
    "params",
    "throws",
    "invariants",
    "unhandled",
    "unfinished",
];

const RESAMPLES: usize = 2000;

/// One decided judgment: per metric, +1 when the function with the higher value is
/// the one not chosen (the rater would rather not change it), -1 when it is the one
/// chosen, 0 when both functions have the same value.
type Judgment = Vec<i8>;

/// A metric's result.
pub struct Support {
    pub metric: String,
    pub pairs: usize,
    pub share: f64,
    pub low: f64,
    pub high: f64,
    /// Paired difference to the baseline (tokens): share minus baseline share.
    pub vs_tokens: Option<(f64, f64, f64)>,
    pub supported: bool,
}

/// Agreement of one kind: the share of comparable judgments that agree, and how many.
pub struct Agreement {
    pub who: String,
    pub agree: usize,
    pub of: usize,
}

pub struct Report {
    pub defects: Defects,
    pub labels: usize,
    pub decided: usize,
    pub unknown: usize,
    pub metrics: Vec<Support>,
    pub self_agreement: Vec<Agreement>,
    pub between: Option<Agreement>,
    pub kappa: Option<f64>,
}

/// Rankings of files the defect check compares: columns of `units.defect_windows`;
/// random (a fixed pseudo-random order), tokens (size) and changes are the baselines.
pub const RANKINGS: &[&str] = &[
    "random",
    "tokens",
    "changes",
    "decisions",
    "max_decisions",
    "max_nesting",
    "max_params",
    "throws",
    "unhandled",
    "invariants",
    "global_state",
    "hotspot",
];

const BASELINES: &[&str] = &["random", "tokens", "changes"];

/// Share of tokens inspected, in ranking order, at which fixes found are counted.
const EFFORT: f64 = 0.2;

/// The defect check: per ranking, the share of next-window fix touches in the files
/// that make up the top EFFORT of tokens, over every cut (pooled) and on average.
pub struct Defects {
    pub cuts: usize,
    pub window_days: i64,
    pub fixes: i64,
    /// Ranking, pooled share, mean share over cuts with fixes.
    pub found: Vec<(String, f64, f64)>,
}

/// A file at a cut: its lineage's hash (the tie order), tokens, fixes after the cut,
/// and its value per ranking.
type CutFile = (u64, f64, i64, Vec<f64>);

fn defects(db: &Connection) -> Result<Defects> {
    let window_days: i64 = db.query_row("select defect_window_days()", [], |r| r.get(0))?;
    let query = format!(
        "select k, lineage, tokens, fixes, {} from units.defect_windows",
        RANKINGS.join(", ")
    );
    // Per cut, in order (grouped here: see `db::open` on ORDER BY): (lineage, tokens,
    // fixes, values per ranking).
    let mut by_cut = std::collections::BTreeMap::<i64, Vec<CutFile>>::new();
    let mut stmt = db.prepare(&query)?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let lineage: String = r.get(1)?;
        let values = (0..RANKINGS.len())
            .map(|i| r.get::<_, Option<f64>>(4 + i).map(|v| v.unwrap_or(0.0)))
            .collect::<duckdb::Result<_>>()?;
        by_cut.entry(r.get(0)?).or_default().push((
            xxh3_64(lineage.as_bytes()),
            r.get(2)?,
            r.get(3)?,
            values,
        ));
    }
    let cuts: Vec<Vec<CutFile>> = by_cut.into_values().collect();
    let fixes: i64 = cuts.iter().flatten().map(|f| f.2).sum();
    let found = (0..RANKINGS.len())
        .map(|i| {
            let (mut hit, mut total, mut shares) = (0i64, 0i64, Vec::new());
            for files in &cuts {
                let mut order: Vec<&CutFile> = files.iter().collect();
                // Highest first; ties in a fixed pseudo-random order.
                order.sort_by(|a, b| b.3[i].total_cmp(&a.3[i]).then(a.0.cmp(&b.0)));
                let tokens: f64 = files.iter().map(|f| f.1).sum();
                let (mut seen, mut found) = (0.0, 0i64);
                for f in order {
                    if seen >= EFFORT * tokens {
                        break;
                    }
                    seen += f.1;
                    found += f.2;
                }
                let all: i64 = files.iter().map(|f| f.2).sum();
                hit += found;
                total += all;
                if all > 0 {
                    shares.push(found as f64 / all as f64);
                }
            }
            let pooled = if total > 0 {
                hit as f64 / total as f64
            } else {
                f64::NAN
            };
            let mean = if shares.is_empty() {
                f64::NAN
            } else {
                shares.iter().sum::<f64>() / shares.len() as f64
            };
            (RANKINGS[i].to_owned(), pooled, mean)
        })
        .collect();
    Ok(Defects {
        cuts: cuts.len(),
        window_days,
        fixes,
        found,
    })
}

/// The caveat the defect check always states.
const FIX_RECALL: &str =
    "is_fix finds fixes by commit subject; its recall may be as low as 0.6 (validation/README.md)";

/// Loads the labels file into a temp table and computes the report.
pub fn validate(db: &mut Connection, file: &Path) -> Result<Report> {
    crate::metrics::cache_helpers(
        db,
        &["function_facts".into(), "units.defect_windows".into()],
    )?;
    let labels = crate::labels::load(db, file)?;
    // Each label's two functions with their metrics (any declared test features: the
    // facts of a function do not depend on them unless it is test code).
    let columns: Vec<String> = METRICS
        .iter()
        .map(|m| format!("a.{m}::double, b.{m}::double"))
        .collect();
    let query = format!(
        "with f as (
           select blob, start_byte, end_byte, any_value(columns(['{}'])) from function_facts
           group by blob, start_byte, end_byte)
         select l.choice, a.blob is not null and b.blob is not null, {}
         from labels l
         left join f a on a.blob = l.a_blob and a.start_byte = l.a_start and a.end_byte = l.a_end
         left join f b on b.blob = l.b_blob and b.start_byte = l.b_start and b.end_byte = l.b_end
         order by l.rater, l.n",
        METRICS.join("', '"),
        columns.join(", ")
    );
    let mut judgments = Vec::new();
    let (mut decided, mut unknown) = (0, 0);
    let mut stmt = db.prepare(&query)?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let choice: String = r.get(0)?;
        let known: bool = r.get(1)?;
        if !known {
            unknown += 1;
            continue;
        }
        if choice == "same" {
            continue;
        }
        decided += 1;
        let signs = (0..METRICS.len())
            .map(|i| -> duckdb::Result<i8> {
                // A metric a function lacks (no parameter list: params is NULL)
                // decides nothing.
                let (a, b): (Option<f64>, Option<f64>) = (r.get(2 + 2 * i)?, r.get(3 + 2 * i)?);
                let (Some(a), Some(b)) = (a, b) else {
                    return Ok(0);
                };
                // The function the rater would rather not change: the one not chosen.
                let (kept, chosen) = if choice == "a" { (b, a) } else { (a, b) };
                Ok(match kept.partial_cmp(&chosen) {
                    Some(std::cmp::Ordering::Greater) => 1,
                    Some(std::cmp::Ordering::Less) => -1,
                    _ => 0,
                })
            })
            .collect::<duckdb::Result<_>>()?;
        judgments.push(signs);
    }
    drop(rows);
    drop(stmt);
    let metrics = (0..METRICS.len()).map(|i| support(&judgments, i)).collect();
    let (self_agreement, between, kappa) = agreement(db)?;
    let defects = defects(db)?;
    Ok(Report {
        defects,
        labels,
        decided,
        unknown,
        metrics,
        self_agreement,
        between,
        kappa,
    })
}

/// The share of judgments `idx` (decided by `metric`) in which the higher value is
/// kept: wins / (wins + losses).
fn share(judgments: &[Judgment], idx: &[usize], metric: usize) -> Option<f64> {
    let (mut wins, mut n) = (0usize, 0usize);
    for &i in idx {
        match judgments[i][metric] {
            1 => {
                wins += 1;
                n += 1
            }
            -1 => n += 1,
            _ => {}
        }
    }
    (n > 0).then(|| wins as f64 / n as f64)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let k = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[k]
}

fn support(judgments: &[Judgment], metric: usize) -> Support {
    let all: Vec<usize> = (0..judgments.len()).collect();
    let pairs = all.iter().filter(|&&i| judgments[i][metric] != 0).count();
    let point = share(judgments, &all, metric).unwrap_or(f64::NAN);
    // Paired with tokens: judgments both decide.
    let both: Vec<usize> = all
        .iter()
        .copied()
        .filter(|&i| judgments[i][metric] != 0 && judgments[i][0] != 0)
        .collect();
    let mut rng = Rng::new(METRICS[metric]);
    let mut next = |n: usize| rng.below(n);
    let (mut shares, mut diffs) = (Vec::new(), Vec::new());
    if !all.is_empty() {
        for _ in 0..RESAMPLES {
            let sample: Vec<usize> = (0..all.len()).map(|_| all[next(all.len())]).collect();
            if let Some(s) = share(judgments, &sample, metric) {
                shares.push(s);
            }
            if metric != 0 && !both.is_empty() {
                let sample: Vec<usize> = (0..both.len()).map(|_| both[next(both.len())]).collect();
                if let (Some(m), Some(t)) = (
                    share(judgments, &sample, metric),
                    share(judgments, &sample, 0),
                ) {
                    diffs.push(m - t);
                }
            }
        }
    }
    shares.sort_by(f64::total_cmp);
    diffs.sort_by(f64::total_cmp);
    let (low, high) = match shares.is_empty() {
        true => (f64::NAN, f64::NAN),
        false => (percentile(&shares, 0.025), percentile(&shares, 0.975)),
    };
    let vs_tokens = (!diffs.is_empty()).then(|| {
        let d = share(judgments, &both, metric).unwrap() - share(judgments, &both, 0).unwrap();
        (d, percentile(&diffs, 0.025), percentile(&diffs, 0.975))
    });
    let excludes_chance = low > 0.5 || high < 0.5;
    let beats = vs_tokens.is_some_and(|(_, lo, _)| lo > 0.0);
    Support {
        metric: METRICS[metric].to_owned(),
        pairs,
        share: point,
        low,
        high,
        vs_tokens,
        supported: metric != 0 && excludes_chance && beats && point > 0.5,
    }
}

type Pair = ((String, i64, i64), (String, i64, i64));
/// A stored judgment: rater, n, a (blob, start, end), b (blob, start, end), choice,
/// repeat_of.
type Row = (
    String,
    i64,
    String,
    i64,
    i64,
    String,
    i64,
    i64,
    String,
    Option<i64>,
);

/// Self-agreement per rater on repeated pairs, and agreement between raters on
/// pairs several judged, with Cohen's kappa over the pooled rater pairs.
fn agreement(db: &Connection) -> Result<(Vec<Agreement>, Option<Agreement>, Option<f64>)> {
    // Each judgment as (rater, n, pair key, the chosen function or None for same).
    let rows: Vec<Row> = db
        .prepare(
            "select rater, n, a_blob, a_start, a_end, b_blob, b_start, b_end, choice, repeat_of
             from labels order by rater, n",
        )?
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    type Fn = (String, i64, i64);
    let judged = |r: &Row| -> (Pair, Option<Fn>) {
        let a: Fn = (r.2.clone(), r.3, r.4);
        let b: Fn = (r.5.clone(), r.6, r.7);
        let chosen = match r.8.as_str() {
            "a" => Some(a.clone()),
            "b" => Some(b.clone()),
            _ => None,
        };
        let pair = if a < b { (a, b) } else { (b, a) };
        (pair, chosen)
    };
    let mut by_rater: HashMap<(String, i64), Option<Fn>> = HashMap::new();
    let mut self_counts: HashMap<String, (usize, usize)> = HashMap::new();
    let mut first: HashMap<(String, Pair), Option<Fn>> = HashMap::new();
    for r in &rows {
        let (pair, chosen) = judged(r);
        by_rater.insert((r.0.clone(), r.1), chosen.clone());
        if let Some(earlier) = r.9 {
            let e = by_rater.get(&(r.0.clone(), earlier)).cloned().flatten();
            let entry = self_counts.entry(r.0.clone()).or_default();
            entry.1 += 1;
            entry.0 += usize::from(e == chosen);
        } else {
            first.entry((r.0.clone(), pair)).or_insert(chosen);
        }
    }
    let mut self_agreement: Vec<Agreement> = self_counts
        .into_iter()
        .map(|(who, (agree, of))| Agreement { who, agree, of })
        .collect();
    self_agreement.sort_by(|a, b| a.who.cmp(&b.who));
    // Between raters: every two raters' first judgments of the same pair.
    let mut by_pair: HashMap<Pair, Vec<Option<Fn>>> = HashMap::new();
    for ((_, pair), chosen) in first {
        by_pair.entry(pair).or_default().push(chosen);
    }
    let (mut agree, mut of) = (0, 0);
    // Kappa over categories: first function of the pair, second, same.
    let mut table = [[0f64; 3]; 3];
    for (pair, choices) in &by_pair {
        let cat = |c: &Option<Fn>| match c {
            Some(f) if *f == pair.0 => 0,
            Some(_) => 1,
            None => 2,
        };
        for i in 0..choices.len() {
            for j in i + 1..choices.len() {
                of += 1;
                agree += usize::from(choices[i] == choices[j]);
                table[cat(&choices[i])][cat(&choices[j])] += 0.5;
                table[cat(&choices[j])][cat(&choices[i])] += 0.5;
            }
        }
    }
    let between = (of > 0).then(|| Agreement {
        who: "between raters".into(),
        agree,
        of,
    });
    let kappa = (of > 0).then(|| {
        let n = of as f64;
        let po = (0..3).map(|k| table[k][k]).sum::<f64>() / n;
        let pe = (0..3)
            .map(|k| {
                let row: f64 = table[k].iter().sum();
                (row / n) * (row / n)
            })
            .sum::<f64>();
        if pe < 1.0 {
            (po - pe) / (1.0 - pe)
        } else {
            f64::NAN
        }
    });
    Ok((self_agreement, between, kappa))
}

impl Report {
    pub fn json(&self) -> String {
        let d = &self.defects;
        let found: Vec<String> = d
            .found
            .iter()
            .map(|(r, pooled, mean)| {
                format!(
                    "{{\"ranking\":\"{r}\",\"baseline\":{},\"found\":{},\"mean\":{}}}",
                    BASELINES.contains(&r.as_str()),
                    json_number(Some(*pooled)),
                    json_number(Some(*mean))
                )
            })
            .collect();
        let defects = format!(
            "{{\"cuts\":{},\"window_days\":{},\"effort\":{EFFORT},\"fixes\":{},\"rankings\":[{}],\"caveat\":\"{FIX_RECALL}\"}}",
            d.cuts,
            d.window_days,
            d.fixes,
            found.join(",")
        );
        format!(
            "{{\"defects\":{defects},\"labels\":{}}}",
            self.labels_json()
        )
    }

    fn labels_json(&self) -> String {
        let metrics: Vec<String> = self
            .metrics
            .iter()
            .map(|m| {
                let vs = m.vs_tokens.map_or("null".to_owned(), |(d, lo, hi)| {
                    format!("{{\"difference\":{},\"low\":{},\"high\":{}}}", json_number(Some(d)), json_number(Some(lo)), json_number(Some(hi)))
                });
                format!(
                    "{{\"metric\":\"{}\",\"baseline\":{},\"pairs\":{},\"share\":{},\"low\":{},\"high\":{},\"vs_tokens\":{vs},\"supported\":{}}}",
                    m.metric,
                    m.metric == METRICS[0],
                    m.pairs,
                    json_number(Some(m.share)),
                    json_number(Some(m.low)),
                    json_number(Some(m.high)),
                    m.supported
                )
            })
            .collect();
        let agreement = |a: &Agreement| {
            format!(
                "{{\"rater\":{},\"agree\":{},\"of\":{}}}",
                json_text(&a.who),
                a.agree,
                a.of
            )
        };
        let selfs: Vec<String> = self.self_agreement.iter().map(agreement).collect();
        format!(
            "{{\"count\":{},\"decided\":{},\"unknown_functions\":{},\"metrics\":[{}],\"self_agreement\":[{}],\"between_raters\":{},\"kappa\":{}}}",
            self.labels,
            self.decided,
            self.unknown,
            metrics.join(","),
            selfs.join(","),
            self.between.as_ref().map_or("null".into(), |a| format!(
                "{{\"agree\":{},\"of\":{}}}",
                a.agree, a.of
            )),
            json_number(self.kappa)
        )
    }

    pub fn text(&self) -> String {
        let d = &self.defects;
        if d.fixes == 0 {
            let out = format!(
                "defects: {} cuts of {} days, no fix touches after them: history too short or without fixes\n\n",
                d.cuts, d.window_days
            );
            return out + &self.labels_text();
        }
        let mut out = format!(
            "defects: {} cuts of {} days, {} fix touches in the window after a cut\n\
             share of them in the files ranked first that hold {:.0}% of the tokens (pooled, mean over cuts)\n",
            d.cuts,
            d.window_days,
            d.fixes,
            100.0 * EFFORT
        );
        for (r, pooled, mean) in &d.found {
            let base = if BASELINES.contains(&r.as_str()) {
                "  baseline"
            } else {
                ""
            };
            writeln!(out, "  {r:<14} {pooled:>5.3}  {mean:>5.3}{base}").expect("write to String");
        }
        writeln!(out, "  {FIX_RECALL}\n").expect("write to String");
        out + &self.labels_text()
    }

    fn labels_text(&self) -> String {
        if self.labels == 0 {
            return "labels: none yet; judge pairs with `cv label`\n".into();
        }
        let mut out = format!(
            "{} labels, {} decided (left or right){}\n\n",
            self.labels,
            self.decided,
            match self.unknown {
                0 => String::new(),
                n => format!(", {n} with functions this database lacks"),
            }
        );
        out.push_str("share of decided pairs where the higher value is the function the rater would rather not change\n");
        out.push_str(
            "metric       pairs  share  95% interval    vs tokens (95%)            supported\n",
        );
        for m in &self.metrics {
            let vs = match (m.metric == METRICS[0], m.vs_tokens) {
                (true, _) => "baseline".to_owned(),
                (false, Some((d, lo, hi))) => format!("{d:+.3} ({lo:+.3}, {hi:+.3})"),
                (false, None) => "-".to_owned(),
            };
            let (share, interval) = match m.pairs {
                0 => ("-".to_owned(), "-".to_owned()),
                _ => (
                    format!("{:.3}", m.share),
                    format!("({:.3}, {:.3})", m.low, m.high),
                ),
            };
            let verdict = match (m.metric == METRICS[0], m.pairs, m.supported) {
                (true, _, _) => "",
                (false, 0, _) => "no pairs differ",
                (false, _, true) => "yes",
                (false, _, false) => "no",
            };
            writeln!(
                out,
                "{:<12} {:>5}  {:>5}  {:<14}  {:<26} {verdict}",
                m.metric, m.pairs, share, interval, vs
            )
            .expect("write to String");
        }
        out.push_str("\nself-agreement on repeated pairs (sides swapped)\n");
        if self.self_agreement.is_empty() {
            out.push_str("  no repeats yet\n");
        }
        for a in &self.self_agreement {
            writeln!(
                out,
                "  {}: {}/{} ({:.2})",
                a.who,
                a.agree,
                a.of,
                a.agree as f64 / a.of as f64
            )
            .expect("write to String");
        }
        match &self.between {
            Some(a) => writeln!(
                out,
                "agreement between raters on shared pairs: {}/{} ({:.2}), Cohen's kappa {:.2}",
                a.agree,
                a.of,
                a.agree as f64 / a.of as f64,
                self.kappa.unwrap_or(f64::NAN)
            )
            .expect("write to String"),
            None => out.push_str("agreement between raters: no pair judged by two raters yet\n"),
        }
        out
    }
}
