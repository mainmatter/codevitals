//! `extract`: a pure function from one blob's bytes to its span facts.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::OnceLock;

use tree_sitter::{Language, Node, Parser, Point, Query, QueryCursor, StreamingIterator};
use xxhash_rust::xxh3::xxh3_64;

/// A language: a name, the extensions that select it, a grammar and a query file.
#[derive(Clone, Copy)]
pub struct Lang {
    pub name: &'static str,
    pub exts: &'static [&'static str],
    pub grammar: fn() -> Language,
    pub query: &'static str,
}

pub const LANGS: &[Lang] = &[
    Lang {
        name: "rust",
        exts: &["rs"],
        grammar: || tree_sitter_rust::LANGUAGE.into(),
        query: include_str!("../langs/rust.scm"),
    },
    Lang {
        name: "c",
        exts: &["c"],
        grammar: || tree_sitter_c::LANGUAGE.into(),
        query: include_str!("../langs/c.scm"),
    },
    Lang {
        name: "cpp",
        exts: &["h", "cc", "cpp", "cxx", "hh", "hpp", "hxx"],
        grammar: || tree_sitter_cpp::LANGUAGE.into(),
        query: include_str!("../langs/cpp.scm"),
    },
    Lang {
        name: "python",
        exts: &["py"],
        grammar: || tree_sitter_python::LANGUAGE.into(),
        query: include_str!("../langs/python.scm"),
    },
    Lang {
        name: "typescript",
        exts: &["ts", "mts", "cts"],
        grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        query: include_str!("../langs/ecma.scm"),
    },
    Lang {
        name: "tsx",
        exts: &["tsx"],
        grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        query: include_str!("../langs/ecma.scm"),
    },
    Lang {
        name: "javascript",
        exts: &["js", "mjs", "cjs", "jsx"],
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        query: include_str!("../langs/ecma.scm"),
    },
];

impl Lang {
    /// The language selected by the lowercased extension of `path`.
    pub fn of(path: &str) -> Option<Lang> {
        let ext = std::path::Path::new(path)
            .extension()?
            .to_str()?
            .to_lowercase();
        LANGS
            .iter()
            .copied()
            .find(|l| l.exts.contains(&ext.as_str()))
    }

    /// The compiled query, memoized: compiling costs up to 13 ms (C++), far more
    /// than parsing a typical file. It depends only on `LANGS`, so `extract` stays pure.
    fn compiled_query(self) -> &'static Query {
        static QUERIES: OnceLock<Vec<Query>> = OnceLock::new();
        let queries = QUERIES.get_or_init(|| {
            let compile = |l: &Lang| Query::new(&(l.grammar)(), l.query).expect("query compiles");
            LANGS.iter().map(compile).collect()
        });
        &queries[LANGS
            .iter()
            .position(|l| l.name == self.name)
            .expect("lang is in LANGS")]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub kind: String,
    pub name: Option<String>,
    pub line: i32,
    pub end_line: i32,
    pub tokens: i32,
    pub comments: i32,
    pub params: Option<i32>,
    pub hash: u64,
    pub errors: Option<i32>,
}

