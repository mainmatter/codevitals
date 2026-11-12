#[path = "common/samples.rs"]
mod samples;

use codevitals::{Incremental, LANGS, Lang, Span, edits, extract};

/// Kinds of control-flow structure, checked by `structure` instead of `check`.
const STRUCTURE: [&str; 9] = [
    "branch",
    "condition",
    "block",
    "global_state",
    "throw",
    "goto",
    "invariant",
    "unhandled",
    "unfinished",
];

/// Kinds checked by their own tests: clones, function extras, markers.
const EXTRAS: [&str; 5] = ["fingerprint", "forward", "doc", "vocabulary", "marker"];

/// One line per span: kind name line end_line tokens comments params errors.
fn render(spans: &[Span]) -> String {
    let opt = |v: Option<i32>| v.map_or("NULL".into(), |v| v.to_string());
    spans
        .iter()
        .filter(|s| !STRUCTURE.contains(&s.kind) && !EXTRAS.contains(&s.kind))
        .map(|s| {
            let name = s.name.as_deref().unwrap_or("NULL");
            let (p, e) = (opt(s.params), opt(s.errors));
            let (k, l, el, t, c) = (s.kind, s.line, s.end_line, s.tokens, s.comments);
            format!("{k} {name} {l} {el} {t} {c} {p} {e}\n")
        })
        .collect()
}

/// The spans of `src` parsed as the language of `path`, checked against `expected`.
fn check(path: &str, src: &str, expected: &str) -> Vec<Span> {
    let spans = extract(Lang::of(path).expect("known extension"), src.as_bytes());
    assert_eq!(render(&spans), expected, "{path} spans for:\n{src}");
    spans
}

/// The spans of `src`, parsed as the language of `path`, of the kinds in `keep`,
/// each as `show` gives it.
fn kinds<T>(path: &str, src: &str, keep: &[&str], show: impl Fn(Span) -> T) -> Vec<T> {
    extract(Lang::of(path).expect("known extension"), src.as_bytes())
        .into_iter()
        .filter(|s| keep.contains(&s.kind))
        .map(show)
        .collect()
}

/// The control-flow structure of `src`: one line per structural span, as
/// `kind line depth tokens`, checked against `expected`.
fn structure(path: &str, src: &str, expected: &str) {
    let lines = kinds(path, src, &STRUCTURE, |s| {
        format!("{} {} {} {}\n", s.kind, s.line, s.depth, s.tokens)
    });
    assert_eq!(lines.concat(), expected, "{path} structure for:\n{src}");
}

#[test]
fn worked_example() {
    let src = r#"use std::collections::HashMap;

/// Adds two numbers.
fn add(a: i32, b: i32) -> i32 {
    // sum them
    a + b
}

impl Foo {
    fn get(&self) -> i32 { self.x }
}

fn plus(x: i32, y: i32) -> i32 {
    x + y
}
"#;
    let s = check(
        "x.rs",
        src,
        "file NULL 1 15 60 7 NULL 0
import std::collections::HashMap 1 1 7 0 NULL NULL
fn add 4 7 18 3 2 NULL
scope Foo 9 11 17 0 NULL NULL
fn get 10 10 13 0 1 NULL
fn plus 13 15 18 0 2 NULL
",
    );
    assert_eq!((s[0].start_byte, s[0].end_byte), (0, src.len() as i32));
    let add = s.iter().find(|s| s.name.as_deref() == Some("add")).unwrap();
    let add = &src[add.start_byte as usize..add.end_byte as usize];
    assert!(add.starts_with("fn add") && add.ends_with('}'), "{add}");
    let named = |n: &str| s.iter().find(|s| s.name.as_deref() == Some(n)).unwrap();
    assert_eq!(named("add").hash, named("plus").hash);
    assert_ne!(named("get").hash, named("add").hash);
}

