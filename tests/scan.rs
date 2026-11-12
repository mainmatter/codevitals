//! The walk and the parse, on git repositories built in process.

mod common;

use common::{assert_rows, db, fails, fixture, rev, rows, scan};
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
    assert_rows(
        dir,
        "select seq, path, blob, source from changes order by seq, path",
        format!(
            "(0,README.md,{readme},NULL) (0,a.rs,{a1},NULL) (1,a.rs,{a2},NULL) (1,src/b.rs,{plus},NULL) \
             (2,README.md,NULL,NULL) (2,src/b.rs,NULL,NULL) (2,src/c.rs,{plus},src/b.rs) (3,d.rs,{d},NULL)"
        ),
    );
    // src/c.rs continues the lineage of src/b.rs, which it was renamed from.
    assert_rows(
        dir,
        "select distinct path, lineage from lineages order by path",
        "(README.md,0:README.md) (a.rs,0:a.rs) (d.rs,3:d.rs) (src/b.rs,1:src/b.rs) (src/c.rs,1:src/b.rs)",
    );
    assert_rows(
        dir,
        "select path, blob, since, until from files order by path, since",
        format!(
            "(README.md,{readme},0,2) (a.rs,{a1},0,1) (a.rs,{a2},1,2147483647) \
             (d.rs,{d},3,2147483647) (src/b.rs,{plus},1,2) (src/c.rs,{plus},2,2147483647)"
        ),
    );
    assert_rows(
        dir,
        "select kind, count(*), count(distinct blob) from spans group by kind order by kind",
        "(block,4,4) (file,4,4) (fn,4,4)",
    );
    assert_rows(
        dir,
        &format!(
            "select blob, name, tokens, comments from spans
                 where kind = 'fn' and blob in ('{a1}', '{a2}') order by comments"
        ),
        format!("({a1},add,18,0) ({a2},add,18,2)"),
    );
}

/// Renames, also with edits, continue a file's lineage through any number of moves;
/// a path deleted and added again starts a new one; an unrelated file added where
/// another was deleted is no rename.
#[test]
fn renames_continue_lineages() {
    let body = |n: usize| {
        (0..n)
            .map(|i| format!("fn f{i}(x: i32) -> i32 {{ x + {i} }}\n"))
            .collect::<String>()
    };
    let mut r = common::Repo::new();
    r.write("a.rs", body(10)).write("z.rs", body(3));
    r.commit("main", "c0\n", 0, &[]);
    r.remove("a.rs").write("src/a.rs", body(10));
    r.extend("move\n", 1);
    r.remove("src/a.rs").write("lib/b.rs", body(11)); // renamed and edited
    r.extend("move and edit\n", 2);
    r.remove("z.rs");
    r.extend("delete\n", 3);
    r.write("z.rs", "struct Unrelated;\n");
    r.extend("re-add\n", 4);
    scan(&r.dir, "main");
    assert_rows(
        &r.dir,
        "select seq, path, source, lineage from lineages order by seq, path",
        "(0,a.rs,NULL,0:a.rs) (0,z.rs,NULL,0:z.rs) (1,a.rs,NULL,0:a.rs) (1,src/a.rs,a.rs,0:a.rs) \
         (2,lib/b.rs,src/a.rs,0:a.rs) (2,src/a.rs,NULL,0:a.rs) (3,z.rs,NULL,0:z.rs) (4,z.rs,NULL,4:z.rs)",
    );
    // The deletions a rename implies are no changes of their own.
    assert_rows(
        &r.dir,
        "select seq, path from code_changes order by seq, path",
        "(0,a.rs) (0,z.rs) (1,src/a.rs) (2,lib/b.rs) (3,z.rs) (4,z.rs)",
    );
}

