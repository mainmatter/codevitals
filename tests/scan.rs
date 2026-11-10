use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// The fixture repository of the spec, built in `dir/repo`.
fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("repo")).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture.sh");
    sh(&dir.path().join("repo"), &format!("sh {script:?}"));
    dir
}

fn sh(dir: &Path, script: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn tally(dir: &TempDir, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tally"));
    cmd.env("RUST_LOG", "off"); // stderr carries only the summary line
    cmd.args(args).arg("--db").arg(db(dir));
    if args[0] == "scan" {
        cmd.arg("--repo").arg(dir.path().join("repo"));
    }
    cmd.output().unwrap()
}

fn scan(dir: &TempDir, rev: &str) -> String {
    let out = tally(dir, &["scan", rev]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stderr).unwrap()
}

fn db(dir: &TempDir) -> PathBuf {
    dir.path().join("t.duckdb")
}

/// Rows of `query` rendered as `(a,b,c)` joined by spaces, NULL as `NULL`.
fn rows(dir: &TempDir, query: &str) -> String {
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

fn dump(dir: &TempDir) -> String {
    [
        "select * from commits order by seq",
        "select * from changes order by seq, path",
        "select * from spans order by blob, kind, line, hash",
    ]
    .map(|q| rows(dir, q))
    .join("\n")
}

fn fresh_dump() -> String {
    let dir = fixture();
    scan(&dir, "main");
    dump(&dir)
}

#[test]
fn fixture_tables() {
    let dir = fixture();
    assert_eq!(
        scan(&dir, "main"),
        "scanned 4 commits, 8 changes; parsed 4 blobs\n"
    );

    let ids = sh(
        &dir.path().join("repo"),
        "git rev-list --first-parent --reverse main",
    );
    let ids: Vec<&str> = ids.lines().collect();
    let times = ["2026-01-01", "2026-01-02", "2026-01-03", "2026-01-05"];
    let commits: Vec<String> = (0..4)
        .map(|i| {
            let message = ["c0", "c1", "c2", "merge"][i];
            format!("({i},{},{} 00:00:00,t <t@t>,{message}\n)", ids[i], times[i])
        })
        .collect();
    assert_eq!(
        rows(&dir, "select * from commits order by seq"),
        commits.join(" ")
    );

    let blob = |rev: &str| sh(&dir.path().join("repo"), &format!("git rev-parse {rev}"));
    let (readme, a1, a2) = (
        blob("main~3:README.md"),
        blob("main~3:a.rs"),
        blob("main~2:a.rs"),
    );
    let (plus, d) = (blob("main~2:src/b.rs"), blob("main:d.rs"));
    assert_eq!(
        rows(&dir, "select * from changes order by seq, path"),
        format!(
            "(0,README.md,{readme}) (0,a.rs,{a1}) (1,a.rs,{a2}) (1,src/b.rs,{plus}) \
             (2,README.md,NULL) (2,src/b.rs,NULL) (2,src/c.rs,{plus}) (3,d.rs,{d})"
        )
    );
    assert_eq!(
        rows(
            &dir,
            "select path, blob, since, until from files order by path, since"
        ),
        format!(
            "(README.md,{readme},0,2) (a.rs,{a1},0,1) (a.rs,{a2},1,2147483647) \
             (d.rs,{d},3,2147483647) (src/b.rs,{plus},1,2) (src/c.rs,{plus},2,2147483647)"
        )
    );

    assert_eq!(
        rows(
            &dir,
            "select kind, count(*), count(distinct blob) from spans group by kind order by kind"
        ),
        "(file,4,4) (fn,4,4)"
    );
    assert_eq!(
        rows(
            &dir,
            &format!(
                "select blob, name, tokens, comments from spans where kind = 'fn' and blob in ('{a1}', '{a2}') order by comments"
            )
        ),
        format!("({a1},add,18,0) ({a2},add,18,2)")
    );
}

#[test]
fn idempotent() {
    let dir = fixture();
    scan(&dir, "main");
    let before = dump(&dir);
    assert_eq!(
        scan(&dir, "main"),
        "scanned 0 commits, 0 changes; parsed 0 blobs\n"
    );
    assert_eq!(dump(&dir), before);
}

#[test]
fn incremental() {
    let dir = fixture();
    assert_eq!(
        scan(&dir, "main~2"),
        "scanned 2 commits, 4 changes; parsed 3 blobs\n"
    );
    assert_eq!(
        scan(&dir, "main"),
        "scanned 2 commits, 4 changes; parsed 1 blobs\n"
    );
    assert_eq!(dump(&dir), fresh_dump());
}

#[test]
fn resume() {
    // The state an interrupted scan can leave: whole commits past some point and
    // all spans missing. A rerun must complete both loops.
    let dir = fixture();
    scan(&dir, "main");
    let conn = duckdb::Connection::open(db(&dir)).unwrap();
    conn.execute_batch(
        "delete from commits where seq >= 2; delete from changes where seq >= 2; delete from spans",
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        scan(&dir, "main"),
        "scanned 2 commits, 4 changes; parsed 4 blobs\n"
    );
    assert_eq!(dump(&dir), fresh_dump());
}

#[test]
fn version_bump() {
    let dir = fixture();
    scan(&dir, "main");
    let before = dump(&dir);
    duckdb::Connection::open(db(&dir))
        .unwrap()
        .execute("update spans set v = 0", [])
        .unwrap();
    assert_eq!(
        scan(&dir, "main"),
        "scanned 0 commits, 0 changes; parsed 4 blobs\n"
    );
    assert_eq!(dump(&dir), before);
}

#[test]
fn diverged() {
    let dir = fixture();
    scan(&dir, "main");
    let before = dump(&dir);
    let out = tally(&dir, &["scan", "side"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("history diverged from the stored chain; reset the walk with: duckdb <db> \"delete from commits; delete from changes\""),
        "{stderr}"
    );
    assert_eq!(dump(&dir), before);
}

#[test]
fn oversized() {
    let dir = fixture();
    let repo = dir.path().join("repo");
    std::fs::write(repo.join("big.rs"), "// x\n".repeat(300_000)).unwrap();
    sh(&repo, "git add -A && git commit -qm big");
    scan(&dir, "main");
    assert_eq!(
        rows(
            &dir,
            &format!(
                "select * exclude (blob) from spans where blob = '{}'",
                sh(&repo, "git rev-parse main:big.rs")
            )
        ),
        "(2,rust,file,NULL,1,NULL,NULL,NULL,NULL,NULL,NULL)"
    );
}

#[test]
fn metric_views() {
    let dir = fixture();
    scan(&dir, "main");

    // Helper views.
    assert_eq!(
        rows(
            &dir,
            "select seq, subject, is_fix, is_agent from commit_info order by seq"
        ),
        "(0,c0,false,false) (1,c1,false,false) (2,c2,false,false) (3,merge,false,false)"
    );
    assert_eq!(
        rows(&dir, "select name, public from fns order by name, tokens"),
        "(add,false) (add,false) (d,false) (plus,false)"
    );
    assert_eq!(rows(&dir, "select count(*) from call_edges"), "(0)");

    // Values checked by hand against the fixture history.
    let cases = [
        // Commits within one module (depth 2; root files share the module ''), trailing average.
        (
            "select seq, value from m_locality",
            "(0,1.0) (1,0.5) (2,0.3333333333333333) (3,0.5)",
        ),
        // Same-commit pairs: c1 a.rs+src/b.rs crosses; c2 README.md with src/b.rs and src/c.rs crosses twice.
        (
            "select seq, value, pairs from m_hidden_coupling",
            "(0,0.6,5)",
        ),
        // 2 parameters per 18 tokens.
        (
            "select seq, value from m_interface_bloat where seq = 0",
            "(0,11.11111111111111)",
        ),
        // No function is pub, so all function tokens are non-public.
        (
            "select seq, value from m_encapsulation",
            "(0,1.0) (1,1.0) (2,1.0) (3,1.0)",
        ),
        // `add` and `d` have at most three characters; `plus` has four.
        (
            "select seq, value from m_naming",
            "(0,1.0) (1,0.5) (2,0.5) (3,0.6666666666666666)",
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(
            rows(&dir, &format!("{query} order by seq")),
            expected,
            "{query}"
        );
    }

    // Every metric view has the columns the export and the dashboard rely on.
    let metrics = rows(
        &dir,
        "select view_name from duckdb_views() where starts_with(view_name, 'm_')",
    );
    for m in metrics.split(' ').map(|m| m.trim_matches(['(', ')'])) {
        let has = rows(
            &dir,
            &format!("select count(*) from (describe {m}) where column_name in ('seq', 'value')"),
        );
        assert_eq!(has, "(2)", "{m} lacks seq or value");
    }
}

#[test]
fn metrics_json() {
    let dir = fixture();
    scan(&dir, "main");
    let out = tally(&dir, &["metrics", "m_hidden_coupling", "m_locality"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let locality = [("01", "1.0", "2.0"), ("02", "0.5", "2.0"), ("03", "0.3333333333333333", "2.3333333333333335"), ("05", "0.5", "2.0")]
        .iter()
        .enumerate()
        .map(|(i, (day, value, files))| {
            format!(r#"{{"seq":{i},"time":"2026-01-{day}T00:00:00Z","value":{value},"files_per_commit":{files}}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!(
            r#"{{"m_hidden_coupling":[{{"seq":0,"time":"2026-01-01T00:00:00Z","win":0,"value":0.6,"pairs":5}}],"m_locality":[{locality}]}}"#
        ) + "\n"
    );

    // All metrics by default, in definition order; empty ones as [].
    let all = String::from_utf8(tally(&dir, &["metrics"]).stdout).unwrap();
    assert!(
        all.starts_with(r#"{"m_locality":["#) && all.contains(r#""m_call_coupling":[]"#),
        "{all}"
    );

    let out = tally(&dir, &["metrics", "nope"]);
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        !out.status.success() && err.contains("invalid value 'nope'"),
        "{err}"
    );
}

#[test]
fn dashboard_matches_metrics() {
    let dir = fixture();
    scan(&dir, "main");
    let dashboard = Path::new(env!("CARGO_MANIFEST_DIR")).join("grafana/dashboards/tally.json");
    let panels = format!(
        "select unnest(json_extract(json, '$.panels[*]')) as p from read_json_objects('{}')",
        dashboard.display()
    );
    // Each metric view has one single-series panel, described by the view's comment.
    let mismatched = rows(
        &dir,
        &format!(
            "with p as ({panels}),
             single as (select p ->> '$.targets[0].root_selector' as selector, p ->> '$.description' as description
                        from p where json_array_length(p -> '$.targets') = 1)
             select v.view_name from duckdb_views() v
             where starts_with(v.view_name, 'm_')
               and (select count(*) from single s
                    where s.selector = '$.' || v.view_name and s.description = v.comment) <> 1"
        ),
    );
    assert_eq!(
        mismatched, "",
        "metric views without a matching dashboard panel"
    );
    // Every series in the dashboard reads a metric that exists.
    let unknown = rows(
        &dir,
        &format!(
            "with p as ({panels}), t as (select unnest(json_extract_string(p, '$.targets[*].root_selector')) as s from p)
             select s from t where s not in (select '$.' || view_name from duckdb_views())"
        ),
    );
    assert_eq!(unknown, "", "dashboard series for unknown metrics");
}