/// Parse `src`, run the language's query and return the `file` span followed by
/// every captured span in source order (outer before inner).
pub fn extract(lang: Lang, src: &[u8]) -> Vec<Span> {
    let language = (lang.grammar)();
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .expect("grammar ABI is supported");
    let tree = parser
        .parse(src, None)
        .expect("parse has no timeout or cancellation");
    let query = lang.compiled_query();
    let text = |n: Node| String::from_utf8_lossy(&src[n.byte_range()]).into_owned();

    // Comments as (start, end) -> word count, and spans keyed by identity
    // (kind, start, end), ordered by position with outer spans first.
    let mut comments = BTreeMap::new();
    let mut spans = BTreeMap::<_, (Node, Option<Node>, Option<Node>)>::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), src);
    while let Some(m) = matches.next() {
        let (mut span, mut name, mut params) = (None, None, None);
        for c in m.captures {
            match query.capture_names()[c.index as usize] {
                "comment" => {
                    let words = text(c.node).split_whitespace().count();
                    comments.insert((c.node.start_byte(), c.node.end_byte()), words);
                }
                "name" => name = name.or(Some(c.node)),
                "params" => params = params.or(Some(c.node)),
                private if private.starts_with('_') => {} // query-internal, e.g. for predicates
                kind => span = span.or(Some((kind, c.node))),
            }
        }
        if let Some((kind, node)) = span {
            let key = (node.start_byte(), Reverse(node.end_byte()), kind);
            let merged = spans.entry(key).or_insert((node, None, None));
            merged.1 = merged.1.or(name);
            merged.2 = merged.2.or(params);
        }
    }

    let is_comment = |n: Node| comments.contains_key(&(n.start_byte(), n.end_byte()));

    // Code leaves as (start, end, kind_id) in source order, and the error count.
    // Pre-order visits nodes in non-decreasing start order, so one forward pass over
    // the sorted comments finds each comment node.
    let mut leaves = Vec::<(usize, usize, u16)>::new();
    let mut errors = 0;
    let has_errors = tree.root_node().has_error();
    let mut pending = comments.keys().peekable();
    let mut walk = tree.walk();
    // Track depth ourselves: `TreeCursor::depth` rescans the cursor's stack on every call.
    let mut depth = 0usize;
    let mut comment_depth = None; // depth of the comment node we are inside
    'walk: loop {
        let n = walk.node();
        let (start, end) = (n.start_byte(), n.end_byte());
        if has_errors {
            errors += usize::from(n.is_error() || n.is_missing());
        }
        if comment_depth.is_none() {
            while pending.next_if(|c| c.0 < start).is_some() {}
            if pending.next_if_eq(&&(start, end)).is_some() {
                comment_depth = Some(depth);
            }
        }
        if walk.goto_first_child() {
            depth += 1;
            continue;
        }
        if comment_depth.is_none() && start < end {
            leaves.push((start, end, n.kind_id()));
        }
        loop {
            if comment_depth == Some(depth) {
                comment_depth = None;
            }
            if walk.goto_next_sibling() {
                continue 'walk;
            }
            if !walk.goto_parent() {
                break 'walk;
            }
            depth -= 1;
        }
    }
    assert!(
        leaves.windows(2).all(|w| w[0].1 <= w[1].0),
        "leaves are disjoint and ordered"
    );

    let span = |kind: &str, bytes: Range<usize>, start: Point, end: Point| {
        // Leaves are disjoint and sorted, so both their starts and ends are sorted.
        let lo = leaves.partition_point(|l| l.0 < bytes.start);
        let hi = leaves.partition_point(|l| l.1 <= bytes.end);
        let inside = &leaves[lo..hi];
        let mut hashed = lang.name.as_bytes().to_vec();
        inside.iter().for_each(|l| hashed.extend(l.2.to_le_bytes()));
        let line = start.row + 1;
        let end_line = if end.column == 0 {
            end.row
        } else {
            end.row + 1
        };
        Span {
            kind: kind.to_owned(),
            name: None,
            line: int(line),
            end_line: int(end_line.max(line)),
            tokens: int(inside.len()),
            comments: int(comments
                .range((bytes.start, 0)..)
                .take_while(|((s, _), _)| *s < bytes.end)
                .filter(|((_, e), _)| *e <= bytes.end)
                .map(|(_, words)| words)
                .sum()),
            params: None,
            hash: xxh3_64(&hashed),
            errors: None,
        }
    };

    let last_line = src
        .rsplit(|&b| b == b'\n')
        .next()
        .expect("rsplit yields at least once");
    let src_end = Point::new(src.iter().filter(|&&b| b == b'\n').count(), last_line.len());
    let mut file = span("file", 0..src.len(), Point::new(0, 0), src_end);
    file.errors = Some(int(errors));

    let mut out = vec![file];
    for ((_, _, kind), (node, name, params)) in spans {
        let mut s = span(
            kind,
            node.byte_range(),
            node.start_position(),
            node.end_position(),
        );
        s.name = name.map(text);
        s.params = params.map(|p| {
            int(p
                .named_children(&mut p.walk())
                .filter(|&n| !is_comment(n))
                .count())
        });
        out.push(s);
    }
    out
}

fn int(n: usize) -> i32 {
    i32::try_from(n).expect("count fits in i32")
}
