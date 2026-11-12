//! `cv diff`: metric changes between two commits and the units behind them.

mod common;

use common::{Repo, scan, succeeds};

/// Runs `cv diff` on `dir/repo`, which must succeed, and returns its stdout.
fn diff(r: &Repo, args: &[&str]) -> String {
    let repo = r.dir.path().join("repo");
    let mut all = vec!["diff", "--repo", repo.to_str().unwrap()];
    all.extend(args);
    succeeds(&r.dir, &all)
}

fn history() -> Repo {
    let mut r = Repo::new();
    r.write(
        "a.rs",
        "fn f(x: bool) -> i32 {\n    if x { 1 } else { 2 }\n}\n",
    )
    .write("b.rs", "fn g() -> i32 {\n    3\n}\n");
    r.commit("main", "base\n", 0, &[]);
    r.write(
        "a.rs",
        "fn f(x: bool, y: bool) -> i32 {\n    if x { if y { 1 } else { 0 } } else { 2 }\n}\n",
    )
    .write(
        "c.rs",
        "fn h() {\n    let v: Option<i32> = None;\n    v.unwrap();\n}\n",
    );
    r.extend("head\n", 1);
    r
}

/// The change of each metric, and its contributions, which add up to it; the same
/// with and without a scanned database.
#[test]
fn contributions_add_up() {
    let r = history();
    let unscanned = diff(&r, &["main~1", "main", "--top", "100"]);
    scan(&r.dir, "main");
    let scanned = diff(&r, &["main~1", "main", "--top", "100"]);
    assert_eq!(scanned, unscanned);
    let json = scanned;
    // decisions per 100 tokens: f gains an if (and its else); a.rs and c.rs add tokens.
    let decisions = json.split(r#"{"metric":"decisions""#).nth(1).unwrap();
    let decisions = &decisions[..decisions.find(r#"{"metric":"#).unwrap()];
    assert!(
        decisions.starts_with(r#","validated":false,"#),
        "{decisions}"
    );
    assert!(
        decisions.contains(r#""function":"0:a.rs f","name":"f""#),
        "{decisions}"
    );
    // Contributions add up to the change, for functions and for files.
    for metric in json.split(r#"{"metric":"#).skip(1) {
        let number = |key: &str| -> Option<f64> {
            let rest = &metric[metric.find(&format!("\"{key}\":"))? + key.len() + 3..];
            rest[..rest.find([',', '}']).unwrap()].parse().ok()
        };
        let Some(change) = number("change") else {
            continue;
        };
        for list in ["functions", "files"] {
            let part = &metric[metric.find(&format!("\"{list}\":")).unwrap()..];
            let part = &part[..part.find(']').unwrap()];
            let sum: f64 = part
                .split("\"contribution\":")
                .skip(1)
                .map(|s| s[..s.find('}').unwrap()].parse::<f64>().unwrap())
                .sum();
            if metric.contains("\"additive\":true") {
                assert!(
                    (sum - change).abs() < 1e-9,
                    "{list} of {metric}: {sum} vs {change}"
                );
            }
        }
    }
}

/// The text form names the largest contributors and marks unvalidated metrics.
#[test]
fn text_form() {
    let r = history();
    let text = diff(&r, &["main~1", "main", "--format", "text"]);
    let lines: Vec<&str> = text.lines().collect();
    let decisions = lines
        .iter()
        .position(|l| l.starts_with("decisions"))
        .unwrap();
    assert!(lines[decisions].ends_with("unvalidated"), "{text}");
    // c.rs, which adds tokens without decisions, dilutes them more than f adds.
    assert!(lines[decisions + 1].ends_with("-34.9%  c.rs  h"), "{text}");
    assert!(lines[decisions + 2].ends_with("+27.0%  a.rs  f"), "{text}");
    let exceptions = lines
        .iter()
        .position(|l| l.starts_with("exceptions"))
        .unwrap();
    assert!(lines[exceptions + 1].ends_with("c.rs  h"), "{text}");
}

/// Edit thrash: lines one commit of the range adds and a later one deletes.
#[test]
fn edit_thrash() {
    let mut r = Repo::new();
    r.write("a.rs", "fn a() {\n    one();\n}\n");
    r.commit("main", "base\n", 0, &[]);
    r.write(
        "a.rs",
        "fn a() {\n    one();\n    two();\n    three();\n}\n",
    );
    r.extend("add two lines\n", 1);
    r.write("a.rs", "fn a() {\n    one();\n    three();\n}\n");
    r.extend("drop one again\n", 2);
    r.remove("a.rs").write(
        "b.rs",
        "fn a() {\n    one();\n    three();\n    four();\n}\n",
    );
    r.extend("move and add\n", 3);
    let json = diff(&r, &["main~3", "main"]);
    assert!(
        json.ends_with("\"experimental\":{\"edit_thrash\":{\"commits\":3,\"added\":3,\"thrashed\":1,\"share\":0.3333333333333333,\"validated\":false}}}\n"),
        "{json}"
    );
    // Not on the first-parent chain: no thrash.
    let json = diff(&r, &["main", "main~3"]);
    assert!(
        json.ends_with("\"experimental\":{\"edit_thrash\":null}}\n"),
        "{json}"
    );
}

/// Thrash follows a file across a rename: a line added before the rename and
/// deleted after it is thrash, whichever of the two paths sorts first.
#[test]
fn edit_thrash_follows_renames() {
    let mut r = Repo::new();
    let body = "fn a() {\n    one();\n    two();\n    three();\n    four();\n    five();\n}\n";
    r.write("a.rs", body);
    r.commit("main", "base\n", 0, &[]);
    r.write("a.rs", body.replace("three();", "three();\n    added();"));
    r.extend("add a line\n", 1);
    r.remove("a.rs")
        .write("b.rs", body.replace("three();", "three();\n    added();"));
    r.extend("rename\n", 2);
    r.write("b.rs", body);
    r.extend("drop it again\n", 3);
    let json = diff(&r, &["main~3", "main"]);
    assert!(
        json.contains("\"edit_thrash\":{\"commits\":3,\"added\":1,\"thrashed\":1,"),
        "{json}"
    );
}