/// Rust: functions, calls, imports, comments, visibility and control-flow structure.
#[test]
fn rust() {
    // Calls through paths, methods and turbofish; `pub` markers.
    check(
        "x.rs",
        "pub fn a() { b(); x::c(); self.d(); e::<u8>(); s.f::<T>(); }
fn b() {}
pub(crate) fn g() {}
",
        r#"file NULL 1 3 57 0 NULL 0
fn a 1 1 41 0 0 NULL
pub NULL 1 1 41 0 NULL NULL
call b 1 1 3 0 NULL NULL
call c 1 1 5 0 NULL NULL
call d 1 1 5 0 NULL NULL
call e 1 1 7 0 NULL NULL
call f 1 1 9 0 NULL NULL
fn b 2 2 6 0 0 NULL
fn g 3 3 10 0 0 NULL
pub NULL 3 3 10 0 NULL NULL
"#,
    );
    structure(
        "x.rs",
        samples::RUST_STRUCTURE,
        r#"global_state 1 0 8
block 2 0 77
branch 3 0 61
condition 3 0 13
throw 3 0 5
unhandled 3 0 5
block 3 1 28
branch 4 1 26
block 4 2 20
branch 5 2 18
condition 5 0 7
block 5 3 10
throw 5 0 7
branch 7 1 18
block 7 1 12
throw 8 0 8
unhandled 8 0 8
branch 10 0 7
branch 10 0 3
"#,
    );
}

/// C: the same facts, with `static` as the visibility marker and `goto`.
#[test]
fn c() {
    check(
        "x.c",
        "#include <stdio.h>

/* make a buffer */
char *buf(int n, int m) {
    return 0;
}

int sum(int a, int b) { return a + b; }
",
        r#"file NULL 1 8 33 5 NULL 0
import <stdio.h> 1 1 2 0 NULL NULL
fn buf 4 6 15 0 2 NULL
fn sum 8 8 16 0 2 NULL
"#,
    );
    // Calls; `static` through the private `_s` capture, which itself creates no span.
    check(
        "x.c",
        "static int helper(int x) { return x; }
int api(int y) { return helper(y) + g(y); }
extern int h(void);
",
        r#"file NULL 1 3 38 0 NULL 0
fn helper 1 1 12 0 1 NULL
static NULL 1 1 12 0 NULL NULL
fn api 2 2 19 0 1 NULL
call helper 2 2 4 0 NULL NULL
call g 2 2 4 0 NULL NULL
"#,
    );
    structure(
        "x.c",
        samples::C_STRUCTURE,
        r#"global_state 1 0 5
global_state 2 0 7
block 5 0 72
branch 6 0 44
condition 6 0 7
block 6 1 18
branch 7 1 16
block 7 2 11
branch 7 2 9
goto 7 0 3
branch 8 1 15
block 8 1 9
throw 8 0 6
block 9 1 17
branch 9 0 6
branch 9 0 9
branch 9 1 5
"#,
    );
}

/// C++: classes and namespaces, with C's markers.
#[test]
fn cpp() {
    check(
        "x.cpp",
        "#include \"x.h\"

// counter
int &get(Foo &f) { return f.n; }

int Foo::size(int a) const { return a; }
",
        r#"file NULL 1 6 33 2 NULL 0
import "x.h" 1 1 4 0 NULL NULL
fn get 4 4 15 0 1 NULL
fn Foo::size 6 6 14 0 1 NULL
"#,
    );
    check(
        "x.cpp",
        "static int helper(int x) { return x; }
int api(S s) { return helper(1) + ns::f(2) + s.m(3); }
",
        r#"file NULL 1 2 40 0 NULL 0
fn helper 1 1 12 0 1 NULL
static NULL 1 1 12 0 NULL NULL
fn api 2 2 28 0 1 NULL
call helper 2 2 4 0 NULL NULL
call f 2 2 6 0 NULL NULL
call m 2 2 6 0 NULL NULL
"#,
    );
    structure(
        "x.cpp",
        "int f(std::vector<int> v) {
    for (int x : v) { if (x < 0) throw 1; }
    try { g(); } catch (...) { return 0; }
    return 1;
}
",
        r#"block 1 0 39
branch 2 0 18
block 2 1 11
branch 2 1 9
throw 2 0 3
block 3 1 6
branch 3 0 9
block 3 1 5
"#,
    );
}

