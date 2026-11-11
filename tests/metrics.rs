//! Metric views: hand-checked values on the fixture, the JSON export, the dashboard,
//! and properties on synthetic facts written straight into the database.

mod common;

use std::path::Path;

use common::{execute, facts, fixture, fixture_named, random_history, rev, rows, scan, tally};
use tempfile::TempDir;

#[test]
fn metric_views() {
    let r = fixture();
    let dir = &r.dir;
    scan(dir, "main");

    // Helper views.
    assert_eq!(
        rows(
            dir,
            "select seq, subject, is_fix, is_agent, is_bot from commit_info order by seq"
        ),
        "(0,c0,false,false,false) (1,c1,false,false,false) (2,c2,false,false,false) (3,merge,false,false,false)"
    );
    assert_eq!(
        rows(dir, "select name, public from fns order by name, tokens"),
        "(add,false) (add,false) (d,false) (plus,false)"
    );

    // Values checked by hand against the fixture history. The fixture has four
    // commits: shrink the process-metric windows to three code commits. Windows end at
    // each ISO week's last code commit: c2 (Jan 3) and c3 (Jan 5).
    execute(dir, "create or replace macro window_commits() as 3");
    let cases = [
        // Production tokens and files: a.rs (18), then src/b.rs (18) replaced by src/c.rs, then d.rs (9).
        (
            "select seq, value, files from m_size",
            "(0,18,1) (1,36,2) (2,36,2) (3,45,3)",
        ),
        // Both windows hold c1 (a.rs + src/b.rs, crosses modules) and c2 (src/b.rs + src/c.rs,
        // does not): observed 1/2. Shuffling the labels of their files (one in '', two in
        // src) crosses for 2 of 3 pairs.
        (
            "select seq, value, observed, expected, commits from m_hidden_coupling",
            "(2,0.7499999999999999,0.5,0.6666666666666667,2) (3,0.7499999999999999,0.5,0.6666666666666667,2)",
        ),
        // Only a.rs at c1 changed again within 7 days (one day after c0).
        (
            "select seq, value, commits, modifications from m_rework",
            "(2,1.0,1,1) (3,1.0,1,1)",
        ),
        // Window c0..c2: a.rs 1 + 1/2, src/b.rs 1/2 + 1/2, src/c.rs 1/2, so 1 / sum p^2 = 2.5714.
        // Window c1..c3: a.rs 1/2, src/b.rs 1, src/c.rs 1/2, d.rs 1, so 3.6.
        (
            "select seq, round(value, 4), round(files_per_commit, 4) from m_change_entropy",
            "(2,2.5714,1.6667) (3,3.6,1.6667)",
        ),
        // Module '' and src carry equal weight in both windows.
        (
            "select seq, value, modules from m_module_entropy",
            "(2,2.0,2) (3,2.0,2)",
        ),
        // No function is pub, so all function tokens are non-public.
        (
            "select seq, value, by_language from m_encapsulation",
            "(0,1.0,{rust=1.0}) (1,1.0,{rust=1.0}) (2,1.0,{rust=1.0}) (3,1.0,{rust=1.0})",
        ),
        // No function exceeds 200 tokens, none reaches the 50 tokens duplication counts.
        (
            "select seq, value from m_concentration",
            "(0,0.0) (1,0.0) (2,0.0) (3,0.0)",
        ),
        ("select count(*) as seq from m_duplication", "(0)"),
        // Window c0..c2: a.rs (c0, c1) and src/b.rs (c1, c2) each changed only by t.
        // Window c1..c3: only src/b.rs changes twice.
        (
            "select seq, value, files from m_ownership",
            "(2,1.0,2) (3,1.0,1)",
        ),
        // No branches, conditions, globals, throws or goto; each function is its body block.
        (
            "select seq, value, deep_share from m_nesting",
            "(0,0.0,0.0) (1,0.0,0.0) (2,0.0,0.0) (3,0.0,0.0)",
        ),
        (
            "select seq, value from m_decisions",
            "(0,0.0) (1,0.0) (2,0.0) (3,0.0)",
        ),
        (
            "select seq, value from m_global_state",
            "(0,0.0) (1,0.0) (2,0.0) (3,0.0)",
        ),
        (
            "select seq, value from m_exceptions",
            "(0,0.0) (1,0.0) (2,0.0) (3,0.0)",
        ),
        ("select count(*) as seq from m_goto", "(0)"),
        // Rust files without imports: no import graph, every file reaches nothing.
        ("select count(*) as seq from m_import_coverage", "(0)"),
        ("select count(*) as seq from m_cross_module_imports", "(0)"),
        // No file reaches another; with one file (c0) there is no other file to reach.
        (
            "select seq, value, mean_reach, median_reach, largest_cycle, files, rust from m_dependency_reach",
            "(1,0.0,0.0,0.0,1,2,0.0) (2,0.0,0.0,0.0,1,2,0.0) (3,0.0,0.0,0.0,1,3,0.0)",
        ),
        // Every changed file reaches nothing, as does every other: the middle percentile.
        (
            "select seq, value, reach from m_changed_file_reach",
            "(2,0.5,0.0) (3,0.5,0.0)",
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(
            rows(dir, &format!("{query} order by seq")),
            expected,
            "{query}"
        );
    }

    // Every metric view has the columns the export and the dashboard rely on.
    let metrics = rows(
        dir,
        "select view_name from duckdb_views() where starts_with(view_name, 'm_')",
    );
    for m in metrics.split(' ').map(|m| m.trim_matches(['(', ')'])) {
        let has = rows(
            dir,
            &format!("select count(*) from (describe {m}) where column_name in ('seq', 'value')"),
        );
        assert_eq!(has, "(2)", "{m} lacks seq or value");
    }
}

#[test]
fn metrics_json() {
    let r = fixture();
    let dir = &r.dir;
    scan(dir, "main");
    let out = tally(dir, &["metrics", "-", "m_hidden_coupling", "m_size"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let size = [("01", 18, 1), ("02", 36, 2), ("03", 36, 2), ("05", 45, 3)]
        .iter()
        .enumerate()
        .map(|(i, (day, tokens, files))| {
            format!(
                r#"{{"seq":{i},"time":"2026-01-{day}T00:00:00Z","value":{tokens},"files":{files}}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!(r#"{{"m_hidden_coupling":[],"m_size":[{size}]}}"#) + "\n"
    );

    // All metrics by default, in definition order; empty ones as [].
    let all = String::from_utf8(tally(dir, &["metrics", "-"]).stdout).unwrap();
    assert!(
        all.starts_with(r#"{"m_size":["#)
            && all.contains(r#""m_duplication":[]"#)
            && all.contains(r#""by_language":{"rust":1.0}"#),
        "{all}"
    );

    // A file output holds the same document; the partial file is renamed away.
    let path = dir.path().join("metrics.json");
    let out = tally(dir, &["metrics", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), all);
    assert!(!dir.path().join("metrics.partial").exists());

    // A metric removed from metrics.sql disappears from a database an older tally,
    // with other SQL, opened.
    execute(
        dir,
        "create view m_removed as select 0 as seq, 1 as value;
         create or replace macro tally_sql() as 'older'",
    );
    assert_eq!(
        String::from_utf8(tally(dir, &["metrics", "-"]).stdout).unwrap(),
        all
    );

    // Forgetting the output argument does not silently write a file named after a metric.
    let out = tally(dir, &["metrics", "m_size"]);
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        !out.status.success() && err.contains("is a metric"),
        "{err}"
    );
    assert!(!dir.path().join("m_size").exists());

    let out = tally(dir, &["metrics", "-", "nope"]);
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        !out.status.success() && err.contains("invalid value 'nope'"),
        "{err}"
    );
}

#[test]
fn dashboard_matches_metrics() {
    let dir = facts("");
    let dashboard = Path::new(env!("CARGO_MANIFEST_DIR")).join("grafana/dashboards/tally.json");
    let panels = format!(
        "select unnest(json_extract(json, '$.panels[*]')) as p from read_json_objects('{}')",
        dashboard.display()
    );
    // Each metric view has one single-target panel, described by the view's comment.
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

fn mean_value(dir: &TempDir, metric: &str) -> f64 {
    rows(dir, &format!("select avg(value) from {metric}"))
        .trim_matches(['(', ')'])
        .parse()
        .unwrap()
}

/// Shuffled module labels give a lift of 1: when co-change is independent of modules,
/// the chance level is exactly what is observed, at few and many modules.
#[test]
fn hidden_coupling_lift_is_one_under_the_null() {
    for (modules, files) in [(5, 40), (50, 200), (200, 800)] {
        let dir = facts(&random_history(0, 3000, modules, files, 3));
        let lift = mean_value(&dir, "m_hidden_coupling");
        assert!((lift - 1.0).abs() < 0.02, "{modules} modules: lift {lift}");
    }
}

/// Commits that bulk-add files or add a lone file in a new module do not rewrite past
/// windows of the process metrics.
#[test]
fn inert_commits_leave_past_windows_unchanged() {
    let dir = facts(&random_history(0, 300, 5, 40, 3));
    // The new commits come weeks later, so no window that ended before them moves.
    // Rounded: parallel sums may differ in the last bits.
    let windows = |d: &TempDir| {
        [
            "m_hidden_coupling",
            "m_rework",
            "m_change_entropy",
            "m_module_entropy",
        ]
        .map(|m| {
            rows(
                d,
                &format!("select seq, round(columns(* exclude (seq)), 9) from {m} order by seq"),
            )
        })
    };
    let before = windows(&dir);
    execute(
        &dir,
        "insert into commits values
           (300, 'bulk', timestamp '2026-01-01' + to_days(320), 't <t@t>', 'bulk'),
           (301, 'lone', timestamp '2026-01-01' + to_days(321), 't <t@t>', 'lone');
         insert into changes select 300, 'bulk/b' || i || '.rs', 'bulk' || i from range(40) r(i);
         insert into changes values (301, 'lone/x.rs', 'lone');
         insert into spans (blob, v, lang, kind, line, tokens)
           select blob, 3, 'rust', 'file', 1, 1 from changes where seq >= 300;",
    );
    let after = windows(&dir);
    for (b, a) in before.iter().zip(&after) {
        assert!(a.starts_with(b.as_str()), "past windows changed:\n{b}\n{a}");
    }
}

/// Renaming identifiers changes no structural metric: tokens, hashes and visibility
/// do not depend on names.
#[test]
fn renaming_identifiers_changes_no_structural_metric() {
    let original = fixture();
    scan(&original.dir, "main");
    let renamed = fixture_named("sum_of", "total", "delta");
    scan(&renamed.dir, "main");
    for m in [
        "m_size",
        "m_encapsulation",
        "m_duplication",
        "m_concentration",
    ] {
        let q = format!("select * from {m} order by seq");
        assert_eq!(rows(&renamed.dir, &q), rows(&original.dir, &q), "{m}");
    }
    let names = "select name from spans where kind = 'fn' order by name";
    assert_ne!(rows(&renamed.dir, names), rows(&original.dir, names));
}

const RUST_STRUCTURE: &str = "static mut COUNT: i32 = 0;
fn f(x: Option<i32>) -> i32 {
    if x.is_some() && x.unwrap() > 0 {
        for i in 0..3 {
            if i == 1 || i == 2 { panic!(\"no\"); }
        }
    } else if x.is_none() {
        return x.expect(\"e\");
    }
    match x { Some(v) => v, None => 0 }
}
";

const C_STRUCTURE: &str = "int counter = 0;
static int cache[4];
const int limit = 3;
extern int shared;
int f(int a) {
    if (a > 0 && a < limit) {
        while (a--) { if (a == 2) goto done; }
    } else if (a == 0) { longjmp(env, 1); }
    switch (a) { case 1: return 1; default: return a ? 2 : 3; }
done:
    return 0;
}
";

/// Per-blob structure facts, checked by hand on the samples whose spans
/// `tests/extract.rs` pins: decisions are branches plus conditions, and nesting is
/// the deepest block below the function body.
#[test]
fn blob_facts_count_structure() {
    let mut r = fixture();
    r.write("x.rs", RUST_STRUCTURE).write("x.c", C_STRUCTURE);
    r.extend("structure\n", 5);
    scan(&r.dir, "main");
    let facts = |path: &str| {
        let blob = rev(&r.dir, &format!("main:{path}"));
        rows(
            &r.dir,
            &format!(
                "select decisions, global_state, throws, gotos, nested_tokens / fn_tokens, deep_tokens
                 from blob_facts where blob = '{blob}'"
            ),
        )
    };
    // Rust: if, for, if, else-if and two match arms, plus && and ||; static mut; unwrap,
    // panic! and expect; blocks at depths 0 to 3.
    assert_eq!(facts("x.rs"), "(8,1,3,0,3.0,0)");
    // C: if, while, if, else-if, two case labels and a ternary, plus &&; two non-const
    // globals; longjmp; goto; blocks at depths 0 to 2.
    assert_eq!(facts("x.c"), "(8,2,1,1,2.0,0)");
}

/// `code_totals` and `m_duplication`, built from change points, equal computing them
/// from the live files commit by commit, through additions, a modification that
/// breaks a structural twin, a deletion and a rename.
#[test]
fn change_points_match_per_commit_sums() {
    let mut r = fixture();
    r.write("x.rs", RUST_STRUCTURE)
        .write("twin.rs", RUST_STRUCTURE);
    r.extend("add rust\n", 5);
    r.write("x.c", C_STRUCTURE);
    r.extend("add c\n", 6);
    r.write(
        "x.rs",
        RUST_STRUCTURE.replace("        for i in 0..3 {\n", "        {\n"),
    );
    r.extend("modify rust\n", 7);
    r.remove("x.c");
    r.extend("delete c\n", 8);
    r.remove("x.rs").write("src/y.rs", RUST_STRUCTURE);
    r.extend("rename rust\n", 9);
    scan(&r.dir, "main");
    // Every fact of every live production file, summed commit by commit.
    let naive = "select c.seq, f.lang, sum(columns(f.* exclude (blob, lang)))
         from commits c join files l on l.since <= c.seq and c.seq < l.until
         join path_roles p on p.path = l.path and p.role = 'code'
         join blob_facts f on f.blob = l.blob
         group by all order by all";
    let expected = rows(&r.dir, naive);
    let actual = rows(&r.dir, "select * from code_totals order by all");
    assert_eq!(actual, expected);
    let naive_duplication = "with g as (
           select c.seq, s.lang, s.hash, sum(s.tokens) as t, count(*) as n
           from commits c join files l on l.since <= c.seq and c.seq < l.until
           join path_roles p on p.path = l.path and p.role = 'code'
           join spans s on s.blob = l.blob and s.kind = 'fn' and s.tokens >= clone_tokens()
           group by all)
         select seq, coalesce(sum(t) filter (n > 1), 0) / sum(t) from g group by seq order by seq";
    let duplication = rows(&r.dir, naive_duplication);
    assert_eq!(
        rows(&r.dir, "select seq, value from m_duplication order by seq"),
        duplication
    );
    assert!(
        duplication.contains("(4,1.0)") && duplication.contains("(6,0.0)"),
        "{duplication}"
    );
    // Not vacuous: C appears at seq 5 and is gone from seq 7.
    assert!(
        expected.contains("(5,c,1,") && !expected.contains("(7,c,"),
        "{expected}"
    );
}

/// SQL for one commit adding `files` Python files spread over `modules` directories,
/// each importing `per_file` others picked at random among the first `targets`
/// files: imports are independent of modules by construction, and few targets make
/// hubs.
fn random_imports(modules: u32, files: u32, targets: u32, per_file: u32) -> String {
    format!(
        "insert into commits values (0, 'c0', timestamp '2026-01-01', 't <t@t>', 'init');
         insert into changes select 0, 'm' || (f % {modules}) || '/f' || f || '.py', 'b' || f from range({files}) g(f);
         insert into spans (blob, v, lang, kind, line, tokens) select 'b' || f, 6, 'python', 'file', 1, 1 from range({files}) g(f);
         insert into spans (blob, v, lang, kind, name, line, start_byte, end_byte)
           select 'b' || f, 6, 'python', 'import', 'f' || t, 1, t, t + 1
           from range({files}) a(f), range({targets}) b(t) where f <> t
           qualify row_number() over (partition by f order by md5(f || ':' || t)) <= {per_file};"
    )
}

/// Rewiring imports at random, keeping how much each module imports and is
/// imported, gives a lift of 1: also when a few hubs take all the imports.
#[test]
fn import_coupling_lift_is_one_under_the_null() {
    for (modules, targets) in [(5, 600), (50, 600), (50, 5)] {
        let dir = facts(&random_imports(modules, 600, targets, 4));
        assert_eq!(
            rows(
                &dir,
                "select class, count(*) from import_status group by all"
            ),
            "(resolved,2400)"
        );
        let lift = mean_value(&dir, "m_cross_module_imports");
        assert!(
            (lift - 1.0).abs() < 0.03,
            "{modules} modules, {targets} targets: lift {lift}"
        );
    }
}

/// The dependency metrics on a small Python graph, checked by hand: a/x imports a/y,
/// and a/y and b/z import each other; b/w imports nothing.
#[test]
fn dependency_metrics_by_hand() {
    let mut r = common::Repo::new();
    let x = |n: u32| format!("from . import y\n{}", "# edit\n".repeat(n as usize));
    let w = |n: u32| format!("w = 1\n{}", "# edit\n".repeat(n as usize));
    r.write("a/x.py", x(0))
        .write("a/y.py", "from ..b import z\n")
        .write("b/z.py", "from ..a import y\n")
        .write("b/w.py", w(0));
    r.commit("main", "init\n", 0, &[]);
    r.write("a/x.py", x(1));
    r.extend("x\n", 1);
    r.write("a/y.py", "from ..b import z\n# edit\n")
        .write("b/z.py", "from ..a import y\n# edit\n");
    r.extend("y and z\n", 2);
    r.write("a/x.py", x(2)).write("b/w.py", w(1));
    r.extend("x and w\n", 3);
    scan(&r.dir, "main");
    // `metrics` rebuilds the cones that m_dependency_reach and m_changed_file_reach read.
    assert!(tally(&r.dir, &["metrics", "-"]).status.success());
    // One window: the week's last commit, over the last three code commits.
    execute(&r.dir, "create or replace macro window_commits() as 3");
    let cases = [
        (
            "select seq, value, fallback_share, imports, python from m_import_coverage where seq = 3",
            "(3,1.0,0.0,3,1.0)",
        ),
        // Edges x->y (a to a), y->z (a to b), z->y (b to a): 2 of 3 cross. a imports twice
        // and is imported twice, b once each: 5 of 9 rewired edges stay inside, 4/9 cross.
        (
            "select seq, round(value, 4), edges, round(python, 4) from m_cross_module_imports where seq = 3",
            "(3,1.5,3,1.5)",
        ),
        // Cones: x 2, y 1, z 1, w 0 of 3 others: (2 + 1 + 1 + 0) / 3 / 4.
        (
            "select seq, round(value, 4), mean_reach, median_reach, largest_cycle, files from m_dependency_reach where seq = 3",
            "(3,0.3333,1.0,1.0,2,4)",
        ),
        // Cone percentiles among the 4 files, ties counting half: x 3.5/4, y and z
        // 2/4, w 0.5/4. Commits: x (0.875, reach 2), y and z (0.5, 1), x and w (0.5, 1).
        (
            "select seq, value, round(reach, 4), commits from m_changed_file_reach",
            "(3,0.625,1.3333,3)",
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(rows(&r.dir, query), expected, "{query}");
    }
}

/// `family`, by extension in SQL, agrees with the extensions of `LANGS`.
#[test]
fn families_match_languages() {
    let dir = facts("");
    for lang in tally::LANGS {
        let expected = match lang.name {
            "c" | "cpp" => "c",
            "typescript" | "tsx" | "javascript" => "ecma",
            other => other,
        };
        for ext in lang.exts {
            assert_eq!(
                rows(&dir, &format!("select family('a/b.{ext}')")),
                format!("({expected})"),
                "{ext}"
            );
        }
    }
    assert_eq!(rows(&dir, "select family('a/b.md')"), "(NULL)");
}
