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
struct Judgment {
    signs: Vec<i8>,
}

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
    pub labels: usize,
    pub decided: usize,
    pub unknown: usize,
    pub metrics: Vec<Support>,
    pub self_agreement: Vec<Agreement>,
    pub between: Option<Agreement>,
    pub kappa: Option<f64>,
}

/// Loads the labels file into a temp table and computes the report.
pub fn validate(db: &mut Connection, file: &Path) -> Result<Report> {
    crate::metrics::cache_helpers(db)?;
    db.execute_batch("create or replace temp table all_labels as select * from labels limit 0")?;
    let labels = crate::labels::load(db, file, "all_labels")?;
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
         from all_labels l
         left join f a on a.blob = l.a_blob and a.start_byte = l.a_start and a.end_byte = l.a_end
         left join f b on b.blob = l.b_blob and b.start_byte = l.b_start and b.end_byte = l.b_end",
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
                let (a, b): (f64, f64) = (r.get(2 + 2 * i)?, r.get(3 + 2 * i)?);
                // The function the rater would rather not change: the one not chosen.
                let (kept, chosen) = if choice == "a" { (b, a) } else { (a, b) };
                Ok(match kept.partial_cmp(&chosen) {
                    Some(std::cmp::Ordering::Greater) => 1,
                    Some(std::cmp::Ordering::Less) => -1,
                    _ => 0,
                })
            })
            .collect::<duckdb::Result<_>>()?;
        judgments.push(Judgment { signs });
    }
    drop(rows);
    drop(stmt);
    let metrics = (0..METRICS.len()).map(|i| support(&judgments, i)).collect();
    let (self_agreement, between, kappa) = agreement(db)?;
    Ok(Report {
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
        match judgments[i].signs[metric] {
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
    let pairs = all
        .iter()
        .filter(|&&i| judgments[i].signs[metric] != 0)
        .count();
    let point = share(judgments, &all, metric).unwrap_or(f64::NAN);
    // Paired with tokens: judgments both decide.
    let both: Vec<usize> = all
        .iter()
        .copied()
        .filter(|&i| judgments[i].signs[metric] != 0 && judgments[i].signs[0] != 0)
        .collect();
    let mut rng = xxh3_64(METRICS[metric].as_bytes());
    let mut next = |n: usize| {
        rng = rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) % n as u64) as usize
    };
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
             from all_labels order by rater, n",
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

fn num(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "null".into()
    }
}

impl Report {
    pub fn json(&self) -> String {
        let metrics: Vec<String> = self
            .metrics
            .iter()
            .map(|m| {
                let vs = m.vs_tokens.map_or("null".to_owned(), |(d, lo, hi)| {
                    format!("{{\"difference\":{},\"low\":{},\"high\":{}}}", num(d), num(lo), num(hi))
                });
                format!(
                    "{{\"metric\":\"{}\",\"baseline\":{},\"pairs\":{},\"share\":{},\"low\":{},\"high\":{},\"vs_tokens\":{vs},\"supported\":{}}}",
                    m.metric,
                    m.metric == METRICS[0],
                    m.pairs,
                    num(m.share),
                    num(m.low),
                    num(m.high),
                    m.supported
                )
            })
            .collect();
        let agreement = |a: &Agreement| {
            format!(
                "{{\"rater\":\"{}\",\"agree\":{},\"of\":{}}}",
                a.who.replace('\\', "\\\\").replace('"', "\\\""),
                a.agree,
                a.of
            )
        };
        let selfs: Vec<String> = self.self_agreement.iter().map(agreement).collect();
        format!(
            "{{\"labels\":{},\"decided\":{},\"unknown_functions\":{},\"metrics\":[{}],\"self_agreement\":[{}],\"between_raters\":{},\"kappa\":{}}}",
            self.labels,
            self.decided,
            self.unknown,
            metrics.join(","),
            selfs.join(","),
            self.between.as_ref().map_or("null".into(), |a| format!(
                "{{\"agree\":{},\"of\":{}}}",
                a.agree, a.of
            )),
            self.kappa.map_or("null".into(), num)
        )
    }

    pub fn text(&self) -> String {
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
            writeln!(
                out,
                "{:<12} {:>5}  {:>5.3}  ({:.3}, {:.3})  {:<26} {}",
                m.metric,
                m.pairs,
                m.share,
                m.low,
                m.high,
                vs,
                if m.metric == METRICS[0] {
                    ""
                } else if m.supported {
                    "yes"
                } else {
                    "no"
                }
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