/// Python: visibility comes from the name, in SQL.
#[test]
fn python() {
    check(
        "x.py",
        "import os, sys
from a.b import c

# twice
def double(x, y=1):
    return x * 2
",
        r#"file NULL 1 6 24 2 NULL 0
import os 1 1 4 0 NULL NULL
import sys 1 1 4 0 NULL NULL
import a.b.c 2 2 6 0 NULL NULL
fn double 5 6 14 0 2 NULL
"#,
    );
    check(
        "x.py",
        "def _private(x):
    return len(x) + os.path.join(x)
",
        r#"file NULL 1 2 20 0 NULL 0
fn _private 1 2 20 0 1 NULL
call len 2 2 4 0 NULL NULL
call join 2 2 8 0 NULL NULL
"#,
    );
    structure(
        "x.py",
        "def f(x):
    global COUNT
    if x and x > 1:
        for i in range(x):
            if i == 2 or i == 3:
                raise ValueError()
    elif x is None:
        return [y for y in x if y]
    try:
        pass
    except KeyError:
        return 0
    return 1 if x else 2
",
        r#"block 2 0 59
global_state 2 0 2
branch 3 0 43
condition 3 0 5
block 4 1 21
branch 4 1 21
block 5 2 13
branch 5 2 13
condition 5 0 7
block 6 3 4
throw 6 0 4
branch 7 1 15
block 8 1 10
branch 8 2 2
block 10 1 1
branch 11 0 5
block 12 1 2
branch 13 0 5
"#,
    );
}

/// TypeScript, TSX and JavaScript share one query: functions, calls, imports and
/// exports, and control-flow structure.
#[test]
fn ecma() {
    check(
        "x.ts",
        "import { a } from './a';

// adds
function add(x: number, y: number): number { return x + y; }
const id = (v: string) => v;
const inc = n => n + 1;
class K { m(a, b) { return a; } }
",
        r#"file NULL 1 7 64 2 NULL 0
import ./a 1 1 9 0 NULL NULL
fn add 4 4 20 0 2 NULL
fn id 5 5 9 0 1 NULL
fn inc 6 6 7 0 NULL NULL
scope K 7 7 15 0 NULL NULL
fn m 7 7 11 0 2 NULL
"#,
    );
    check(
        "x.tsx",
        "import React from 'react';
/** a view */
const View = (p: Props) => <div>{p.x}</div>;
",
        r#"file NULL 1 3 28 4 NULL 0
import react 1 1 7 0 NULL NULL
fn View 3 3 19 0 1 NULL
"#,
    );
    check(
        "x.js",
        "import x from \"x\";
// gen
function* g(a) { yield a; }
const f = function (a, b) { return a; };
",
        r#"file NULL 1 4 33 2 NULL 0
import x 1 1 7 0 NULL NULL
fn g 3 3 11 0 1 NULL
fn f 4 4 13 0 2 NULL
"#,
    );
    // Calls and `export` markers, alike in every ECMAScript dialect.
    let src = "export function a() { b(); this.c(); }
function b() {}
export function* g() {}
export const h = () => b();
export const k = function () {};
const internal = () => 1;
";
    for path in ["x.ts", "x.tsx", "x.js"] {
        check(
            path,
            src,
            r#"file NULL 1 6 60 0 NULL 0
fn a 1 1 16 0 0 NULL
pub NULL 1 1 16 0 NULL NULL
call b 1 1 3 0 NULL NULL
call c 1 1 5 0 NULL NULL
fn b 2 2 6 0 0 NULL
fn g 3 3 7 0 0 NULL
pub NULL 3 3 7 0 NULL NULL
fn h 4 4 8 0 0 NULL
pub NULL 4 4 8 0 NULL NULL
call b 4 4 3 0 NULL NULL
fn k 5 5 7 0 0 NULL
pub NULL 5 5 7 0 NULL NULL
fn internal 6 6 6 0 0 NULL
"#,
        );
    }
    let src = "let count = 0;
export let shared = 1;
const fixed = 2;
var legacy;
function f(a) {
    if (a && a.b) {
        for (const k in a) { if (k ?? 0) throw new Error(); }
    }
    try { g(); } catch (e) { return a ? 1 : 2; }
    switch (a) { case 1: return 1; }
}
";
    for path in ["x.ts", "x.tsx", "x.js"] {
        structure(
            path,
            src,
            r#"global_state 1 0 5
global_state 2 0 5
global_state 4 0 3
block 5 0 65
branch 6 0 31
condition 6 0 5
block 6 1 23
branch 7 1 21
block 7 2 14
branch 7 2 12
condition 7 0 3
throw 7 0 6
block 9 1 6
branch 9 0 13
block 9 1 9
branch 9 1 5
branch 10 0 6
"#,
        );
    }
}

