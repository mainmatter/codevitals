//! Helpers shared by the integration tests: running codevitals, querying its database,
//! building git repositories in process, and writing facts directly.
#![allow(dead_code)] // each test file uses a different subset

use std::path::PathBuf;
use std::process::{Command, Output};

use gix::ObjectId;
use gix::objs::tree::EntryKind;
use tempfile::TempDir;

/// Runs codevitals against the database in `dir`, and for `scan` the repository in `dir/repo`.
pub fn cv(dir: &TempDir, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cv"));
    cmd.env("RUST_LOG", "off"); // stderr carries only the summary line
    cmd.args(args).arg("--db").arg(db(dir));
    if args[0] == "scan" {
        cmd.arg(dir.path().join("repo"));
    }
    cmd.output().unwrap()
}

/// Runs codevitals, which must succeed, and returns its stdout.
pub fn succeeds(dir: &TempDir, args: &[&str]) -> String {
    let out = cv(dir, args);
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{err}");
    String::from_utf8(out.stdout).unwrap()
}

/// Runs codevitals, which must fail, and returns its stderr.
pub fn fails(dir: &TempDir, args: &[&str]) -> String {
    let out = cv(dir, args);
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "{err}");
    err
}

/// Scans `rev`, which must succeed, and returns the summary line.
pub fn scan(dir: &TempDir, rev: &str) -> String {
    let out = cv(dir, &["scan", "--rev", rev]);
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{err}");
    err
}

pub fn db(dir: &TempDir) -> PathBuf {
    dir.path().join("t.duckdb")
}

