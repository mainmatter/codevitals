//! The `tally` command line: `scan` records a repository's history and parses its
//! blobs; `metrics` exports the metric views as JSON.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::{CommandFactory, Parser};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::db::{Access, open};

mod attributes;
mod db;
mod graph;
mod metrics;
mod parse;
mod walk;

/// Record facts about a git repository's history into a DuckDB file.
///
/// Logs go to stderr; set `RUST_LOG` to `debug` for one line per commit and blob,
/// or to `off` for silence. The default is `info`: phases and progress.
#[derive(Parser)]
enum Cli {
    /// Record REV's first-parent history, then parse every new blob.
    Scan {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = "tally.duckdb")]
        db: PathBuf,
        #[arg(default_value = "HEAD")]
        rev: String,
    },
    /// Compute metrics into one JSON object: each metric's name maps to an array
    /// of row objects.
    Metrics {
        #[arg(long, default_value = "tally.duckdb")]
        db: PathBuf,
        /// File to write, replaced atomically; `-` writes to stdout.
        output: PathBuf,
        /// Metrics to compute; all of them when omitted.
        #[arg(value_name = "METRIC")]
        names: Vec<String>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .init();
    // Listing the metrics means binding tally's SQL, so the command that lists them
    // (in `--help` and as the valid values of METRIC) is built only to report help
    // or an error.
    let cli = Cli::try_parse().unwrap_or_else(|_| usage());
    match cli {
        Cli::Scan {
            repo,
            db: path,
            rev,
        } => {
            let repo = gix::discover(&repo)
                .with_context(|| format!("open the git repository containing {repo:?}"))?;
            let mut db = open(&path, Access::ReadWrite)?;
            let (commits, changes) = walk::walk(&mut db, &path, &repo, &rev)?;
            let blobs = parse::parse(&mut db, &repo)?;
            graph::derive(&mut db, graph::Cones::Stored)?;
            eprintln!("scanned {commits} commits, {changes} changes; parsed {blobs} blobs");
        }
        Cli::Metrics {
            db: path,
            output,
            names,
        } => {
            ensure!(
                path.exists(),
                "no database at {path:?}; create it with `tally scan --db {}`",
                path.display()
            );
            let mut db = open(&path, Access::ReadOnly)?;
            let commits: i64 = db.query_row("select count(*) from commits", [], |r| r.get(0))?;
            ensure!(
                commits > 0,
                "{path:?} has no commits yet; run `tally scan --db {}` first",
                path.display()
            );
            ensure!(
                !metrics::exists(&db, &output.to_string_lossy())?,
                "{output:?} is a metric; give the output file first, e.g. `tally metrics - {}`",
                output.display()
            );
            for name in &names {
                if !metrics::exists(&db, name)? {
                    usage();
                }
            }
            let mut seen = std::collections::HashSet::new();
            let names: Vec<String> = names
                .into_iter()
                .filter(|n| seen.insert(n.clone()))
                .collect();
            let json = metrics::export(&mut db, &names)?;
            if output.as_os_str() == "-" {
                println!("{json}");
            } else {
                // Write beside the target, then rename: readers never see a partial file.
                let partial = output.with_extension("partial");
                std::fs::write(&partial, json + "\n")
                    .with_context(|| format!("write {partial:?}"))?;
                std::fs::rename(&partial, &output)
                    .with_context(|| format!("replace {output:?}"))?;
                info!(?output, "metrics written");
            }
        }
    }
    Ok(())
}

/// Parses the arguments again with the full command, which lists every metric, to
/// print help or a usage error, and exits.
fn usage() -> ! {
    Cli::command()
        .mut_subcommand("metrics", |c| {
            c.mut_arg("names", |a| a.value_parser(metrics::names()))
        })
        .get_matches();
    unreachable!("the arguments fail to parse with the full command too")
}
