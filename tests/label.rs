//! `cv label`, driven by keys on stdin as when no terminal is attached.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use common::{Repo, db, rows, scan};

fn label(r: &Repo, keys: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cv"))
        .env("RUST_LOG", "off")
        .args(["label", "--repo"])
        .arg(r.dir.path().join("repo"))
        .arg("--labels")
        .arg(r.dir.path().join("labels.jsonl"))
        .arg("--db")
        .arg(db(&r.dir))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(keys.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{err}");
    err
}

#[test]
fn judgments_append_and_resume() {
    let mut r = Repo::new();
    let mut src = String::new();
    for i in 0..12 {
        src += &format!(
            "pub fn f{i}(x: i32, y: i32) -> i32 {{\n    let z = x * {i} + y;\n    if z > {i} {{ z - 1 }} else {{ z + y * 2 }}\n}}\n\n"
        );
    }
    r.write("src/lib.rs", &src);
    r.commit("main", "c0\n", 0, &[]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(r.dir.path().join("repo/.git/config"))
        .unwrap()
        .write_all(b"[user]\n\temail = T@t\n")
        .unwrap();
    scan(&r.dir, "main");
    assert_eq!(
        label(&r, "hl\nsq"),
        "3 judgments added to ".to_owned()
            + r.dir.path().join("labels.jsonl").to_str().unwrap()
            + "\n"
    );
    assert!(label(&r, "a").starts_with("1 judgments added"));
    let file = r.dir.path().join("labels.jsonl");
    let loaded = format!(
        "select rater, n, choice, a_path, a_commits, b_commits, a_start <> b_start, ms >= 0
         from read_json('{}', format = 'newline_delimited') order by n",
        file.display()
    );
    assert_eq!(
        rows(&r.dir, &loaded),
        "(t@t,0,a,src/lib.rs,1,1,true,true) (t@t,1,b,src/lib.rs,1,1,true,true) \
         (t@t,2,same,src/lib.rs,1,1,true,true) (t@t,3,a,src/lib.rs,1,1,true,true)"
    );
}