/// Rows of `query` rendered as `(a,b,c)` joined by spaces, NULL as `NULL`.
pub fn rows(dir: &TempDir, query: &str) -> String {
    let conn = duckdb::Connection::open(db(dir)).unwrap();
    let mut stmt = conn
        .prepare(&format!("select columns(*)::varchar from ({query})"))
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| {
            let cols = (0..r.as_ref().column_count())
                .map(|i| {
                    r.get::<_, Option<String>>(i)
                        .map(|v| v.unwrap_or("NULL".into()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("({})", cols.join(",")))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.join(" ")
}

/// Asserts that `query` gives `expected`, rendered as `rows` renders it.
#[track_caller]
pub fn assert_rows(dir: &TempDir, query: &str, expected: impl AsRef<str>) {
    assert_eq!(rows(dir, query), expected.as_ref(), "{query}");
}

pub fn execute(dir: &TempDir, sql: &str) {
    duckdb::Connection::open(db(dir))
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

/// The id `spec` resolves to in `dir/repo`, e.g. `main~2` or `main:a.rs`.
pub fn rev(dir: &TempDir, spec: &str) -> String {
    let repo = gix::open(dir.path().join("repo")).unwrap();
    repo.rev_parse_single(spec).unwrap().to_string()
}

/// A git repository in `dir/repo`, written in process. Files accumulate in a working
/// tree that each commit snapshots; author and committer are `t <t@t>`.
pub struct Repo {
    pub dir: TempDir,
    repo: gix::Repository,
    tree: ObjectId,
}

impl Repo {
    pub fn new() -> Repo {
        let dir = TempDir::new().unwrap();
        let repo = gix::init(dir.path().join("repo")).unwrap();
        let tree = repo.empty_tree().id;
        Repo { dir, repo, tree }
    }

    /// Writes `content` at `path`, which may be any bytes (git paths need not be UTF-8).
    pub fn write(&mut self, path: impl AsRef<[u8]>, content: impl AsRef<[u8]>) -> &mut Repo {
        let blob = self.repo.write_blob(content).unwrap().detach();
        let mut editor = self.repo.edit_tree(self.tree).unwrap();
        let path: &gix::bstr::BStr = path.as_ref().into();
        editor.upsert(path, EntryKind::Blob, blob).unwrap();
        self.tree = editor.write().unwrap().detach();
        self
    }

    pub fn remove(&mut self, path: &str) -> &mut Repo {
        let mut editor = self.repo.edit_tree(self.tree).unwrap();
        editor.remove(path).unwrap();
        self.tree = editor.write().unwrap().detach();
        self
    }

    /// Commits the working tree to `branch`, `day` days after 2026-01-01 (UTC).
    pub fn commit(
        &mut self,
        branch: &str,
        message: &str,
        day: i64,
        parents: &[ObjectId],
    ) -> ObjectId {
        let time = format!("{} +0000", 1_767_225_600 + day * 86_400);
        let who = gix::actor::SignatureRef {
            name: "t".into(),
            email: "t@t".into(),
            time: &time,
        };
        let reference = format!("refs/heads/{branch}");
        let id = self.repo.commit_as(
            who,
            who,
            reference.as_str(),
            message,
            self.tree,
            parents.iter().copied(),
        );
        id.unwrap().detach()
    }

    /// Makes the repository a shallow clone cut at `boundary`: the commit stays, its
    /// parent's object is deleted and `.git/shallow` names the boundary, as `git clone
    /// --depth` leaves it.
    pub fn make_shallow(&mut self, boundary: ObjectId) {
        let parent = self
            .repo
            .find_commit(boundary)
            .unwrap()
            .parent_ids()
            .next()
            .unwrap()
            .detach();
        let hex = parent.to_string();
        let git = self.dir.path().join("repo/.git");
        std::fs::remove_file(git.join("objects").join(&hex[..2]).join(&hex[2..])).unwrap();
        std::fs::write(git.join("shallow"), format!("{boundary}\n")).unwrap();
    }

    /// Commits the working tree on top of `main`.
    pub fn extend(&mut self, message: &str, day: i64) -> ObjectId {
        let tip = self
            .repo
            .find_reference("refs/heads/main")
            .unwrap()
            .id()
            .detach();
        self.commit("main", message, day, &[tip])
    }
}

/// The fixture repository: four commits on `main` (an edit, a rename, a merge of a
/// side branch), with its three functions named `add`, `plus` and `d`.
pub fn fixture() -> Repo {
    fixture_named("add", "plus", "d")
}

pub fn fixture_named(add: &str, plus: &str, d: &str) -> Repo {
    let plus_rs = format!("fn {plus}(x: i32, y: i32) -> i32 {{\n    x + y\n}}\n");
    let mut r = Repo::new();
    r.write("README.md", "# readme\n");
    r.write(
        "a.rs",
        format!("fn {add}(a: i32, b: i32) -> i32 {{\n    a + b\n}}\n"),
    );
    let c0 = r.commit("main", "c0\n", 0, &[]);
    r.write(
        "a.rs",
        format!("fn {add}(a: i32, b: i32) -> i32 {{\n    // sum\n    a + b\n}}\n"),
    );
    r.write("src/b.rs", &plus_rs);
    let c1 = r.commit("main", "c1\n", 1, &[c0]);
    r.remove("src/b.rs")
        .write("src/c.rs", &plus_rs)
        .remove("README.md");
    let c2 = r.commit("main", "c2\n", 2, &[c1]);
    r.write("d.rs", format!("fn {d}() {{}}\n"));
    let s1 = r.commit("side", "s1\n", 3, &[c2]);
    r.write("d.rs", format!("fn {d}() -> i32 {{ 1 }}\n"));
    let s2 = r.commit("side", "s2\n", 3, &[s1]);
    r.commit("main", "merge\n", 4, &[c2, s2]);
    r
}

/// A database with the schema and metrics applied and no repository, for metric
/// tests that write facts directly with `history`.
pub fn facts(history: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    execute(&dir, codevitals::SQL);
    execute(&dir, history);
    dir
}

/// SQL for commits `first..first + commits`, one per day from 2026-01-01, each changing
/// `per_commit` of `files` Rust files spread over `modules` directories. Files are
/// picked by MD5 rank (DuckDB's `hash` mixes small integers too poorly for this), so co-change is independent of modules by construction; every
/// change writes a new blob that parses as production code.
pub fn random_history(
    first: u32,
    commits: u32,
    modules: u32,
    files: u32,
    per_commit: u32,
) -> String {
    let last = first + commits;
    format!(
        "insert into commits select seq, 'c' || seq, timestamp '2026-01-01' + to_days(seq::int), 't <t@t>', 'edit'
           from range({first}, {last}) r(seq);
         insert into changes (seq, path, blob) select seq, 'm' || (f % {modules}) || '/f' || f || '.rs', 'b' || seq || '-' || f
           from range({first}, {last}) c(seq), range({files}) g(f)
           qualify row_number() over (partition by seq order by md5(seq || ':' || f)) <= {per_commit};
         insert into spans (blob, v, lang, kind, line, tokens)
           select blob, 1, 'rust', 'file', 1, 1 from changes where seq >= {first};"
    )
}