/// Rust code compiled only for tests: `test` spans over `#[cfg(test)]` items and
/// `#[test]`-style functions, attributes included; `test_if` spans, named by the
/// features that would make them test code, over feature-gated items.
#[test]
fn rust_test_code() {
    let src = r#"fn prod() {}
#[cfg(test)]
// helpers
#[allow(dead_code)]
mod tests {
    #[tokio::test]
    async fn t() { x.unwrap(); }
}
#[test] fn bare() {}
#[cfg(feature = "test-util")] pub fn util() {}
#[cfg(any(test, feature = "x"))] fn either() {}
#[cfg(all(unix, any(feature = "a", feature = "b"), not(test)))] fn gated() {}
#[cfg(any(test, unix))] fn not_test() {}
#[cfg(all(feature = "a", feature = "b"))] fn both() {}
#[cfg(not(test))] fn production() {}
#[cfg_attr(test, derive(Debug))] struct S;
#[cfg(test)] mod testutil;
mod other;
"#;
    let tests = kinds("x.rs", src, &["test", "test_if", "mod_decl"], |s| {
        let text = &src[s.start_byte as usize..s.end_byte as usize];
        let first = text.lines().next().unwrap();
        format!(
            "{} {} {} {}: {first}",
            s.kind,
            s.name.as_deref().unwrap_or("-"),
            s.line,
            s.end_line
        )
    });
    assert_eq!(
        tests,
        [
            "test - 2 8: #[cfg(test)]",
            "test - 6 7: #[tokio::test]",
            "test - 9 9: #[test] fn bare() {}",
            "test_if test-util 10 10: #[cfg(feature = \"test-util\")] pub fn util() {}",
            "test_if x 11 11: #[cfg(any(test, feature = \"x\"))] fn either() {}",
            // any(..) is test-only when every part is, all(..) when one part is.
            "test_if a&b 12 12: #[cfg(all(unix, any(feature = \"a\", feature = \"b\"), not(test)))] fn gated() {}",
            "test_if a|b 14 14: #[cfg(all(feature = \"a\", feature = \"b\"))] fn both() {}",
            "test - 17 17: #[cfg(test)] mod testutil;",
            "mod_decl testutil 17 17: mod testutil;",
            "mod_decl other 18 18: mod other;",
        ]
    );
    // An inner attribute makes the whole file (or module) test code.
    let spans = extract(
        Lang::of("x.rs").unwrap(),
        b"//! doc\n#![cfg(test)]\nfn t() {}\n",
    );
    let test = spans.iter().find(|s| s.kind == "test").unwrap();
    assert_eq!(
        (test.start_byte, test.end_byte, test.tokens),
        (0, 32, spans[0].tokens)
    );
}

/// Scopes qualify function names: Rust impls (named by type, and trait for a trait
/// impl) and traits, classes, C++ namespaces.
#[test]
fn scopes() {
    let scopes = |path, src| kinds(path, src, &["scope"], |s| s.name.unwrap());
    assert_eq!(
        scopes(
            "x.rs",
            "impl Foo<T> { fn new() {} }\nimpl fmt::Display for Foo<T> { fn fmt() {} }\ntrait Bar { fn b() {} }\n"
        ),
        ["Foo<T>", "<Foo<T> as fmt::Display>", "Bar"]
    );
    assert_eq!(scopes("x.py", "class A:\n    def f(self): pass\n"), ["A"]);
    assert_eq!(
        scopes(
            "x.cpp",
            "namespace n { class C { void f(); }; struct D; }\n"
        ),
        ["n", "C"]
    );
    assert_eq!(
        scopes("x.js", "class A { f() {} }\nconst B = class Q {};\n"),
        ["A", "Q"]
    );
}

