use tally::{LANGS, Lang, Span, extract};

fn lang(name: &str) -> Lang {
    *LANGS
        .iter()
        .find(|l| l.name == name)
        .expect("known language")
}

/// One line per span: kind name line end_line tokens comments params errors.
fn render(spans: &[Span]) -> String {
    let opt = |v: Option<i32>| v.map_or("NULL".into(), |v| v.to_string());
    spans
        .iter()
        .map(|s| {
            let name = s.name.as_deref().unwrap_or("NULL");
            let (p, e) = (opt(s.params), opt(s.errors));
            let (k, l, el, t, c) = (&s.kind, s.line, s.end_line, s.tokens, s.comments);
            format!("{k} {name} {l} {el} {t} {c} {p} {e}\n")
        })
        .collect()
}

fn check(name: &str, src: &str, expected: &str) -> Vec<Span> {
    let spans = extract(lang(name), src.as_bytes());
    assert_eq!(render(&spans), expected, "{name} spans for:\n{src}");
    spans
}

#[test]
fn queries_compile() {
    for l in LANGS {
        tree_sitter::Query::new(&(l.grammar)(), l.query)
            .unwrap_or_else(|e| panic!("{}: {e}", l.name));
    }
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
        "rust",
        src,
        "file NULL 1 15 60 7 NULL 0
import std::collections::HashMap 1 1 7 0 NULL NULL
fn add 4 7 18 3 2 NULL
fn get 10 10 13 0 1 NULL
fn plus 13 15 18 0 2 NULL
",
    );
    assert_eq!(s[2].hash, s[4].hash);
    assert_ne!(s[3].hash, s[2].hash);
}

#[test]
fn c() {
    let src = "#include <stdio.h>

/* make a buffer */
char *buf(int n, int m) {
    return 0;
}

int sum(int a, int b) { return a + b; }
";
    check(
        "c",
        src,
        r#"file NULL 1 8 33 5 NULL 0
import <stdio.h> 1 1 2 0 NULL NULL
fn buf 4 6 15 0 2 NULL
fn sum 8 8 16 0 2 NULL
"#,
    );
}

#[test]
fn cpp() {
    let src = "#include \"x.h\"

// counter
int &get(Foo &f) { return f.n; }

int Foo::size(int a) const { return a; }
";
    check(
        "cpp",
        src,
        r#"file NULL 1 6 33 2 NULL 0
import "x.h" 1 1 4 0 NULL NULL
fn get 4 4 15 0 1 NULL
fn Foo::size 6 6 14 0 1 NULL
"#,
    );
}

#[test]
fn python() {
    let src = "import os, sys
from a.b import c

# twice
def double(x, y=1):
    return x * 2
";
    check(
        "python",
        src,
        r#"file NULL 1 6 24 2 NULL 0
import os 1 1 4 0 NULL NULL
import a.b 2 2 6 0 NULL NULL
fn double 5 6 14 0 2 NULL
"#,
    );
}

#[test]
fn typescript() {
    let src = "import { a } from './a';

// adds
function add(x: number, y: number): number { return x + y; }
const id = (v: string) => v;
const inc = n => n + 1;
class K { m(a, b) { return a; } }
";
    check(
        "typescript",
        src,
        r#"file NULL 1 7 64 2 NULL 0
import './a' 1 1 9 0 NULL NULL
fn add 4 4 20 0 2 NULL
fn id 5 5 9 0 1 NULL
fn inc 6 6 7 0 NULL NULL
fn m 7 7 11 0 2 NULL
"#,
    );
}

#[test]
fn tsx() {
    let src = "import React from 'react';
/** a view */
const View = (p: Props) => <div>{p.x}</div>;
";
    check(
        "tsx",
        src,
        r#"file NULL 1 3 28 4 NULL 0
import 'react' 1 1 7 0 NULL NULL
fn View 3 3 19 0 1 NULL
"#,
    );
}

#[test]
fn javascript() {
    let src = "import x from \"x\";
// gen
function* g(a) { yield a; }
const f = function (a, b) { return a; };
";
    check(
        "javascript",
        src,
        r#"file NULL 1 4 33 2 NULL 0
import "x" 1 1 7 0 NULL NULL
fn g 3 3 11 0 1 NULL
fn f 4 4 13 0 2 NULL
"#,
    );
}

#[test]
fn empty() {
    for l in LANGS {
        let s = extract(*l, b"");
        assert_eq!(render(&s), "file NULL 1 1 0 0 NULL 0\n", "{}", l.name);
    }
}

#[test]
fn broken() {
    let s = extract(lang("c"), b"int f( {{{ ");
    assert_eq!(s[0].kind, "file");
    assert!(s[0].errors.unwrap() >= 1);
}

#[test]
fn pure() {
    for l in LANGS {
        let src = b"fn f(a) { g(x => x); } // ok\n";
        assert_eq!(extract(*l, src), extract(*l, src), "{}", l.name);
    }
}

/// Calls, visibility markers and private (`_`-prefixed) captures, per language.
fn facts(name: &str, src: &str) -> String {
    let spans = extract(lang(name), src.as_bytes());
    let facts: Vec<_> = spans
        .iter()
        .filter(|s| !["file", "fn", "import"].contains(&s.kind.as_str()))
        .map(|s| {
            format!(
                "{} {} {}",
                s.kind,
                s.name.as_deref().unwrap_or("NULL"),
                s.line
            )
        })
        .collect();
    facts.join("\n")
}

#[test]
fn rust_facts() {
    let src = "pub fn a() { b(); x::c(); self.d(); e::<u8>(); s.f::<T>(); }
fn b() {}
pub(crate) fn g() {}
";
    assert_eq!(
        facts("rust", src),
        "pub NULL 1\ncall b 1\ncall c 1\ncall d 1\ncall e 1\ncall f 1\npub NULL 3"
    );
}

#[test]
fn c_facts() {
    let src = "static int helper(int x) { return x; }
int api(int y) { return helper(y) + g(y); }
extern int h(void);
";
    assert_eq!(facts("c", src), "static NULL 1\ncall helper 2\ncall g 2");
}

#[test]
fn cpp_facts() {
    let src = "static int helper(int x) { return x; }
int api(S s) { return helper(1) + ns::f(2) + s.m(3); }
";
    assert_eq!(
        facts("cpp", src),
        "static NULL 1\ncall helper 2\ncall f 2\ncall m 2"
    );
}

#[test]
fn python_facts() {
    let src = "def _private(x):
    return len(x) + os.path.join(x)
";
    assert_eq!(facts("python", src), "call len 2\ncall join 2");
}

#[test]
fn ecma_facts() {
    let src = "export function a() { b(); this.c(); }
function b() {}
export function* g() {}
export const h = () => b();
export const k = function () {};
const internal = () => 1;
";
    for l in ["typescript", "tsx", "javascript"] {
        assert_eq!(
            facts(l, src),
            "pub NULL 1\ncall b 1\ncall c 1\npub NULL 3\npub NULL 4\ncall b 4\npub NULL 5",
            "{l}"
        );
    }
}
