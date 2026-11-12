//! codevitals records facts about a git repository's history into DuckDB and computes
//! metrics over them in SQL. This library is the part that needs neither a
//! repository nor a database: the languages codevitals parses, `extract` (a pure function
//! from a blob's bytes to its facts), and the SQL that turns facts into metrics. The
//! `codevitals` binary walks history, parses blobs and exports the metrics.

mod extract;
mod languages;

pub use extract::{Span, extract};
pub use languages::{KINDS, LANGS, Lang};

/// The SQL every codevitals database is opened with, in order: the tables, helper views,
/// the import graph, the metric views and the per-unit views (`sql/`).
pub const SQL: &str = concat!(
    include_str!("../sql/schema.sql"),
    "\n",
    include_str!("../sql/helpers.sql"),
    "\n",
    include_str!("../sql/imports.sql"),
    "\n",
    include_str!("../sql/metrics.sql"),
    "\n",
    include_str!("../sql/units.sql"),
);
