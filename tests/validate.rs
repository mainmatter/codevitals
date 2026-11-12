//! `cv validate` on labels written to match a metric by construction.

mod common;

use common::{Repo, rows, scan, succeeds};

/// Functions with 0 to 7 decision points and token counts that do not follow them.
fn repo() -> Repo {
    let mut r = Repo::new();
    let mut src = String::new();
    for i in 0..16 {
        let ifs: String = (0..i % 8)
            .map(|k| format!("    if x == {k} {{ return {k}; }}\n"))
            .collect();
        let pad: String = (0..(i * 5) % 7)
            .map(|k| format!("    let _p{k} = x;\n"))
            .collect();
        src += &format!("pub fn f{i}(x: i32) -> i32 {{\n{ifs}{pad}    x\n}}\n\n");
    }
    r.write("src/lib.rs", &src);
    r.commit("main", "c0\n", 0, &[]);
    scan(&r.dir, "main");
    r
}

#[test]
fn a_metric_raters_follow_is_supported() {
    let r = repo();
    let functions = rows(
        &r.dir,
        "select blob, start_byte, end_byte, decisions from function_facts order by start_byte",
    );
    let f: Vec<Vec<String>> = functions
        .split(") (")
        .map(|s| {
            s.trim_matches(['(', ')'])
                .split(',')
                .map(String::from)
                .collect()
        })
        .collect();
    let mut lines = String::new();
    let n = |rater: &str,
             a: &[String],
             b: &[String],
             choice: &str,
             repeat: Option<usize>,
             k: &mut usize| {
        let line = format!(
            r#"{{"rater":"{rater}","n":{k},"time":"2026-01-01T00:00:00Z","a_blob":"{}","a_start":{},"a_end":{},"a_path":"src/lib.rs","b_blob":"{}","b_start":{},"b_end":{},"b_path":"src/lib.rs","choice":"{choice}","ms":1000,"a_commits":1,"b_commits":0,"repeat_of":{}}}"#,
            a[0],
            a[1],
            a[2],
            b[0],
            b[1],
            b[2],
            repeat.map_or("null".into(), |r| r.to_string())
        );
        *k += 1;
        line + "\n"
    };
    // Both raters choose (would rather change) the function with fewer decisions.
    for rater in ["ann", "bob"] {
        let mut k = 0;
        for i in 0..f.len() {
            for j in i + 1..f.len() {
                let (di, dj): (i32, i32) = (f[i][3].parse().unwrap(), f[j][3].parse().unwrap());
                let choice = if di < dj {
                    "a"
                } else if dj < di {
                    "b"
                } else {
                    "same"
                };
                lines += &n(rater, &f[i], &f[j], choice, None, &mut k);
            }
        }
        // One repeat, sides swapped, answered consistently.
        lines += &n(rater, &f[1], &f[0], "b", Some(0), &mut k);
    }
    std::fs::write(r.dir.path().join("labels.jsonl"), lines).unwrap();
    let labels = r.dir.path().join("labels.jsonl");
    let json = succeeds(
        &r.dir,
        &[
            "validate",
            "--format",
            "json",
            "--labels",
            labels.to_str().unwrap(),
        ],
    );
    let metric = |name: &str| {
        let m = &json[json.find(&format!(r#"{{"metric":"{name}""#)).unwrap()..];
        m[..m.find('}').unwrap()].to_owned()
    };
    assert!(
        metric("decisions").contains(r#""share":1,"low":1,"high":1,"#),
        "{json}"
    );
    assert!(json.contains(r#""self_agreement":[{"rater":"ann","agree":1,"of":1},{"rater":"bob","agree":1,"of":1}]"#), "{json}");
    assert!(
        json.contains(r#""between_raters":{"agree":120,"of":120},"kappa":1}"#),
        "{json}"
    );
    let text = succeeds(&r.dir, &["validate", "--labels", labels.to_str().unwrap()]);
    assert!(text.contains("tokens"), "{text}");
    let decisions = text.lines().find(|l| l.starts_with("decisions")).unwrap();
    // Supported only if it also beats tokens: depends on how tokens follow decisions here.
    let tokens_share: f64 = {
        let t = metric("tokens");
        let s = &t[t.find(r#""share":"#).unwrap() + 8..];
        s[..s.find(',').unwrap()].parse().unwrap()
    };
    assert_eq!(decisions.ends_with("yes"), tokens_share < 0.9, "{text}");
}

/// The defect check: at a cut, files ranked by each metric, and the share of the
/// next window's fixes in the files that hold the first 20% of tokens.
#[test]
fn defects_after_a_cut() {
    let mut r = Repo::new();
    let big: String = (0..4)
        .map(|i| format!("fn b{i}(x: i32) -> i32 {{ x + {i} }}\n"))
        .collect();
    r.write(
        "a.rs",
        "fn a(x: i32) -> i32 { if x > 0 { 1 } else { 2 } }\n",
    )
    .write("b.rs", &big);
    r.commit("main", "start\n", 0, &[]);
    r.write(
        "a.rs",
        "fn a(x: i32) -> i32 { if x > 1 { 1 } else { 2 } }\n",
    );
    r.extend("tune a\n", 100);
    r.write("b.rs", big.replace("x + 0", "x - 0"));
    r.extend("fix wrong result in b0\n", 200);
    r.write("c.rs", "fn c() {}\n");
    r.extend("add c\n", 400);
    scan(&r.dir, "main");
    // One cut, at day 182: a.rs changed once before it, b.rs is fixed after it.
    common::assert_rows(
        &r.dir,
        "select k, path, changes, fixes from units.defect_windows order by path",
        "(1,a.rs,1,0) (1,b.rs,0,1)",
    );
    let json = succeeds(
        &r.dir,
        &["validate", "--format", "json", "--labels", "none.jsonl"],
    );
    let found = |ranking: &str| {
        let m = &json[json.find(&format!(r#""ranking":"{ranking}""#)).unwrap()..];
        m[..m.find('}').unwrap()].to_owned()
    };
    // Ranked by changes, a.rs comes first and holds no fix; ranked by size, b.rs does.
    assert!(found("changes").contains(r#""found":0,"#), "{json}");
    assert!(found("tokens").contains(r#""found":1,"#), "{json}");
    assert!(json.contains("recall may be as low as 0.6"), "{json}");
    assert!(json.contains(r#""labels":{"count":0,"#), "{json}");
}

/// Functions without a parameter list (JS arrows: params is NULL) decide nothing for
/// params, and break nothing else.
#[test]
fn missing_values_decide_nothing() {
    let mut r = Repo::new();
    r.write(
        "a.js",
        "export const f = x => { if (x) { return 1; } return 2; };\nexport const g = y => y + 1;\n",
    );
    r.commit("main", "c0\n", 0, &[]);
    scan(&r.dir, "main");
    let f = rows(
        &r.dir,
        "select blob, start_byte, end_byte from function_facts order by start_byte",
    );
    let f: Vec<Vec<&str>> = f
        .split(") (")
        .map(|s| s.trim_matches(['(', ')']).split(',').collect())
        .collect();
    let line = format!(
        r#"{{"rater":"ann","n":0,"time":"2026-01-01T00:00:00Z","a_blob":"{}","a_start":{},"a_end":{},"a_path":"a.js","b_blob":"{}","b_start":{},"b_end":{},"b_path":"a.js","choice":"b","ms":1,"a_commits":0,"b_commits":0,"repeat_of":null}}"#,
        f[0][0], f[0][1], f[0][2], f[1][0], f[1][1], f[1][2]
    );
    let labels = r.dir.path().join("labels.jsonl");
    std::fs::write(&labels, line + "\n").unwrap();
    let json = succeeds(
        &r.dir,
        &[
            "validate",
            "--format",
            "json",
            "--labels",
            labels.to_str().unwrap(),
        ],
    );
    assert!(
        json.contains(r#"{"metric":"params","baseline":false,"pairs":0,"#),
        "{json}"
    );
    assert!(
        json.contains(r#"{"metric":"decisions","baseline":false,"pairs":1,"share":1,"#),
        "{json}"
    );
    let text = succeeds(&r.dir, &["validate", "--labels", labels.to_str().unwrap()]);
    assert!(
        text.contains("no pairs differ") && !text.contains("NaN"),
        "{text}"
    );
}
