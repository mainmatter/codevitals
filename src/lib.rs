//! codevitals records facts about a git repository's history into DuckDB and computes
//! metrics over them in SQL. This library is the part that needs neither a
//! repository nor a database: the languages codevitals parses, `extract` (a pure function
//! from a blob's bytes to its facts), and the SQL that turns facts into metrics. The
//! `codevitals` binary walks history, parses blobs and exports the metrics.

mod extract;
mod languages;

pub use extract::{Incremental, Span, edits, extract};
pub use languages::{KINDS, LANGS, Lang};

/// The SQL files a scan reads: the tables, helper views and the import graph.
macro_rules! scan_sql {
    () => {
        concat!(
            include_str!("../sql/schema.sql"),
            "\n",
            include_str!("../sql/helpers.sql"),
            "\n",
            include_str!("../sql/imports.sql"),
            "\n",
        )
    };
}

/// The rest: the per-unit and metric views, which only read a scan's results.
macro_rules! views_sql {
    () => {
        concat!(
            include_str!("../sql/units.sql"),
            "\n",
            include_str!("../sql/metrics.sql"),
            "\n",
            include_str!("../sql/experimental.sql"),
        )
    };
}

/// The SQL every codevitals database is opened with, in order: the tables, helper views,
/// the import graph, the per-unit views and the metric views (`sql/`).
pub const SQL: &str = concat!(scan_sql!(), views_sql!());

/// The part of `SQL` a scan reads.
pub const SCAN_SQL: &str = scan_sql!();

/// The rest of `SQL`, after `SCAN_SQL`.
pub const VIEWS_SQL: &str = views_sql!();

/// The hash of `SQL` a database records (`codevitals_sql()`) once its views and
/// macros are those of this codevitals.
pub fn sql_hash() -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(SQL.as_bytes()))
}

/// The statement that marks a database's views and macros as this codevitals' (`db.rs`
/// rebuilds them when the mark differs).
pub fn sql_mark() -> String {
    format!("create macro codevitals_sql() as '{}'", sql_hash())
}
