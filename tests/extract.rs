use codevitals::{LANGS, Lang, Span, extract};

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

/// One line per span: kind name line end_line tokens comments params errors.
fn render(spans: &[Span]) -> String {
    let opt = |v: Option<i32>| v.map_or("NULL".into(), |v| v.to_string());
    spans
        .iter()
        .filter(|s| !STRUCTURE.contains(&s.kind) && s.kind != "fingerprint")
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

/// The control-flow structure of `src`: one line per structural span, as
/// `kind line depth tokens`, checked against `expected`.
fn structure(path: &str, src: &str, expected: &str) {
    let spans = extract(Lang::of(path).expect("known extension"), src.as_bytes());
    let lines: String = spans
        .iter()
        .filter(|s| STRUCTURE.contains(&s.kind))
        .map(|s| format!("{} {} {} {}\n", s.kind, s.line, s.depth, s.tokens))
        .collect();
    assert_eq!(lines, expected, "{path} structure for:\n{src}");
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
        "static mut COUNT: i32 = 0;
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
",
        r#"global_state 1 0 8
block 2 0 77
branch 3 0 61
condition 3 0 13
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
        "int counter = 0;
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
",
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
    let spans = extract(Lang::of("x.rs").unwrap(), src.as_bytes());
    let tests: Vec<String> = spans
        .iter()
        .filter(|s| ["test", "test_if", "mod_decl"].contains(&s.kind))
        .map(|s| {
            let text = &src[s.start_byte as usize..s.end_byte as usize];
            let first = text.lines().next().unwrap();
            format!(
                "{} {} {} {}: {first}",
                s.kind,
                s.name.as_deref().unwrap_or("-"),
                s.line,
                s.end_line
            )
        })
        .collect();
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
    let scopes = |path: &str, src: &str| {
        extract(Lang::of(path).unwrap(), src.as_bytes())
            .into_iter()
            .filter(|s| s.kind == "scope")
            .map(|s| s.name.unwrap())
            .collect::<Vec<_>>()
    };
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
    let kinds = |path: &str, src: &str| {
        extract(Lang::of(path).unwrap(), src.as_bytes())
            .into_iter()
            .filter(|s| ["throw", "invariant", "unhandled", "unfinished"].contains(&s.kind))
            .map(|s| format!("{}:{}", s.kind, s.name.unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(
        kinds(
            "x.rs",
            "fn f() { assert!(a); debug_assert_eq!(a, b); unreachable!(); panic!(); todo!(); unimplemented!(); x.unwrap(); y.expect(\"e\"); x.unwrap_or(1); }"
        ),
        "invariant:assert invariant:debug_assert_eq invariant:unreachable throw: unfinished: unfinished: unhandled: unhandled:"
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

/// Clone fingerprints: a run of 60+ tokens repeated with other names and literals
/// shares a fingerprint with the original; unrelated code does not.
#[test]
fn clone_fingerprints() {
    let body = |f: &str, v: &str| {
        format!(
            "fn {f}(items: &[i32], {v}: i32) -> i32 {{\n    let mut total = 0;\n    for item in items {{\n        if *item > {v} {{ total += item * 2; }} else {{ total -= 1; }}\n        if *item == {v} {{ return total; }}\n    }}\n    total + {v} * 3\n}}\n"
        )
    };
    let fps = |src: &str| -> std::collections::HashSet<u64> {
        extract(Lang::of("x.rs").unwrap(), src.as_bytes())
            .into_iter()
            .filter(|s| s.kind == "fingerprint")
            .map(|s| s.hash)
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
    let names = |path: &str, src: &str| {
        let lang = Lang::of(path).unwrap();
        extract(lang, src.as_bytes())
            .into_iter()
            .filter(|s| ["import", "module"].contains(&s.kind))
            .map(|s| format!("{}:{}", s.kind, s.name.unwrap()))
            .collect::<Vec<_>>()
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
    let packages = |path: &str, src: &str| {
        extract(Lang::of(path).unwrap(), src.as_bytes())
            .into_iter()
            .filter(|s| s.kind == "package")
            .map(|s| s.name.unwrap())
            .collect::<Vec<_>>()
    };
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
    let spans = |path: &str, src: &str| {
        extract(Lang::of(path).unwrap(), src.as_bytes())
            .into_iter()
            .filter(|s| s.kind != "file")
            .map(|s| format!("{}:{}", s.kind, s.name.unwrap()))
            .collect::<Vec<_>>()
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
