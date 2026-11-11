//! The walk and the parse, on git repositories built in process.

mod common;

use std::path::Path;
use std::process::Command;

use common::{db, fixture, rev, rows, scan, tally};
use tempfile::TempDir;

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
    let r = fixture();
    scan(&r.dir, "main");
    dump(&r.dir)
}

/// The in-process fixture is the spec's: `fixture.sh`, run with real git, produces the
/// same commit ids. The only test that needs a `git` executable.
#[test]
fn fixture_matches_fixture_sh() {
    let dir = TempDir::new().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture.sh");
    let out = Command::new("sh")
        .arg(script)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let git = |rev: &str| {
        let out = Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(dir.path())
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    let r = fixture();
    assert_eq!(rev(&r.dir, "main"), git("main"));
    assert_eq!(rev(&r.dir, "side"), git("side"));
}

#[test]
fn fixture_tables() {
    let r = fixture();
    let dir = &r.dir;
    assert_eq!(
        scan(dir, "main"),
        "scanned 4 commits, 8 changes; parsed 4 blobs\n"
    );

    let ids = ["main~3", "main~2", "main~1", "main"].map(|spec| rev(dir, spec));
    let times = ["2026-01-01", "2026-01-02", "2026-01-03", "2026-01-05"];
    let commits: Vec<String> = (0..4)
        .map(|i| {
            let message = ["c0", "c1", "c2", "merge"][i];
            format!("({i},{},{} 00:00:00,t <t@t>,{message}\n)", ids[i], times[i])
        })
        .collect();
    assert_eq!(
        rows(dir, "select * from commits order by seq"),
        commits.join(" ")
    );

    let blob = |spec: &str| rev(dir, spec);
    let (readme, a1, a2) = (
        blob("main~3:README.md"),
        blob("main~3:a.rs"),
        blob("main~2:a.rs"),
    );
    let (plus, d) = (blob("main~2:src/b.rs"), blob("main:d.rs"));
    assert_eq!(
        rows(dir, "select * from changes order by seq, path"),
        format!(
            "(0,README.md,{readme}) (0,a.rs,{a1}) (1,a.rs,{a2}) (1,src/b.rs,{plus}) \
             (2,README.md,NULL) (2,src/b.rs,NULL) (2,src/c.rs,{plus}) (3,d.rs,{d})"
        )
    );
    assert_eq!(
        rows(
            dir,
            "select path, blob, since, until from files order by path, since"
        ),
        format!(
            "(README.md,{readme},0,2) (a.rs,{a1},0,1) (a.rs,{a2},1,2147483647) \
             (d.rs,{d},3,2147483647) (src/b.rs,{plus},1,2) (src/c.rs,{plus},2,2147483647)"
        )
    );
    assert_eq!(
        rows(
            dir,
            "select kind, count(*), count(distinct blob) from spans group by kind order by kind"
        ),
        "(block,4,4) (file,4,4) (fn,4,4)"
    );
    assert_eq!(
        rows(
            dir,
            &format!(
                "select blob, name, tokens, comments from spans
                 where kind = 'fn' and blob in ('{a1}', '{a2}') order by comments"
            )
        ),
        format!("({a1},add,18,0) ({a2},add,18,2)")
    );
}

#[test]
fn idempotent() {
    let r = fixture();
    scan(&r.dir, "main");
    let before = dump(&r.dir);
    assert_eq!(
        scan(&r.dir, "main"),
        "scanned 0 commits, 0 changes; parsed 0 blobs\n"
    );
    assert_eq!(dump(&r.dir), before);
}

#[test]
fn incremental() {
    let r = fixture();
    assert_eq!(
        scan(&r.dir, "main~2"),
        "scanned 2 commits, 4 changes; parsed 3 blobs\n"
    );
    assert_eq!(
        scan(&r.dir, "main"),
        "scanned 2 commits, 4 changes; parsed 1 blobs\n"
    );
    assert_eq!(dump(&r.dir), fresh_dump());
}

#[test]
fn version_bump() {
    let r = fixture();
    scan(&r.dir, "main");
    let before = dump(&r.dir);
    common::execute(&r.dir, "update spans set v = 0");
    assert_eq!(
        scan(&r.dir, "main"),
        "scanned 0 commits, 0 changes; parsed 4 blobs\n"
    );
    assert_eq!(dump(&r.dir), before);
}

#[test]
fn diverged() {
    let r = fixture();
    scan(&r.dir, "main");
    let before = dump(&r.dir);
    let out = tally(&r.dir, &["scan", "side"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("history diverged from the stored chain; reset the walk with: duckdb <db> \"delete from commits; delete from changes\""),
        "{stderr}"
    );
    assert_eq!(dump(&r.dir), before);
}

#[test]
fn oversized() {
    let mut r = fixture();
    r.write("big.rs", "// x\n".repeat(300_000));
    r.extend("big\n", 5);
    scan(&r.dir, "main");
    assert_eq!(
        rows(
            &r.dir,
            &format!(
                "select * exclude (blob) from spans where blob = '{}'",
                rev(&r.dir, "main:big.rs")
            )
        ),
        "(6,rust,file,NULL,1,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL)"
    );
}

#[test]
fn old_database_is_refused() {
    let r = fixture();
    common::execute(&r.dir, "create table spans (blob text)");
    let out = tally(&r.dir, &["scan", "main"]);
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        !out.status.success() && err.contains("delete it and scan again"),
        "{err}"
    );
    assert!(db(&r.dir).exists());
}

/// A path's module is its nearest manifest directory, ignoring a manifest at the
/// repository root, else the module depth; its role follows path conventions.
#[test]
fn modules_follow_manifests() {
    let mut r = fixture();
    for manifest in ["Cargo.toml", "crates/x/Cargo.toml", "docs/a.md"] {
        r.write(manifest, "");
    }
    for code in [
        "crates/x/src/lib.rs",
        "crates/x/tests/t.rs",
        "deep/a/b/c.rs",
    ] {
        r.write(code, "fn f() {}\n");
    }
    r.write("vendor/v.c", "int v(void) { return 0; }\n");
    r.write("web/app.spec.ts", "f();\n")
        .write("py/test_x.py", "f()\n");
    r.extend("crates\n", 5);
    scan(&r.dir, "main");
    assert_eq!(
        rows(
            &r.dir,
            "select path, module, role from path_modules join path_roles using (path) order by path"
        ),
        "(Cargo.toml,,other) (README.md,,other) (a.rs,,code) (crates/x/Cargo.toml,crates/x,other) \
         (crates/x/src/lib.rs,crates/x,code) (crates/x/tests/t.rs,crates/x,test) (d.rs,,code) \
         (deep/a/b/c.rs,deep/a,code) (docs/a.md,docs,other) (py/test_x.py,py,test) (src/b.rs,src,code) \
         (src/c.rs,src,code) (vendor/v.c,vendor,vendored) (web/app.spec.ts,web,test)"
    );
}

/// Squashing history keeps the state metrics of the final tree: the same tree in one
/// commit has the same size, encapsulation, duplication and concentration.
#[test]
fn squashing_keeps_state_metrics_of_the_final_tree() {
    let full = fixture();
    scan(&full.dir, "main");
    let mut flat = fixture();
    flat.commit("flat", "flat\n", 4, &[]);
    scan(&flat.dir, "flat");
    for m in [
        "m_size",
        "m_encapsulation",
        "m_duplication",
        "m_concentration",
    ] {
        let latest = format!("select * exclude (seq) from {m} order by seq desc limit 1");
        assert_eq!(rows(&flat.dir, &latest), rows(&full.dir, &latest), "{m}");
    }
}

/// Imports resolve by each language's conventions, and resolution follows the files
/// that exist: an include of a missing header resolves once the header is added.
#[test]
fn imports_resolve_by_language_convention() {
    let mut r = fixture();
    let files = [
        ("Cargo.toml", ""),
        ("crates/core-lib/Cargo.toml", ""),
        (
            "crates/core-lib/src/lib.rs",
            "pub mod a;\nmod tests {\n    use super::a::X;\n}\n",
        ),
        (
            "crates/core-lib/src/a.rs",
            "use crate::b::{Y, c::Z};\nuse std::io;\n",
        ),
        ("crates/core-lib/src/b/mod.rs", "use self::c::Z;\n"),
        ("crates/core-lib/src/b/c.rs", "use super::super::a;\n"),
        ("crates/app/Cargo.toml", ""),
        (
            "crates/app/src/main.rs",
            "use core_lib::a::X;\nuse app::run;\nuse serde::Serialize;\n",
        ),
        ("crates/app/src/lib.rs", "pub fn run() {}\n"),
        (
            "src/x.c",
            "#include \"x.h\"\n#include \"util/u.h\"\n#include <stdio.h>\n#include \"missing.h\"\n#include \"v.h\"\n#include \"submodule/s.h\"\n",
        ),
        ("src/x.h", "int x(void);\n"),
        ("src/util/u.h", "int u(void);\n"),
        ("lib/util/u.h", "int u2(void);\n"),
        ("vendor/v.h", "int v(void);\n"),
        ("py/pkg/__init__.py", "from . import mod\n"),
        (
            "py/pkg/mod.py",
            "import os\nfrom pkg.sub import thing\nfrom .. import nothing\n",
        ),
        ("py/pkg/sub.py", "thing = 1\n"),
        (
            "web/a.ts",
            "import { b } from './b';\nimport c from './c.js';\nimport d from './d';\nimport React from 'react';\n",
        ),
        ("web/b.ts", "export const b = 1;\n"),
        ("web/c.ts", "export default 2;\n"),
        ("web/d/index.ts", "export default 3;\n"),
    ];
    for (path, content) in files {
        r.write(path, content);
    }
    r.extend("imports\n", 5);
    r.write("src/missing.h", "int m(void);\n");
    r.extend("add the missing header\n", 6);
    scan(&r.dir, "main");
    let at = |seq: i32| {
        rows(
            &r.dir,
            &format!(
                "select path, name, class, target from import_status
                 where since <= {seq} and {seq} < until order by path, name"
            ),
        )
        .replace(") (", ")\n(")
    };
    assert_eq!(
        at(4),
        "(crates/app/src/main.rs,app::run,resolved,crates/app/src/lib.rs)
(crates/app/src/main.rs,core_lib::a::X,resolved,crates/core-lib/src/a.rs)
(crates/app/src/main.rs,serde::Serialize,external,NULL)
(crates/core-lib/src/a.rs,crate::b::Y,resolved,crates/core-lib/src/b/mod.rs)
(crates/core-lib/src/a.rs,crate::b::c::Z,resolved,crates/core-lib/src/b/c.rs)
(crates/core-lib/src/a.rs,std::io,external,NULL)
(crates/core-lib/src/b/c.rs,super::super::a,resolved,crates/core-lib/src/a.rs)
(crates/core-lib/src/b/mod.rs,self::c::Z,resolved,crates/core-lib/src/b/c.rs)
(crates/core-lib/src/lib.rs,super::a::X,resolved,crates/core-lib/src/a.rs)
(py/pkg/__init__.py,.mod,resolved,py/pkg/mod.py)
(py/pkg/mod.py,..nothing,unresolved,NULL)
(py/pkg/mod.py,os,external,NULL)
(py/pkg/mod.py,pkg.sub.thing,resolved,py/pkg/sub.py)
(src/x.c,\"missing.h\",unresolved,NULL)
(src/x.c,\"submodule/s.h\",external,NULL)
(src/x.c,\"util/u.h\",resolved,src/util/u.h)
(src/x.c,\"v.h\",external,NULL)
(src/x.c,\"x.h\",resolved,src/x.h)
(src/x.c,<stdio.h>,external,NULL)
(web/a.ts,./b,resolved,web/b.ts)
(web/a.ts,./c.js,resolved,web/c.ts)
(web/a.ts,./d,resolved,web/d/index.ts)
(web/a.ts,react,external,NULL)"
    );
    assert!(
        at(5).contains("(src/x.c,\"missing.h\",resolved,src/missing.h)"),
        "{}",
        at(5)
    );
    // Only app::run names more than a module file has: it falls back to the crate root.
    assert_eq!(
        rows(
            &r.dir,
            "select name from import_status where fallback and since <= 4 and 4 < until"
        ),
        "(app::run)"
    );
    assert_eq!(
        rows(
            &r.dir,
            "select source, target from import_edges where since <= 5 and 5 < until
             and source in ('src/x.c', 'web/a.ts') order by all"
        ),
        "(src/x.c,src/missing.h) (src/x.c,src/util/u.h) (src/x.c,src/x.h) (web/a.ts,web/b.ts) \
         (web/a.ts,web/c.ts) (web/a.ts,web/d/index.ts)"
    );
}
