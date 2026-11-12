//! The `codevitals` command line: `scan` records a repository's history and parses its
//! blobs; `metrics` exports the metric views as JSON.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::{CommandFactory, Parser};
use tracing::info;
use tracing_subscriber::EnvFilter;

use duckdb::AccessMode;

use crate::db::open;

mod attributes;
mod db;
mod diff;
mod graph;
mod metrics;
mod parse;
mod units;
mod walk;

/// Record facts about a git repository's history into a DuckDB file.
///
/// Logs go to stderr; set `RUST_LOG` to `debug` for one line per commit and blob,
/// or to `off` for silence. The default is `info`: phases and progress.
#[derive(Parser)]
#[command(name = "cv", version)]
enum Cli {
    /// Record a repository's first-parent history, then parse every new blob.
    Scan {
        #[arg(default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = "codevitals.duckdb")]
        db: PathBuf,
        /// Commit whose history to record.
        #[arg(long, default_value = "HEAD")]
        rev: String,
    },
    /// Compute metrics into one JSON object: each metric's name maps to an array
    /// of row objects.
    Metrics {
        #[arg(long, default_value = "codevitals.duckdb")]
        db: PathBuf,
        /// File to write, replaced atomically; `-` writes to stdout.
        output: PathBuf,
        /// Metrics to compute; all of them when omitted.
        #[arg(value_name = "METRIC")]
        names: Vec<String>,
    },
    /// Report how each metric changes from BASE to HEAD, and the functions and files
    /// that account for the change, largest contribution first. Needs no scan: blobs
    /// the database lacks are parsed (with a scan that reached BASE, only those that
    /// differ). Windowed metrics (changes, entropy, ownership) and dependency metrics
    /// need history and are not reported.
    Diff {
        base: String,
        head: String,
        /// Repository to read the commits from.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Scanned database to take parsed blobs from, if it exists.
        #[arg(long, default_value = "codevitals.duckdb")]
        db: PathBuf,
        /// Contributions listed per metric, for functions and for files.
        #[arg(long, default_value_t = 10)]
        top: usize,
        #[arg(long, value_enum, default_value_t = diff::Format::Json)]
        format: diff::Format,
    },
    /// Rank the production files of the newest scanned commit by how often they
    /// changed recently times their decision points: where change meets complexity.
    Hotspots {
        #[arg(long, default_value = "codevitals.duckdb")]
        db: PathBuf,
        /// Files to list.
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[arg(long, value_enum, default_value_t = diff::Format::Text)]
        format: diff::Format,
    },
    /// Export the production files and functions at one scanned commit, with the
    /// values the metrics add up (tokens, decisions, nesting, ...), as one JSON object.
    /// A function keeps its `function` id across versions while its file and name do.
    Units {
        #[arg(long, default_value = "codevitals.duckdb")]
        db: PathBuf,
        /// File to write, replaced atomically; `-` writes to stdout.
        output: PathBuf,
        /// Scanned commit (id or prefix); the newest when omitted.
        #[arg(long)]
        rev: Option<String>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .init();
    // Listing the metrics means binding codevitals' SQL, so the command that lists them
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
            let mut db = open(&path, AccessMode::ReadWrite)?;
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
            let mut db = open_scanned(&path)?;
            let metrics = metrics::list(&db)?;
            ensure!(
                !metrics.iter().any(|m| output.as_os_str() == m.as_str()),
                "{output:?} is a metric; give the output file first, e.g. `cv metrics - {}`",
                output.display()
            );
            if names.iter().any(|n| !metrics.contains(n)) {
                usage();
            }
            let names = match names.is_empty() {
                true => metrics,
                false => {
                    let mut seen = HashSet::new();
                    names
                        .into_iter()
                        .filter(|n| seen.insert(n.clone()))
                        .collect()
                }
            };
            let json = metrics::export(&mut db, &names)?;
            write(&output, json)?;
            info!(?output, "metrics written");
        }
        Cli::Diff {
            base,
            head,
            repo,
            db,
            top,
            format,
        } => {
            let repo = gix::discover(&repo)
                .with_context(|| format!("open the git repository containing {repo:?}"))?;
            print!("{}", diff::diff(&repo, &db, &base, &head, top, format)?);
            if matches!(format, diff::Format::Json) {
                println!();
            }
        }
        Cli::Hotspots {
            db: path,
            top,
            format,
        } => {
            let mut db = open_scanned(&path)?;
            let json = matches!(format, diff::Format::Json);
            let out = units::hotspots(&mut db, top, json)?;
            match json {
                true => println!("{out}"),
                false => print!("{out}"),
            }
        }
        Cli::Units {
            db: path,
            output,
            rev,
        } => {
            let mut db = open_scanned(&path)?;
            let json = units::export(&mut db, rev.as_deref())?;
            write(&output, json)?;
        }
    }
    Ok(())
}

/// Opens an existing database with at least one scanned commit, read-only.
fn open_scanned(path: &std::path::Path) -> Result<duckdb::Connection> {
    ensure!(
        path.exists(),
        "no database at {path:?}; create it with `cv scan --db {}`",
        path.display()
    );
    let db = open(path, AccessMode::ReadOnly)?;
    let commits: i64 = db.query_row("select count(*) from commits", [], |r| r.get(0))?;
    ensure!(
        commits > 0,
        "{path:?} has no commits yet; run `cv scan --db {}` first",
        path.display()
    );
    Ok(db)
}

/// Writes `text` and a newline to `output`, atomically, or to stdout for `-`.
fn write(output: &std::path::Path, text: String) -> Result<()> {
    if output.as_os_str() == "-" {
        println!("{text}");
        return Ok(());
    }
    // Write beside the target, then rename: readers never see a partial file.
    let partial = output.with_extension("partial");
    std::fs::write(&partial, text + "\n").with_context(|| format!("write {partial:?}"))?;
    std::fs::rename(&partial, output).with_context(|| format!("replace {output:?}"))?;
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