/// Panics by intent: stated invariants, fallible operations left unhandled,
/// unfinished code; in Python and C only where the language has the construct.
#[test]
fn panics_by_intent() {
    let kinds = |path, src| {
        let keep = ["throw", "invariant", "unhandled", "unfinished"];
        kinds(path, src, &keep, |s| {
            format!("{}:{}", s.kind, s.name.unwrap_or_default())
        })
        .join(" ")
    };
    assert_eq!(
        kinds(
            "x.rs",
            "fn f() { assert!(a); debug_assert_eq!(a, b); unreachable!(); panic!(); todo!(); unimplemented!(); x.unwrap(); y.expect(\"e\"); x.unwrap_or(1); }"
        ),
        // Panics that exit non-locally are also throws, for exceptions.
        "invariant:assert invariant:debug_assert_eq invariant:unreachable throw: throw: throw: unfinished: throw: unfinished: throw: unhandled: throw: unhandled:"
    );
    assert_eq!(
        kinds(
            "x.py",
            "def f():\n    assert x\n    raise NotImplementedError\n    raise NotImplementedError('no')\n    raise ValueError()\n"
        ),
        "invariant: throw: unfinished: throw: unfinished: throw:"
    );
    assert_eq!(
        kinds("x.c", "void f(void) { assert(x > 0); abort(); }"),
        "invariant:"
    );
}

/// Clone fingerprints: a run of 70+ tokens repeated with other names and literals
/// shares a fingerprint with the original; unrelated code does not.
#[test]
fn clone_fingerprints() {
    let body = |f: &str, v: &str| {
        format!(
            "fn {f}(items: &[i32], {v}: i32) -> i32 {{\n    let mut total = 0;\n    for item in items {{\n        if *item > {v} {{ total += item * 2; }} else {{ total -= 1; }}\n        if *item == {v} {{ return total; }}\n    }}\n    total + {v} * 3\n}}\n"
        )
    };
    let fps = |src: &str| -> std::collections::HashSet<u64> {
        kinds("x.rs", src, &["fingerprint"], |s| s.hash)
            .into_iter()
            .collect()
    };
    let a = fps(&body("sum", "limit"));
    let b = fps(&body("total_over", "bound"));
    let c = fps(
        "struct Config { name: String, size: usize }\nimpl Config { fn new() -> Config { Config { name: String::new(), size: 0 } } }\n",
    );
    assert!(
        !a.is_empty() && a.is_subset(&b) && b.is_subset(&a),
        "{a:?} {b:?}"
    );
    assert!(a.is_disjoint(&c));
}

/// Function extras: pass-through wrappers, doc comments with their overlap with
/// the signature, vocabulary; and comment markers.
#[test]
fn function_extras_and_markers() {
    let kinds = |path, src| {
        let keep = ["forward", "doc", "vocabulary", "marker"];
        kinds(path, src, &keep, |s| {
            format!("{}:{}:{}:{}", s.kind, s.line, s.tokens, s.comments)
        })
    };
    assert_eq!(
        kinds(
            "x.rs",
            "/// Reads the config file.\n#[inline]\npub fn read_config(path: &str, strict: bool) -> Config {\n    load(path, strict)\n}\n\
             fn sub(a: i32, b: i32) -> i32 { a - b }\n// TODO: remove\nfn swap(a: i32, b: i32) -> i32 { inner.go(b, a) }\n"
        ),
        // read_config forwards; its doc's subtokens (reads, the, config, file) share
        // config with the signature. sub calls nothing. swap forwards in another order.
        [
            "vocabulary:3:17:0",
            "forward:3:23:0",
            "doc:3:4:1",
            "vocabulary:6:13:0",
            "vocabulary:8:15:0",
            "forward:8:23:0",
            "marker:7:0:2",
        ]
    );
    assert_eq!(
        kinds(
            "x.py",
            "def f(a, b):\n    \"\"\"Compute f of a and b.\"\"\"\n    return g(a, b)\n# FIXME\n"
        ),
        [
            "vocabulary:1:12:0",
            "forward:1:18:0",
            "doc:1:3:0",
            "marker:4:0:1"
        ]
    );
    assert_eq!(
        kinds(
            "x.ts",
            "/** Adds. */\nexport function add(a: number, b: number) { return plus(a, b); }\n// do not touch this\n"
        ),
        [
            "vocabulary:2:14:0",
            "forward:2:21:0",
            "doc:2:1:0",
            "marker:3:0:4"
        ]
    );
}

