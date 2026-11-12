//! Metric views: hand-checked values on the fixture, the JSON export, the dashboard,
//! and properties on synthetic facts written straight into the database.

mod common;

use std::path::Path;

use common::{
    assert_rows, execute, facts, fails, fixture, fixture_named, random_history, rev, rows, scan,
    succeeds,
};
use tempfile::TempDir;

#[test]
fn metric_views() {
    let r = fixture();
    let dir = &r.dir;
    scan(dir, "main");

    // Values checked by hand against the fixture history. The fixture has four
    // commits: shrink the process-metric windows to three code commits. Windows end at
    // each ISO week's last code commit: c2 (Jan 3) and c3 (Jan 5).
    execute(dir, "create or replace macro window_commits() as 3");
    let cases = [
        // Production tokens and files: a.rs (18), then src/b.rs (18) replaced by src/c.rs, then d.rs (9).
        (
            "select seq, value, files from metrics.size",
            "(0,18,1) (1,36,2) (2,36,2) (3,45,3)",
        ),
        // c2 renames src/b.rs to src/c.rs: one change, so only c1 (a.rs and src/b.rs:
        // 1 step apart) changes two files, as far apart as its window's two files.
        (
            "select seq, round(value, 4), observed, round(expected, 4), commits from metrics.co_change_distance",
            "(2,1.0,1.0,1.0,1) (3,1.0,1.0,1.0,1)",
        ),
        // Only a.rs at c1 changed again within 7 days (one day after c0); the rename at
        // c2 moves src/b.rs without editing it.
        (
            "select seq, value, commits, modifications from metrics.rework",
            "(2,1.0,1,1) (3,1.0,1,1)",
        ),
        // src/b.rs and src/c.rs are one lineage (b). Window c0..c2: a.rs 1 + 1/2, b 1/2 + 1,
        // so 1 / sum p^2 = 2. Window c1..c3: a.rs 1/2, b 3/2, d.rs 1, so 2.5714.
        (
            "select seq, round(value, 4), round(files_per_commit, 4), round(rust, 4) from metrics.change_entropy",
            "(2,2.0,1.3333,2.0) (3,2.5714,1.3333,2.5714)",
        ),
        // Files per commit in each window: c0 1, c1 2, c2 1 (the rename is one); c3 1.
        (
            "select seq, round(value, 4), p90, max, commits from metrics.change_span",
            "(2,1.3333,2,2,3) (3,1.3333,2,2,3)",
        ),
        // No function is pub, so all function tokens are non-public.
        (
            "select seq, value, by_language from metrics.encapsulation",
            "(0,1.0,{rust=1.0}) (1,1.0,{rust=1.0}) (2,1.0,{rust=1.0}) (3,1.0,{rust=1.0})",
        ),
        // No function exceeds 200 tokens, none reaches the 50 tokens duplication counts.
        (
            "select seq, value from metrics.concentration",
            "(0,0.0) (1,0.0) (2,0.0) (3,0.0)",
        ),
        ("select count(*) as seq from metrics.duplication", "(0)"),
        // Window c0..c2: a.rs (c0, c1) and lineage src/b.rs, src/c.rs (c1, c2) each
        // changed only by t. Window c1..c3: only that lineage changes twice.
        (
            "select seq, value, files from metrics.ownership",
            "(2,1.0,2) (3,1.0,1)",
        ),
        // No branches, conditions, globals, throws or goto; each function is its body block.
        // Per language: the fixture is all Rust, so `rust` is the value and `c` is NULL.
        (
            "select seq, value, deep_share, rust, c from metrics.nesting",
            "(0,0.0,0.0,0.0,NULL) (1,0.0,0.0,0.0,NULL) (2,0.0,0.0,0.0,NULL) (3,0.0,0.0,0.0,NULL)",
        ),
        (
            "select seq, value from metrics.decisions",
            "(0,0.0) (1,0.0) (2,0.0) (3,0.0)",
        ),
        // Parameters: add (2), then plus (2), then d (0); nine in ten take at most 2.
        (
            "select seq, value, max, share, functions from metrics.function_params",
            "(0,2,2,0.0,1) (1,2,2,0.0,2) (2,2,2,0.0,2) (3,2,2,0.0,3)",
        ),
        (
            "select seq, value, max from metrics.function_decisions",
            "(0,0,0) (1,0,0) (2,0,0) (3,0,0)",
        ),
        (
            "select seq, value from metrics.global_state",
            "(0,0.0) (1,0.0) (2,0.0) (3,0.0)",
        ),
        (
            "select seq, value from metrics.exceptions",
            "(0,0.0) (1,0.0) (2,0.0) (3,0.0)",
        ),
        ("select count(*) as seq from metrics.goto", "(0)"),
        // Rust files without imports: no import graph, every file reaches nothing.
        ("select count(*) as seq from metrics.import_coverage", "(0)"),
        ("select count(*) as seq from metrics.import_distance", "(0)"),
        // Read at each week's last commit (c2, then c3): no file reaches another.
        (
            "select seq, value, mean_reach, median_reach, largest_cycle, files, rust from metrics.dependency_reach",
            "(2,0.0,0.0,0,1,2,0.0) (3,0.0,0.0,0,1,3,0.0)",
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
        "select view_name from duckdb_views() where schema_name = 'metrics'",
    );
    for m in metrics.split(' ').map(|m| m.trim_matches(['(', ')'])) {
        let has = rows(
            dir,
            &format!(
                "select count(*) from (describe metrics.{m}) where column_name in ('seq', 'value')"
            ),
        );
        assert_eq!(has, "(2)", "{m} lacks seq or value");
    }
}

/// The JSON export, and what `metrics` refuses: a missing or empty database, an
/// output that names a metric, unknown metrics. It reads a read-only database and
/// ignores views a user added.
#[test]
fn export() {
    let r = fixture();
    let dir = &r.dir;
    scan(dir, "main");
    let out = succeeds(dir, &["metrics", "-", "co_change_distance", "size"]);
    let size = [("01", 18, 1), ("02", 36, 2), ("03", 36, 2), ("05", 45, 3)]
        .iter()
        .enumerate()
        .map(|(i, (day, tokens, files))| {
            format!(
                r#"{{"seq":{i},"time":"2026-01-{day}T00:00:00Z","value":{tokens},"files":{files},"c":null,"rust":{tokens},"python":null,"ecma":null}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        out,
        format!(r#"{{"co_change_distance":[],"size":[{size}]}}"#) + "\n"
    );

    // All metrics by default, in definition order; empty ones as [].
    let all = succeeds(dir, &["metrics", "-"]);
    assert!(
        all.starts_with(r#"{"size":["#)
            && all.contains(r#""duplication":[]"#)
            && all.contains(r#""by_language":{"rust":1.0}"#),
        "{all}"
    );

    // A file output holds the same document; the partial file is renamed away.
    let path = dir.path().join("metrics.json");
    succeeds(dir, &["metrics", path.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), all);
    assert!(!dir.path().join("metrics.partial").exists());

    // A metric removed from codevitals' SQL disappears from a database an older codevitals,
    // with other SQL, opened.
    execute(
        dir,
        "create view metrics.removed as select 0 as seq, 1 as value;
         create or replace macro codevitals_sql() as 'older'",
    );
    assert_eq!(succeeds(dir, &["metrics", "-"]), all);

    // Forgetting the output argument does not silently write a file named after a metric.
    let err = fails(dir, &["metrics", "size"]);
    assert!(err.contains("is a metric"), "{err}");
    assert!(!dir.path().join("size").exists());

    let err = fails(dir, &["metrics", "-", "nope"]);
    assert!(err.contains("invalid value 'nope'"), "{err}");
    let r = fixture();
    let err = fails(&r.dir, &["metrics", "-"]);
    assert!(err.contains("no database at"), "{err}");
    assert!(!common::db(&r.dir).exists());

    execute(&r.dir, include_str!("../sql/schema.sql"));
    let err = fails(&r.dir, &["metrics", "-"]);
    assert!(err.contains("has no commits yet"), "{err}");

    scan(&r.dir, "main");
    execute(
        &r.dir,
        "create view metrics.mine as select 0 as seq, 1 as value; create view \"my view\" as select 1",
    );
    let db = common::db(&r.dir);
    let mut perms = std::fs::metadata(&db).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&db, perms).unwrap();
    let out = succeeds(&r.dir, &["metrics", "-", "size", "size"]);
    assert_eq!(out.matches("\"size\"").count(), 1);
}

#[test]
fn dashboard_matches_metrics() {
    let dir = facts("");
    let dashboard =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("grafana/dashboards/codevitals.json");
    let panels = format!(
        "select unnest(json_extract(json, '$.panels[*]')) as p from read_json_objects('{}')",
        dashboard.display()
    );
    // Each metric view has one single-target panel (besides its by-language copy),
    // described by the view's comment after its `CATEGORY: Title` line.
    let mismatched = rows(
        &dir,
        &format!(
            "with p as ({panels}),
             single as (select p ->> '$.targets[0].root_selector' as selector, p ->> '$.description' as description
                        from p where json_array_length(p -> '$.targets') = 1
                          and not contains(p ->> '$.title', '(by language)'))
             select v.view_name from duckdb_views() v
             where v.schema_name = 'metrics'
               and (select count(*) from single s
                    where s.selector = '$.' || v.view_name
                      and s.description = regexp_replace(v.comment, '^[^\n]*\n\n', '')) <> 1"
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

/// When co-change and imports are independent of where files live, observed
/// distances equal their chance level: a lift of 1, over few and many directories,
/// and when a few hubs take all the imports.
#[test]
fn lifts_are_one_under_the_null() {
    for (modules, files) in [(5, 40), (50, 200), (200, 800)] {
        let dir = facts(&random_history(0, 3000, modules, files, 3));
        let lift = mean_value(&dir, "metrics.co_change_distance");
        assert!((lift - 1.0).abs() < 0.02, "{modules} modules: lift {lift}");
    }
    for (modules, targets) in [(5, 600), (50, 600), (50, 5)] {
        let dir = facts(&random_imports(modules, 600, targets, 4));
        assert_rows(
            &dir,
            "select class, count(*) from import_status group by all",
            "(resolved,2400)",
        );
        let lift = mean_value(&dir, "metrics.import_distance");
        assert!(
            (lift - 1.0).abs() < 0.03,
            "{modules} modules, {targets} targets: lift {lift}"
        );
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
            "metrics.co_change_distance",
            "metrics.rework",
            "metrics.change_entropy",
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
           (300, 'bulk', timestamp '2026-01-01' + to_days(320), 't', 't@t', 'bulk'),
           (301, 'lone', timestamp '2026-01-01' + to_days(321), 't', 't@t', 'lone');
         insert into changes (seq, path, blob) select 300, 'bulk/b' || i || '.rs', 'bulk' || i from range(40) r(i);
         insert into changes (seq, path, blob) values (301, 'lone/x.rs', 'lone');
         insert into spans (blob, v, lang, kind, line, tokens)
           select blob, 1, 'rust', 'file', 1, 1 from changes where seq >= 300;",
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
        "metrics.size",
        "metrics.encapsulation",
        "metrics.duplication",
        "metrics.concentration",
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

/// `code_totals` and `duplication`, built from change points, equal computing them
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
    let naive = "select c.seq, f.lang, sum(columns(f.* exclude (blob, test_features, lang)))
         from commits c join files l on l.since <= c.seq and c.seq < l.until
         join path_roles p on p.path = l.path and p.role = 'code'
         join blob_facts f on f.blob = l.blob and f.test_features = l.test_features
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
    assert_rows(
        &r.dir,
        "select seq, value from metrics.duplication order by seq",
        &duplication,
    );
    assert!(
        duplication.contains("(4,1.0)") && duplication.contains("(6,0.0)"),
        "{duplication}"
    );
    // The per-function tails, from histograms by change points, equal the nearest-rank
    // 90th percentile, maximum and share over the live functions commit by commit.
    for (measure, threshold) in [
        ("decisions", "many_decisions()"),
        ("nesting", "deep_nesting()"),
        ("params", "many_params()"),
    ] {
        let naive = format!(
            "with f as (
               select c.seq, x.{measure} as v, x.tokens
               from commits c join files l on l.since <= c.seq and c.seq < l.until
               join path_roles p on p.path = l.path and p.role = 'code'
               join function_facts x on x.blob = l.blob and x.test_features = l.test_features
               where x.{measure} is not null),
             r as (select *, row_number() over (partition by seq order by v) as k,
                          count(*) over (partition by seq) as n from f)
             select seq, min(v) filter (k >= ceil(0.9 * n)), max(v),
                    round(coalesce(sum(tokens) filter (v > {threshold}), 0) / sum(tokens), 9)
             from r group by seq order by seq"
        );
        let metric = format!(
            "select seq, value, max, round(share, 9) from metrics.function_{measure} order by seq"
        );
        assert_eq!(rows(&r.dir, &metric), rows(&r.dir, &naive), "{measure}");
    }
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
        "insert into commits values (0, 'c0', timestamp '2026-01-01', 't', 't@t', 'init');
         insert into changes (seq, path, blob) select 0, 'm' || (f % {modules}) || '/f' || f || '.py', 'b' || f from range({files}) g(f);
         insert into spans (blob, v, lang, kind, line, tokens) select 'b' || f, 1, 'python', 'file', 1, 1 from range({files}) g(f);
         insert into spans (blob, v, lang, kind, name, line, start_byte, end_byte)
           select 'b' || f, 1, 'python', 'import', 'f' || t, 1, t, t + 1
           from range({files}) a(f), range({targets}) b(t) where f <> t
           qualify row_number() over (partition by f order by md5(f || ':' || t)) <= {per_file};"
    )
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
    // `metrics` rebuilds the cones that dependency_reach reads.
    succeeds(&r.dir, &["metrics", "-"]);
    // One window: the week's last commit, over the last three code commits.
    execute(&r.dir, "create or replace macro window_commits() as 3");
    let cases = [
        (
            "select seq, value, fallback_share, imports, python from metrics.import_coverage where seq = 3",
            "(3,1.0,0.0,3,1.0)",
        ),
        // Edges x->y (0 steps), y->z and z->y (2 steps each): observed 4/3. Rewired at
        // random: importers and imported files sit 1 deep (2 steps on average apart), less
        // twice the stubs that share a directory, (a: 2 out x 2 in, b: 1 x 1) / 9: 8/9.
        (
            "select seq, round(value, 4), edges, round(python, 4) from metrics.import_distance where seq = 3",
            "(3,1.5,3,1.5)",
        ),
        // Cones: x 2, y 1, z 1, w 0 of 3 others: (2 + 1 + 1 + 0) / 3 / 4.
        (
            "select seq, round(value, 4), mean_reach, median_reach, largest_cycle, files from metrics.dependency_reach where seq = 3",
            "(3,0.3333,1.0,1,2,4)",
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(rows(&r.dir, query), expected, "{query}");
    }
}

/// The SQL matches the languages: `family()` and `lang_family()` agree with `LANGS`,
/// every family has every per-family rule, every per-family metric has a column for
/// each family, and every span kind the SQL reads is one a query captures.
#[test]
fn sql_covers_every_language() {
    let dir = facts("");
    for lang in codevitals::LANGS {
        let expected = match lang.name {
            "c" | "cpp" => "c",
            "typescript" | "tsx" | "javascript" => "ecma",
            other => other,
        };
        if !lang.exts.is_empty() {
            assert_eq!(
                rows(&dir, &format!("select lang_family('{}')", lang.name)),
                format!("({expected})"),
                "{}",
                lang.name
            );
        }
        for ext in lang.exts {
            assert_eq!(
                rows(&dir, &format!("select family('a/b.{ext}')")),
                format!("({expected})"),
                "{ext}"
            );
        }
    }
    assert_eq!(rows(&dir, "select family('a/b.md')"), "(NULL)");
    let dir = facts("");
    let sql = codevitals::SQL;
    let families = rows(
        &dir,
        &format!(
            "select distinct family('x.' || ext) from unnest([{}]) t(ext) order by 1",
            codevitals::LANGS
                .iter()
                .flat_map(|l| l.exts)
                .map(|e| format!("'{e}'"))
                .collect::<Vec<_>>()
                .join(",")
        ),
    );
    for family in families.split(' ').map(|f| f.trim_matches(['(', ')'])) {
        for rule in [
            format!("when '{family}' then"),
            format!("where family(path) = '{family}'"),
        ] {
            assert!(
                sql.contains(rule.as_str()),
                "codevitals' SQL lacks `{rule}`"
            );
        }
    }
    // Every metric with a column per family has one for each: (complete, incomplete).
    let names: Vec<String> = families
        .split(' ')
        .map(|f| format!("'{}'", f.trim_matches(['(', ')'])))
        .collect();
    assert_rows(
        &dir,
        &format!(
            "select count(*) filter (n = {}), count(*) filter (n <> {}) from (
               select count(*) as n from duckdb_columns()
               where schema_name = 'metrics' and column_name in ({}) group by table_name)",
            names.len(),
            names.len(),
            names.join(", ")
        ),
        "(15,0)",
    );
    let sql = codevitals::SQL;
    let mut kinds = Vec::new();
    for (i, _) in sql.match_indices("kind ") {
        let rest = &sql[i + 5..];
        let literals = if let Some(r) = rest.strip_prefix("= ") {
            &r[..r
                .find(|c: char| c != '\'' && !c.is_ascii_lowercase() && c != '_')
                .unwrap()]
        } else if let Some(r) = rest.strip_prefix("in (") {
            &r[..r.find(')').unwrap()]
        } else {
            continue;
        };
        kinds.extend(
            literals
                .split('\'')
                .filter(|k| k.chars().all(|c| c.is_ascii_lowercase() || c == '_') && !k.is_empty()),
        );
    }
    assert!(kinds.len() > 10, "{kinds:?}");
    for kind in kinds {
        assert!(
            kind == "file" || codevitals::KINDS.contains(&kind),
            "the SQL reads span kind {kind:?}, which no query captures"
        );
    }
}

/// A lone wrong clock (a commit dated 1970 or 2999 among its neighbours) takes its
/// neighbours' time, and times never fall back.
#[test]
fn commit_times_drop_a_lone_wrong_clock() {
    let dir = facts(
        "insert into commits values
           (0, 'a', timestamp '2026-01-01', 't', 't@t', 'x'), (1, 'b', timestamp '1970-01-01', 't', 't@t', 'x'),
           (2, 'c', timestamp '2026-01-03', 't', 't@t', 'x'), (3, 'd', timestamp '2999-01-01', 't', 't@t', 'x'),
           (4, 'e', timestamp '2026-01-05', 't', 't@t', 'x'), (5, 'f', timestamp '2026-01-04', 't', 't@t', 'x')",
    );
    assert_rows(
        &dir,
        "select seq, time::date from commit_times order by seq",
        "(0,2026-01-01) (1,2026-01-01) (2,2026-01-03) (3,2026-01-05) (4,2026-01-05) (5,2026-01-05)",
    );
}

/// Rust test code inside production files is subtracted from their facts: a
/// `#[cfg(test)]` module, a `#[test]` function, code behind a feature that is
/// test-only by name or declared so in .gitattributes. A module file declared only
/// for tests (`#[cfg(test)] mod testutil;`) is test code as a whole.
#[test]
fn rust_test_code_is_not_production() {
    let production =
        "pub fn f(x: Option<i32>) -> i32 {\n    if x.is_some() { x.unwrap() } else { 0 }\n}\n";
    let tests = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { if true { super::f(None); } let _ = Some(1).unwrap(); }\n}\n\
                 #[cfg(feature = \"test-util\")]\nfn util() { panic!(); }\n\
                 #[cfg(feature = \"mock\")]\nfn mocked() { if true { panic!(); } }\n\
                 #[cfg(test)]\nmod testutil;\n";
    let mut r = common::Repo::new();
    r.write("src/plain.rs", production)
        .write("src/lib.rs", format!("{production}{tests}"))
        .write("src/testutil.rs", "pub fn helper() { todo!() }\n")
        .write("mock/src/lib.rs", format!("{production}{tests}"))
        .write(".gitattributes", "mock/** codevitals-test-features=mock\n");
    r.commit("main", "c0\n", 0, &[]);
    scan(&r.dir, "main");
    let facts = "select r.path, f.files, f.tokens, f.decisions, f.throws, f.fn_tokens
         from files v join path_roles r using (path)
         join blob_facts f on f.blob = v.blob and f.test_features = v.test_features
         where r.role = 'code' order by r.path";
    let plain = rows(
        &r.dir,
        &facts.replace("where", "where r.path = 'src/plain.rs' and"),
    );
    let plain = plain.trim_start_matches("(src/plain.rs,");
    // lib.rs keeps `mocked` (mock is not test-only there); mock/src/lib.rs drops it too.
    assert_eq!(
        rows(&r.dir, facts),
        format!("(mock/src/lib.rs,{plain} (src/lib.rs,1,58,2,2,47) (src/plain.rs,{plain}")
    );
    assert_rows(
        &r.dir,
        "select path, role from path_roles where path like 'src/test%'",
        "(src/testutil.rs,test)",
    );
}

/// Per-unit tables: functions keep their identity across edits and file renames,
/// are told apart by scope and ordinal, and sum to the per-blob facts; `cv units`
/// exports one commit's files and functions.
#[test]
fn units_follow_files_and_functions() {
    let v1 = "struct A;\nimpl A {\n    pub fn new() -> A { A }\n}\nimpl Clone for A {\n    fn clone(&self) -> A { if true { A } else { A } }\n}\n\
              #[cfg(unix)]\nfn os() {}\n#[cfg(windows)]\nfn os() {}\n";
    let mut r = common::Repo::new();
    r.write("a.rs", v1);
    r.commit("main", "c0\n", 0, &[]);
    r.write(
        "a.rs",
        v1.replace("{ A } else { A }", "{ A } else if false { A } else { A }"),
    );
    r.extend("edit\n", 1);
    r.remove("a.rs").write(
        "src/a.rs",
        v1.replace("{ A } else { A }", "{ A } else if false { A } else { A }"),
    );
    r.extend("move\n", 2);
    scan(&r.dir, "main");
    assert_rows(
        &r.dir,
        "select since, path, function, decisions, public from units.functions order by since, start_byte",
        "(0,a.rs,0:a.rs A::new,0,true) (0,a.rs,0:a.rs <A as Clone>::clone,1,false) (0,a.rs,0:a.rs os,0,false) (0,a.rs,0:a.rs os#2,0,false) \
         (1,a.rs,0:a.rs A::new,0,true) (1,a.rs,0:a.rs <A as Clone>::clone,2,false) (1,a.rs,0:a.rs os,0,false) (1,a.rs,0:a.rs os#2,0,false) \
         (2,src/a.rs,0:a.rs A::new,0,true) (2,src/a.rs,0:a.rs <A as Clone>::clone,2,false) (2,src/a.rs,0:a.rs os,0,false) (2,src/a.rs,0:a.rs os#2,0,false)",
    );
    // Functions add up to their file's facts.
    assert_rows(
        &r.dir,
        "select f.since, f.fn_tokens = sum(u.tokens), f.decisions = sum(u.decisions)
         from units.files f join units.functions u using (path, since)
         group by f.since, f.fn_tokens, f.decisions order by 1",
        "(0,true,true) (1,true,true) (2,true,true)",
    );
    let first = rev(&r.dir, "main~2");
    let out = succeeds(&r.dir, &["units", "-", "--rev", &first[..8]]);
    assert!(
        out.starts_with(&format!(
            r#"{{"seq":0,"id":"{first}","files":[{{"path":"a.rs","lineage":"0:a.rs","#
        )),
        "{out}"
    );
    assert_eq!(out.matches(r#""function":"#).count(), 4, "{out}");
    let err = fails(&r.dir, &["units", "-", "--rev", "ffffffff"]);
    assert!(err.contains("no scanned commit"), "{err}");
}

/// Hotspots rank live production files by recent changes times decision points,
/// following renames; files changed only by bulk commits or long ago count no change.
#[test]
fn hotspots_rank_change_times_complexity() {
    let busy = |n: usize| {
        format!(
            "fn f(x: i32) -> i32 {{\n    if x > {n} {{ 1 }} else {{ 2 }}\n}}\n\nfn a() {{}}\n\nfn b() {{}}\n"
        )
    };
    let complex = "fn g(x: i32) -> i32 { if x > 1 { if x > 2 { 3 } else { 4 } } else { 5 } }\n";
    let mut r = common::Repo::new();
    r.write("busy.rs", busy(0))
        .write("complex.rs", complex)
        .write("old.rs", complex);
    r.commit("main", "c0\n", 0, &[]);
    for day in 400..404 {
        r.write("busy.rs", busy(day as usize));
        r.extend("edit\n", day);
    }
    r.remove("busy.rs").write("src/busy.rs", busy(9)); // a rename keeps the count
    r.write("complex.rs", format!("{complex}\n"));
    r.extend("move\n", 404);
    scan(&r.dir, "main");
    // busy: 5 changes x 1 decision; complex: 1 x 2; old.rs: no change within a year.
    assert_rows(
        &r.dir,
        "select rank, path, changes, decisions, score from units.hotspots order by rank, path",
        "(1,src/busy.rs,5,1,5) (2,complex.rs,1,2,2) (3,old.rs,0,2,0)",
    );
    let text = succeeds(&r.dir, &["hotspots"]);
    assert!(
        text.lines()
            .nth(1)
            .unwrap()
            .ends_with("5 changes x    1 decisions  src/busy.rs"),
        "{text}"
    );
    assert_eq!(
        text.lines().count(),
        3,
        "files without recent changes are left out: {text}"
    );
}

/// Orphaning: code mostly changed by authors who stopped committing.
#[test]
fn orphaning_counts_departed_authors() {
    let f = |n: u32| format!("fn f(x: i32) -> i32 {{ x + {n} }}\n");
    let mut r = common::Repo::new();
    r.author("Old", "old@x")
        .write("a.rs", f(0))
        .write("b.rs", f(0));
    r.commit("main", "c0\n", 0, &[]);
    r.write("a.rs", f(1));
    r.extend("c1\n", 1);
    r.author("New", "new@x");
    for day in 400..403 {
        r.write("b.rs", f(day));
        r.extend("edit b\n", day as i64);
    }
    scan(&r.dir, "main");
    execute(&r.dir, "create or replace macro window_commits() as 3");
    // At the last point only New committed in the past year: a.rs has all its changes
    // by Old (orphaned), b.rs one of four.
    assert_rows(
        &r.dir,
        "select path, changes, orphaned from units.orphaning where seq = 4 order by path",
        "(a.rs,2,1.0) (b.rs,4,0.25)",
    );
    assert_rows(
        &r.dir,
        "select seq, value, orphaned_files, files from metrics.orphaning order by seq",
        "(4,0.5,1,2)",
    );
}

/// Accretion: changes that make an existing function branchier or deeper; new
/// functions and pure moves do not count.
#[test]
fn accretion_counts_growth_of_existing_functions() {
    let mut r = common::Repo::new();
    let base = "fn f(x: i32) -> i32 {\n    x\n}\n\nfn keep() {}\n\nfn other() {}\n";
    r.write("a.rs", base);
    r.commit("main", "c0\n", 0, &[]);
    let grown = base.to_owned() + "fn g(x: i32) -> i32 {\n    if x > 0 { 1 } else { 2 }\n}\n";
    r.write("a.rs", &grown); // a new function: no accretion
    r.extend("add g\n", 1);
    let deeper = grown.replace("    x\n", "    if x > 1 { x } else { 0 }\n");
    r.write("a.rs", &deeper); // f gains a decision: accretion
    r.extend("grow f\n", 2);
    r.remove("a.rs").write("src/a.rs", &deeper); // a pure move: no touch
    r.extend("move\n", 3);
    r.write(
        "src/a.rs",
        deeper.replace("fn keep() {}", "fn keep() { let _y = 1; }"),
    );
    r.extend("edit keep\n", 4);
    scan(&r.dir, "main");
    execute(&r.dir, "create or replace macro window_commits() as 4");
    assert_rows(
        &r.dir,
        "select seq, path, raised from units.touches order by seq",
        "(1,a.rs,false) (2,a.rs,true) (4,src/a.rs,false)",
    );
    // Window c1..c4: three touches of one lineage, one raising.
    assert_rows(
        &r.dir,
        "select seq, round(value, 4), files, touches from metrics.accretion order by seq",
        "(4,0.3333,1,3)",
    );
}

/// Modularity violations: files changed together often whose imports do not
/// connect them.
#[test]
fn modularity_violations_need_no_import() {
    let mut r = common::Repo::new();
    for day in 0..4 {
        r.write(
            "pkg/a.py",
            format!("import pkg.b\n\ndef a():\n    return {day}\n"),
        )
        .write("pkg/b.py", format!("def b():\n    return {day}\n"))
        .write("pkg/c.py", format!("def c():\n    return {day}\n"));
        match day {
            0 => r.commit("main", "c0\n", 0, &[]),
            d => r.extend("edit\n", d),
        };
    }
    scan(&r.dir, "main");
    execute(&r.dir, "create or replace macro window_commits() as 4");
    // a, b and c change together four times; only a and b are connected by an import.
    assert_rows(
        &r.dir,
        "select seq, a, b, commits, imported from units.coupled_pairs order by a, b",
        "(3,pkg/a.py,pkg/b.py,4,true) (3,pkg/a.py,pkg/c.py,4,false) (3,pkg/b.py,pkg/c.py,4,false)",
    );
    assert_rows(
        &r.dir,
        "select seq, round(value, 4), violations, pairs, import_coverage from metrics.modularity_violations",
        "(3,0.6667,2,3,1.0)",
    );
}

/// Experimental clones: a function copied with other names is duplicated code.
#[test]
fn experimental_clones() {
    let body = |f: &str, v: &str| {
        format!(
            "fn {f}(items: &[i32], {v}: i32) -> i32 {{\n    let mut total = 0;\n    for item in items {{\n        if *item > {v} {{ total += item * 2; }} else {{ total -= 1; }}\n        if *item == {v} {{ return total; }}\n    }}\n    total + {v} * 3\n}}\n"
        )
    };
    let mut r = common::Repo::new();
    r.write("a.rs", body("sum", "limit"));
    r.commit("main", "c0\n", 0, &[]);
    r.write("b.rs", body("total_over", "bound"));
    r.extend("copy\n", 1);
    scan(&r.dir, "main");
    // Read at the tip (both commits fall in one week). Winnowed fingerprints cover most,
    // not all, of a repeated run.
    assert_rows(
        &r.dir,
        "select seq, value > 0.5, value <= 1, duplicated_tokens > 0 from experimental.clones order by seq",
        "(1,true,true,true)",
    );
    let out = succeeds(&r.dir, &["experimental", "-", "clones"]);
    assert!(out.starts_with(r#"{"clones":[{"seq":1,"#), "{out}");
}

/// Experimental structure and documentation metrics on one hand-checked file.
#[test]
fn experimental_snapshot_metrics() {
    let mut r = common::Repo::new();
    r.write(
        "a.rs",
        "/// Reads the input.\npub fn read(path: &str) -> String {\n    load(path)\n}\n\n\
         pub fn undocumented(x: i32) -> i32 {\n    helper(x) + 1\n}\n\n\
         fn helper(x: i32) -> i32 {\n    // TODO: faster\n    x * 2\n}\n\n\
         fn load(p: &str) -> String {\n    p.to_owned()\n}\n",
    );
    r.commit("main", "c0\n", 0, &[]);
    scan(&r.dir, "main");
    let one = |view: &str, columns: &str| {
        rows(
            &r.dir,
            &format!("select {columns} from experimental.{view}"),
        )
    };
    // read forwards to load.
    assert_eq!(one("pass_through", "value, count, functions"), "(0.25,1,4)");
    // helper and load are private, uniquely named and called once each.
    assert_eq!(
        one("single_use", "value, count, private_functions"),
        "(1.0,2,2)"
    );
    // One of the two public functions has a doc comment.
    assert_eq!(
        one("doc_completeness", "value, documented, public_functions"),
        "(0.5,1,2)"
    );
    assert_eq!(one("markers", "count, files_with_markers"), "(1,1)");
}
