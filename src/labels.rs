//! Labels: human judgments of pairs of functions. The labels file (JSON Lines, one
//! judgment per line, appended to) is their record: it outlives the database, which
//! a new codevitals version may need rebuilt, and raters can merge theirs. `labels`
//! in the database is loaded from it.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use duckdb::Connection;

use crate::util::json_text;

/// One judgment: which of two functions the rater would rather not have to change.
#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub rater: String,
    /// The rater's judgments before this one.
    pub n: i64,
    /// Seconds since the Unix epoch.
    pub time: i64,
    pub a: Function,
    pub b: Function,
    /// `a`, `b` or `same` (can't tell).
    pub choice: &'static str,
    pub ms: i64,
    pub a_commits: i64,
    pub b_commits: i64,
    /// `n` of the rater's earlier judgment of the same pair.
    pub repeat_of: Option<i64>,
}

/// A function as stored: its blob and byte range, and the path it was shown at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Function {
    pub blob: String,
    pub start: i64,
    pub end: i64,
    pub path: String,
}

/// The columns of a labels file, in the order of the `labels` table.
const COLUMNS: &str = "{'rater': 'varchar', 'n': 'integer', 'time': 'timestamp',
  'a_blob': 'varchar', 'a_start': 'integer', 'a_end': 'integer', 'a_path': 'varchar',
  'b_blob': 'varchar', 'b_start': 'integer', 'b_end': 'integer', 'b_path': 'varchar',
  'choice': 'varchar', 'ms': 'integer', 'a_commits': 'integer', 'b_commits': 'integer',
  'repeat_of': 'integer'}";

/// Loads the labels file at `file` (none when it does not exist) into a temp table
/// `labels`, which shadows the stored one for this connection, so a read-only
/// database can be labelled. Returns the number loaded.
pub fn load(db: &Connection, file: &Path) -> Result<usize> {
    let table = "temp.labels";
    db.execute_batch("create or replace temp table labels as select * from main.labels limit 0")?;
    if !file.exists() || std::fs::metadata(file)?.len() == 0 {
        return Ok(0);
    }
    let quoted = file.to_string_lossy().replace('\'', "''");
    let loaded = db
        .execute(
            &format!(
                "insert into {table} select * from read_json('{quoted}', format = 'newline_delimited',
                   columns = {COLUMNS}, timestampformat = '%Y-%m-%dT%H:%M:%SZ')"
            ),
            [],
        )
        .with_context(|| format!("read labels from {file:?}"))?;
    let bad: i64 = db.query_row(
        &format!(
            "select count(*) from {table} where choice not in ('a', 'b', 'same') or choice is null"
        ),
        [],
        |r| r.get(0),
    )?;
    ensure!(
        bad == 0,
        "{file:?}: {bad} labels have a choice other than a, b or same"
    );
    // Every field but repeat_of is required: readers rely on it.
    let missing: i64 = db.query_row(
        &format!(
            "select count(*) from {table}
             where rater is null or n is null or time is null or a_blob is null or a_start is null
                or a_end is null or a_path is null or b_blob is null or b_start is null
                or b_end is null or b_path is null or ms is null or a_commits is null
                or b_commits is null"
        ),
        [],
        |r| r.get(0),
    )?;
    ensure!(
        missing == 0,
        "{file:?}: {missing} labels lack a field (every field but repeat_of is required)"
    );
    Ok(loaded)
}

/// Appends `label` to the labels file at `file`, creating it if needed.
pub fn append(file: &Path, label: &Label) -> Result<()> {
    // In UTC, to the second: `2026-01-01T23:59:59Z`.
    let time = jiff::Timestamp::from_second(label.time)?;
    let repeat = label.repeat_of.map_or("null".to_owned(), |n| n.to_string());
    let line = format!(
        "{{\"rater\":{},\"n\":{},\"time\":\"{time}\",\"a_blob\":\"{}\",\"a_start\":{},\"a_end\":{},\"a_path\":{},\
         \"b_blob\":\"{}\",\"b_start\":{},\"b_end\":{},\"b_path\":{},\"choice\":\"{}\",\"ms\":{},\
         \"a_commits\":{},\"b_commits\":{},\"repeat_of\":{repeat}}}\n",
        json_text(&label.rater),
        label.n,
        label.a.blob,
        label.a.start,
        label.a.end,
        json_text(&label.a.path),
        label.b.blob,
        label.b.start,
        label.b.end,
        json_text(&label.b.path),
        label.choice,
        label.ms,
        label.a_commits,
        label.b_commits,
    );
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .with_context(|| format!("open {file:?}"))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("append to {file:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Label times are written as before: UTC, to the second, `Z`.
    #[test]
    fn utc() {
        let utc = |s| jiff::Timestamp::from_second(s).unwrap().to_string();
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(1_767_225_600 + 86_399), "2026-01-01T23:59:59Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn append_then_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("labels.jsonl");
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(codevitals::SQL).unwrap();
        assert_eq!(load(&db, &file).unwrap(), 0);
        let f = |blob: &str, path: &str| Function {
            blob: blob.into(),
            start: 3,
            end: 40,
            path: path.into(),
        };
        let first = Label {
            rater: "Ann \"A\" <ann@x>".into(),
            n: 0,
            time: 1_767_225_600,
            a: f("aa", "src/a.rs"),
            b: f("bb", "src/b.rs"),
            choice: "a",
            ms: 4200,
            a_commits: 3,
            b_commits: 0,
            repeat_of: None,
        };
        append(&file, &first).unwrap();
        let repeat = Label {
            n: 1,
            a: first.b.clone(),
            b: first.a.clone(),
            choice: "b",
            repeat_of: Some(0),
            ..first.clone()
        };
        append(&file, &repeat).unwrap();
        assert_eq!(load(&db, &file).unwrap(), 2);
        let rows: Vec<(String, i64, String, String, String, Option<i64>)> = db
            .prepare(
                "select rater, n, time::varchar, a_path, choice, repeat_of from labels order by n",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                (
                    "Ann \"A\" <ann@x>".into(),
                    0,
                    "2026-01-01 00:00:00".into(),
                    "src/a.rs".into(),
                    "a".into(),
                    None
                ),
                (
                    "Ann \"A\" <ann@x>".into(),
                    1,
                    "2026-01-01 00:00:00".into(),
                    "src/b.rs".into(),
                    "b".into(),
                    Some(0)
                ),
            ]
        );
        // Loading again replaces, not duplicates.
        assert_eq!(load(&db, &file).unwrap(), 2);
        let count: i64 = db
            .query_row("select count(*) from labels", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        // A bad choice is refused.
        std::fs::write(
            &file,
            std::fs::read_to_string(&file)
                .unwrap()
                .replace("\"choice\":\"a\"", "\"choice\":\"x\""),
        )
        .unwrap();
        assert!(load(&db, &file).is_err());
    }
}