/// Empty input gives one `file` span in every language (compiling every query, which
/// checks their captures); broken input counts its parse errors.
#[test]
fn degenerate_input() {
    // Also compiles every language's query.
    for l in LANGS {
        let s = extract(*l, b"");
        assert_eq!(render(&s), "file NULL 1 1 0 0 NULL 0\n", "{}", l.name);
    }
    let s = extract(Lang::of("x.c").unwrap(), b"int f( {{{ ");
    assert_eq!(s[0].kind, "file");
    assert!(s[0].errors.unwrap() >= 1);
}

/// Import statements become one `import` span per target path.
#[test]
fn imports() {
    let names = |path, src| {
        kinds(path, src, &["import", "module"], |s| {
            format!("{}:{}", s.kind, s.name.unwrap())
        })
        .join(" ")
    };
    assert_eq!(
        names(
            "x.rs",
            "use crate::{a::X, b::{self, c as d, e::*}, f};\nmod tests {\n    use super::g;\n}\nuse std::{\n    io,\n};\nmod declared;\n"
        ),
        "import:crate::a::X import:crate::b import:crate::b::c import:crate::b::e import:crate::f \
         module:tests import:super::g import:std::io"
    );
    assert_eq!(
        names(
            "x.py",
            "import a.b as ab, c\nfrom . import d\nfrom ..e import f as g, h\nfrom i import *\n"
        ),
        "import:a.b import:c import:.d import:..e.f import:..e.h import:i"
    );
    assert_eq!(
        names("x.c", "#include \"a/b.h\"\n#include <stdio.h>\n"),
        "import:\"a/b.h\" import:<stdio.h>"
    );
    assert_eq!(
        names(
            "x.ts",
            "import a from './a';\nexport { b } from \"../b\";\nexport const c = 1;\n\
             const d = require('./d');\nconst e = await import('./e');\nconst f = require(name);\n"
        ),
        "import:./a import:../b import:./d import:./e"
    );
}

/// Manifests are selected by file name and give the package's name: Cargo's
/// [package] and [lib] names, package.json's top-level name (not a dependency's);
/// tsconfig.json gives its path aliases and baseUrl.
#[test]
fn manifests() {
    let packages = |path, src| kinds(path, src, &["package"], |s| s.name.unwrap());
    assert_eq!(
        packages(
            "crates/x/Cargo.toml",
            "[package]\nname = \"my-crate\"\nversion = \"1\"\n[lib]\nname = \"mine\"\n[dependencies]\nname = \"no\"\n"
        ),
        ["\"my-crate\"", "\"mine\""]
    );
    assert_eq!(
        packages(
            "web/package.json",
            r#"{"name": "@scope/pkg", "dependencies": {"name": "no"}}"#
        ),
        ["@scope/pkg"]
    );
    assert!(Lang::of("pyproject.toml").is_none());
    let spans = |path, src| {
        let keep = ["base_url", "alias", "alias_target"];
        kinds(path, src, &keep, |s| {
            format!("{}:{}", s.kind, s.name.unwrap())
        })
    };
    assert_eq!(
        spans(
            "web/tsconfig.json",
            r#"{"compilerOptions": {"baseUrl": "src", "paths": {"@/*": ["./*", "./gen/*"]}}, "paths": {"no": ["x"]}}"#
        ),
        [
            "base_url:src",
            "alias:@/*",
            "alias_target:./*",
            "alias_target:./gen/*"
        ]
    );
}