/// Rescanning changes nothing; scanning in two steps equals one scan; a new
/// extractor version reparses every blob to the same spans.
#[test]
fn rescans() {
    let r = fixture();
    scan(&r.dir, "main");
    let before = dump(&r.dir);
    assert_eq!(
        scan(&r.dir, "main"),
        "scanned 0 commits, 0 changes; parsed 0 blobs\n"
    );
    assert_eq!(dump(&r.dir), before);
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

/// A rewritten history or a shallow clone fails, saying how to recover, and leaves
/// the database as it was.
#[test]
fn history_errors_say_what_to_do() {
    let r = fixture();
    scan(&r.dir, "main");
    let before = dump(&r.dir);
    let err = fails(&r.dir, &["scan", "--rev", "side"]);
    assert!(
        err.contains("\"side\" does not extend the history stored in")
            && err.contains("\"delete from commits; delete from changes\""),
        "{err}"
    );
    assert_eq!(dump(&r.dir), before);
    let mut r = fixture();
    let main = rev(&r.dir, "main");
    r.make_shallow(main.parse().unwrap());
    let err = fails(&r.dir, &["scan", "--rev", "main"]);
    assert!(
        err.contains("shallow clone") && err.contains("fetch-depth: 0"),
        "{err}"
    );
}

/// Content that is not source code gets a `file` row with NULL counts: blobs over
/// 1 MiB, binary files, Git LFS pointers and minified code. An empty file is parsed in
/// the language of its first path with a known extension.
#[test]
fn only_source_code_is_parsed() {
    let mut r = fixture();
    r.write("big.rs", "// x\n".repeat(300_000));
    r.extend("big\n", 5);
    scan(&r.dir, "main");
    assert_rows(
        &r.dir,
        &format!(
            "select * exclude (blob, v) from spans where blob = '{}'",
            rev(&r.dir, "main:big.rs")
        ),
        "(rust,file,NULL,1,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL)",
    );
    let mut r = fixture();
    r.write("bin.rs", b"fn\0\x01\x02")
        .write(
            "lfs.rs",
            "version https://git-lfs.github.com/spec/v1\noid sha256:00\nsize 1\n",
        )
        .write("app.min.js", format!("var a=1;{}\n", "a+=1;".repeat(4000)))
        .write(".empty.txt", "")
        .write("pkg/__init__.py", "");
    r.extend("content\n", 5);
    scan(&r.dir, "main");
    assert_rows(
        &r.dir,
            "select c.path, s.lang, s.tokens from changes c join spans s on s.blob = c.blob and s.kind = 'file'
             where c.seq = 4 order by c.path",
        "(.empty.txt,python,0) (app.min.js,javascript,NULL) (bin.rs,rust,NULL) (lfs.rs,rust,NULL) (pkg/__init__.py,python,0)",
    );
}

/// A database from an older layout, or parsed by a newer codevitals, is refused.
#[test]
fn other_versions_are_refused() {
    let r = fixture();
    scan(&r.dir, "main");
    common::execute(&r.dir, "update codevitals set schema = schema - 1");
    let err = fails(&r.dir, &["scan", "--rev", "main"]);
    assert!(err.contains("delete it and scan again"), "{err}");
    assert!(db(&r.dir).exists());
    let r = fixture();
    scan(&r.dir, "main");
    common::execute(&r.dir, "update codevitals set extractor = extractor + 1");
    let err = fails(&r.dir, &["scan", "--rev", "main"]);
    assert!(err.contains("parsed by a newer codevitals"), "{err}");
}

/// A path's role: what .gitattributes declare wins (Linguist's attributes and
/// `codevitals=<role>`, with an attribute turned off meaning first-party code), then path
/// conventions whatever their case, generation markers and minified JavaScript.
#[test]
fn roles_follow_attributes_then_conventions() {
    let mut r = fixture();
    r.write(
        ".gitattributes",
        "integration/** codevitals=test\nthird/** linguist-vendored\nsrc/gen.rs linguist-generated\n",
    )
    .write("third/ours/.gitattributes", "** -linguist-vendored\n");
    let code = "fn f() {}\n";
    for path in [
        "integration/t.rs",
        "third/lib.rs",
        "third/ours/own.rs",
        "src/gen.rs",
        "vendor/v.rs",
        "ThirdParty/t.rs",
        "examples/demo.rs",
        "docs/a.md",
        "crates/x/tests/t.rs",
        "flow-tests/f.rs",
        "Tests/x.rs",
        "fuzz/z.rs",
        "web/component-test.ts",
        "web/__mocks__/m.ts",
        "web/dist/bundle.js",
    ] {
        r.write(path, code);
    }
    r.write(
        "src/marked.rs",
        "// Code generated by hand-gen. DO NOT EDIT.\nfn a() {}\n",
    )
    .write("web/app.js", format!("{}\n", "var a = 1;".repeat(20)))
    .write("web/ok.js", "var a = 1;\n");
    r.extend("roles\n", 5);
    scan(&r.dir, "main");
    assert_eq!(
        rows(
            &r.dir,
            "select path, role from path_roles where path not in ('a.rs', 'd.rs', 'README.md')
               and not starts_with(path, 'src/b') and not starts_with(path, 'src/c') order by path"
        )
        .replace(") (", ")\n("),
        "(.gitattributes,other)
(Tests/x.rs,test)
(ThirdParty/t.rs,vendored)
(crates/x/tests/t.rs,test)
(docs/a.md,documentation)
(examples/demo.rs,documentation)
(flow-tests/f.rs,test)
(fuzz/z.rs,test)
(integration/t.rs,test)
(src/gen.rs,generated)
(src/marked.rs,generated)
(third/lib.rs,vendored)
(third/ours/.gitattributes,other)
(third/ours/own.rs,code)
(vendor/v.rs,vendored)
(web/__mocks__/m.ts,test)
(web/app.js,generated)
(web/component-test.ts,test)
(web/dist/bundle.js,generated)
(web/ok.js,code)"
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
        "metrics.size",
        "metrics.encapsulation",
        "metrics.duplication",
        "metrics.concentration",
    ] {
        let latest = format!("select * exclude (seq) from {m} order by seq desc limit 1");
        assert_eq!(rows(&flat.dir, &latest), rows(&full.dir, &latest), "{m}");
    }
}

/// Imports resolve by each language's conventions, and resolution follows the files
/// that exist: an include of a missing header resolves once the header is added.
/// Rust crates are found by src/lib.rs whatever the build system, and named by their
/// directory or Cargo.toml (also at the repository root); ECMAScript workspace
/// packages by their package.json name, and path aliases by tsconfig.json.
#[test]
fn imports_resolve_by_language_convention() {
    let mut r = fixture();
    let files = [
        ("Cargo.toml", "[package]\nname = \"root-app\"\n"),
        ("src/lib.rs", "pub fn thing() {}\n"),
        ("src/main.rs", "use root_app::thing;\n"),
        (
            "crates/renamed-dir/Cargo.toml",
            "[package]\nname = \"nice-name\"\n",
        ),
        ("crates/renamed-dir/src/lib.rs", "pub fn n() {}\n"),
        ("libs/buck-only/src/lib.rs", "pub fn b() {}\n"),
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
            "use core_lib::a::X;\nuse app::run;\nuse serde::Serialize;\nuse nice_name::n;\nuse buck_only::b;\n",
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
            "import { b } from './b';\nimport c from './c.js';\nimport d from './d';\nimport React from 'react';\n\
             import { u } from '@acme/ui';\nimport btn from '@acme/ui/button';\nconst r = require('./b');\n\
             import gone from '@acme/ui/gone';\nimport { cn } from '@/utils/cn';\nimport m from '@/missing';\n",
        ),
        ("web/b.ts", "export const b = 1;\n"),
        ("web/c.ts", "export default 2;\n"),
        ("web/d/index.ts", "export default 3;\n"),
        ("packages/ui/package.json", r#"{"name": "@acme/ui"}"#),
        (
            "web/tsconfig.json",
            r#"{"compilerOptions": {"baseUrl": ".", "paths": {"@/*": ["./src/*"]}}}"#,
        ),
        ("web/src/utils/cn.ts", "export const cn = 1;\n"),
        ("packages/ui/src/index.ts", "export const u = 1;\n"),
        ("packages/ui/src/button.ts", "export default 2;\n"),
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
(crates/app/src/main.rs,buck_only::b,resolved,libs/buck-only/src/lib.rs)
(crates/app/src/main.rs,core_lib::a::X,resolved,crates/core-lib/src/a.rs)
(crates/app/src/main.rs,nice_name::n,resolved,crates/renamed-dir/src/lib.rs)
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
(src/main.rs,root_app::thing,resolved,src/lib.rs)
(src/x.c,\"missing.h\",unresolved,NULL)
(src/x.c,\"submodule/s.h\",external,NULL)
(src/x.c,\"util/u.h\",resolved,src/util/u.h)
(src/x.c,\"v.h\",external,NULL)
(src/x.c,\"x.h\",resolved,src/x.h)
(src/x.c,<stdio.h>,external,NULL)
(web/a.ts,./b,resolved,web/b.ts)
(web/a.ts,./c.js,resolved,web/c.ts)
(web/a.ts,./d,resolved,web/d/index.ts)
(web/a.ts,@/missing,unresolved,NULL)
(web/a.ts,@/utils/cn,resolved,web/src/utils/cn.ts)
(web/a.ts,@acme/ui,resolved,packages/ui/src/index.ts)
(web/a.ts,@acme/ui/button,resolved,packages/ui/src/button.ts)
(web/a.ts,@acme/ui/gone,unresolved,NULL)
(web/a.ts,react,external,NULL)"
    );
    assert!(
        at(5).contains("(src/x.c,\"missing.h\",resolved,src/missing.h)"),
        "{}",
        at(5)
    );
    // Imports of an item of another crate name more than a module file has: they
    // fall back to the crate root.
    assert_rows(
        &r.dir,
        "select name from import_status where fallback and since <= 4 and 4 < until order by name",
        "(app::run) (buck_only::b) (nice_name::n) (root_app::thing)",
    );
    assert_rows(
        &r.dir,
        "select source, target from import_edges where since <= 5 and 5 < until
             and source in ('src/x.c', 'web/a.ts') order by all",
        "(src/x.c,src/missing.h) (src/x.c,src/util/u.h) (src/x.c,src/x.h) \
         (web/a.ts,packages/ui/src/button.ts) (web/a.ts,packages/ui/src/index.ts) (web/a.ts,web/b.ts) \
         (web/a.ts,web/c.ts) (web/a.ts,web/d/index.ts) (web/a.ts,web/src/utils/cn.ts)",
    );
}

/// Git paths need not be UTF-8: invalid bytes are written `\xNN`, so distinct paths
/// stay distinct.
#[test]
fn non_utf8_paths_stay_distinct() {
    let mut r = fixture();
    r.write(b"a\xff.rs", "fn x() {}\n")
        .write(b"a\xfe.rs", "fn y() {}\n");
    r.extend("bytes\n", 5);
    scan(&r.dir, "main");
    assert_rows(
        &r.dir,
        "select path from changes where seq = 4 order by path",
        "(a\\xfe.rs) (a\\xff.rs)",
    );
}
