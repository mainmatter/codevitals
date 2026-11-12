//! Checks that incremental extraction equals `extract`, over a scanned repository's
//! whole history: every lineage's parsed blobs in order, each extracted both from
//! scratch and incrementally from the lineage's previous version, compared span by
//! span. Prints the time each took (CPU seconds, summed over threads).
//!
//! Usage: cargo run --release --example incremental -- DB [REPO]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use codevitals::{Incremental, LANGS, extract};
use rayon::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = duckdb::Connection::open_with_flags(
        &args[1],
        duckdb::Config::default()
            .access_mode(duckdb::AccessMode::ReadOnly)
            .unwrap(),
    )
    .unwrap();
    db.execute_batch(codevitals::SQL).ok();
    let repo_path: String = match args.get(2) {
        Some(r) => r.clone(),
        None => db
            .query_row("select repo from codevitals", [], |r| r.get(0))
            .unwrap(),
    };
    let repo = gix::open(&repo_path).unwrap().into_sync();
    // Every parsed blob (a `file` span with counts) in each of its lineages, in order.
    let rows: Vec<(String, String, String)> = db
        .prepare(
            "select l.lineage, l.blob, s.lang
             from lineages l join spans s on s.blob = l.blob and s.kind = 'file' and s.tokens is not null
             order by l.lineage, l.seq",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let lineages: Vec<&[(String, String, String)]> = rows.chunk_by(|a, b| a.0 == b.0).collect();
    let langs: HashMap<&str, _> = LANGS.iter().map(|l| (l.name, *l)).collect();
    let (fresh_ns, incremental_ns) = (AtomicU64::new(0), AtomicU64::new(0));
    let (checked, mismatched) = (AtomicUsize::new(0), AtomicUsize::new(0));
    lineages.par_iter().for_each_init(
        || repo.to_thread_local(),
        |repo, versions| {
            let mut incremental = Incremental::new();
            for (lineage, blob, lang) in versions.iter() {
                let lang = langs[lang.as_str()];
                let src = repo
                    .find_blob(gix::ObjectId::from_hex(blob.as_bytes()).unwrap())
                    .unwrap()
                    .take_data();
                let t = Instant::now();
                let fresh = extract(lang, &src);
                fresh_ns.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                let t = Instant::now();
                let inc = incremental.extract(lang, &src);
                incremental_ns.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                checked.fetch_add(1, Relaxed);
                if fresh != inc && mismatched.fetch_add(1, Relaxed) < 5 {
                    let first = fresh.iter().zip(&inc).position(|(a, b)| a != b);
                    eprintln!(
                        "MISMATCH {lineage} {blob} ({}): {} vs {} spans, first difference at {first:?}: {:?} vs {:?}",
                        lang.name,
                        fresh.len(),
                        inc.len(),
                        first.map(|i| &fresh[i]),
                        first.map(|i| &inc[i]),
                    );
                }
            }
        },
    );
    let s = |a: &AtomicU64| a.load(Relaxed) as f64 / 1e9;
    println!(
        "lineages {} versions {} mismatched {} fresh {:.2} cpu-s incremental {:.2} cpu-s",
        lineages.len(),
        checked.load(Relaxed),
        mismatched.load(Relaxed),
        s(&fresh_ns),
        s(&incremental_ns),
    );
    if mismatched.load(Relaxed) > 0 {
        std::process::exit(1);
    }
}