/// A line diff's edits, for `Tree::edit`: last hunk first, each in the coordinates of
/// the text with the later hunks already applied.
#[test]
fn edits_of_a_line_diff() {
    let edit = |e: &tree_sitter::InputEdit| {
        let p = |p: tree_sitter::Point| format!("{}:{}", p.row, p.column);
        format!(
            "{}-{}>{} {}-{}>{}",
            e.start_byte,
            e.old_end_byte,
            e.new_end_byte,
            p(e.start_position),
            p(e.old_end_position),
            p(e.new_end_position)
        )
    };
    let show = |old: &str, new: &str| {
        edits(old.as_bytes(), new.as_bytes())
            .iter()
            .map(edit)
            .collect::<Vec<_>>()
    };
    assert!(show("a\nb\n", "a\nb\n").is_empty());
    // One line replaced by two.
    assert_eq!(show("a\nb\nc\n", "a\nX\nY\nc\n"), ["2-4>6 1:0-2:0>3:0"]);
    // Two hunks, the later first: the first's new end ignores the second's growth.
    assert_eq!(
        show("a\nb\nc\nd\n", "a\nB\nb\nc\nd\nE\n"),
        ["8-8>10 4:0-4:0>5:0", "2-2>4 1:0-1:0>2:0"]
    );
    // A last line without a newline, a multi-byte character (columns are bytes) and
    // a deletion to nothing.
    assert_eq!(show("a\nb", "a\nbé"), ["2-3>5 1:0-1:1>1:3"]);
    assert_eq!(show("a\nb\n", ""), ["0-4>0 0:0-2:0>0:0"]);
    assert_eq!(show("", "x\r\ny"), ["0-0>4 0:0-0:0>1:1"]);
}

/// Incremental extraction gives exactly `extract`'s spans, whatever the edits between
/// successive blobs: lines inserted, deleted, moved and mangled (into parse errors),
/// line endings and the last newline toggled, multi-byte text, empty files, and a
/// change of language.
#[test]
fn incremental_extraction_equals_extract() {
    let samples = [
        (
            "x.rs",
            "use std::io;\n\n/// Adds.\nfn add(a: i32, b: i32) -> i32 {\n    if a > b { a + b } else { b }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { assert_eq!(super::add(1, 2), 3); }\n}\nimpl Foo for Bar { fn x(&self) { todo!() } }\n",
        ),
        (
            "x.c",
            "#include <stdio.h>\n\nstatic int n = 0;\n/* TODO: more */\nint f(int a) {\n    for (int i = 0; i < a; i++) { n += i; }\n    return n;\n}\nint main(void) { return f(3); }\n",
        ),
        (
            "x.cpp",
            "#include \"a.h\"\nnamespace ns {\nclass A {\n  int f(int x) { return x ? 1 : 2; }\n};\n}\nvoid g() { throw 1; }\n",
        ),
        (
            "x.py",
            "import os\nfrom a import b\n\nclass C:\n    def f(self, x):\n        \"\"\"Doc.\"\"\"\n        if x:\n            return g(x)\n        raise ValueError()\n\ndef g(x):\n    return [i for i in x]\n",
        ),
        (
            "x.ts",
            "import a from './a';\n/** Adds. */\nexport function add(a: number, b: number) { return plus(a, b); }\nconst f = (x: number) => { while (x) { x--; } };\nclass K { m() { return 1; } }\n",
        ),
        (
            "x.tsx",
            "export const C = () => <div onClick={() => go()}>hi</div>;\nfunction go() { if (x) { return 1; } }\n",
        ),
        (
            "x.js",
            "const a = require('a');\nfunction f(x) {\n  switch (x) { case 1: return 2; default: throw new Error(); }\n}\nmodule.exports = { f };\n",
        ),
    ];
    // A tiny deterministic generator: the test is the same every run.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    let snippets = [
        "{",
        "}",
        "(",
        "é",
        "日本",
        "// TODO x\n",
        "\"",
        "/*",
        "\n\n",
        "\t",
    ];
    let mut incremental = Incremental::new();
    let mut versions = 0;
    for (path, sample) in samples {
        let lang = Lang::of(path).unwrap();
        let pool: Vec<String> = samples
            .iter()
            .filter(|(p, _)| Lang::of(p).unwrap().name == lang.name)
            .flat_map(|(_, s)| s.lines().map(|l| format!("{l}\n")))
            .collect();
        let mut lines: Vec<String> = sample.lines().map(|l| format!("{l}\n")).collect();
        let (mut crlf, mut last_newline) = (false, true);
        for _ in 0..60 {
            match next(9) {
                0 | 1 => lines.insert(next(lines.len() + 1), pool[next(pool.len())].clone()),
                2 if !lines.is_empty() => {
                    lines.remove(next(lines.len()));
                }
                3 if !lines.is_empty() => {
                    let i = next(lines.len());
                    let line = lines.remove(i);
                    lines.insert(next(lines.len() + 1), line);
                }
                4 | 5 if !lines.is_empty() => {
                    let i = next(lines.len());
                    let mut at = next(lines[i].len());
                    while !lines[i].is_char_boundary(at) {
                        at -= 1;
                    }
                    lines[i].insert_str(at, snippets[next(snippets.len())]);
                }
                6 => crlf = !crlf,
                7 => last_newline = !last_newline,
                8 if next(8) == 0 => lines.clear(),
                _ => {}
            }
            let mut src: String = lines.concat();
            if !last_newline && src.ends_with('\n') {
                src.pop();
            }
            if crlf {
                src = src.replace('\n', "\r\n");
            }
            let fresh = extract(lang, src.as_bytes());
            assert_eq!(
                incremental.extract(lang, src.as_bytes()),
                fresh,
                "{path}:\n{src}"
            );
            versions += 1;
        }
    }
    assert_eq!(versions, 7 * 60);
}

/// Incremental extraction copies an unchanged subtree's facts, but what depends on
/// its surroundings is found again: a doc comment or decorator added above an
/// unchanged function, a test attribute above an unchanged item, code wrapped into a
/// new block or module (its depth), and a file that becomes one comment.
#[test]
fn incremental_extraction_rereads_the_context() {
    let f = "fn f(a: i32) -> i32 {\n    if a > 0 { g(a) } else { 0 }\n}\n";
    let py = "def f(x):\n    return g(x)\n";
    let ts = "function f(a: number) { return g(a); }\n";
    let versions: [(&str, Vec<String>); 3] = [
        (
            "x.rs",
            vec![
                f.to_owned(),
                format!("/// Calls g.\n{f}"),
                format!("// plain\n{f}"),
                format!("#[cfg(test)]\n{f}"),
                format!("mod m {{\n{f}}}\n"),
                format!("mod m {{\nfn outer() {{\n{f}}}\n}}\n"),
                format!("{f}// TODO later\n"),
                format!("{f}fn broken( {{\n"),
                "// only a comment".to_owned(),
                f.to_owned(),
            ],
        ),
        (
            "x.py",
            vec![
                py.to_owned(),
                format!("@decorated\n{py}"),
                format!(
                    "class C:\n{}",
                    py.lines().map(|l| format!("    {l}\n")).collect::<String>()
                ),
                format!("{py}\"\"\"doc\"\"\"\n"),
                py.to_owned(),
            ],
        ),
        (
            "x.ts",
            vec![
                ts.to_owned(),
                format!("/** Calls g. */\n{ts}"),
                format!("export {ts}"),
                format!("class K {{\n}}\n{ts}"),
                ts.to_owned(),
            ],
        ),
    ];
    for (path, versions) in versions {
        let lang = Lang::of(path).unwrap();
        let mut incremental = Incremental::new();
        for src in versions {
            let fresh = extract(lang, src.as_bytes());
            assert_eq!(
                incremental.extract(lang, src.as_bytes()),
                fresh,
                "{path}:\n{src}"
            );
        }
    }
}
