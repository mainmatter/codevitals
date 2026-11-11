//! `extract`: a pure function from one blob's bytes to its span facts.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use tree_sitter::{Node, Parser, Point, QueryCursor, StreamingIterator};
use xxhash_rust::xxh3::xxh3_64;

use crate::Lang;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub kind: &'static str,
    pub name: Option<String>,
    pub line: i32,
    pub end_line: i32,
    pub tokens: i32,
    pub comments: i32,
    pub params: Option<i32>,
    pub hash: u64,
    pub errors: Option<i32>,
    /// Byte range in the source, so SQL can test containment between spans.
    pub start_byte: i32,
    pub end_byte: i32,
    /// Spans of the same kind that strictly contain this one, e.g. a block's nesting.
    pub depth: i32,
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

    let span = |kind: &'static str, bytes: Range<usize>, start: Point, end: Point| {
        // Leaves are disjoint and sorted, so both their starts and ends are sorted.
        let lo = leaves.partition_point(|l| l.0 < bytes.start);
        let hi = leaves.partition_point(|l| l.1 <= bytes.end);
        let inside = &leaves[lo..hi];
        let mut hashed = lang.name.as_bytes().to_vec();
        for l in inside {
            hashed.extend(l.2.to_le_bytes());
        }
        let line = start.row + 1;
        let end_line = if end.column == 0 {
            end.row
        } else {
            end.row + 1
        };
        Span {
            kind,
            start_byte: int(bytes.start),
            end_byte: int(bytes.end),
            depth: 0,
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

    // Spans come ordered by start, outer before inner, so the open spans of a kind
    // form a stack of end offsets: a span's depth is how many of them are still open.
    let mut open = HashMap::<&str, Vec<usize>>::new();
    let mut out = vec![file];
    for ((_, _, kind), (node, name, params)) in spans {
        let mut s = span(
            kind,
            node.byte_range(),
            node.start_position(),
            node.end_position(),
        );
        let stack = open.entry(kind).or_default();
        while stack.last().is_some_and(|&end| end <= node.start_byte()) {
            stack.pop();
        }
        s.depth = int(stack.len());
        stack.push(node.end_byte());
        s.name = name.map(text);
        s.params = params.map(|p| {
            int(p
                .named_children(&mut p.walk())
                .filter(|&n| !is_comment(n))
                .count())
        });
        if kind == "import" {
            // One span per imported path, all over the statement's bytes.
            for target in import_targets(node, &text) {
                out.push(Span {
                    name: Some(target),
                    ..s.clone()
                });
            }
        } else {
            out.push(s);
        }
    }
    out
}

/// The paths an import statement names, one per target, as written: Rust use trees
/// flattened (`a::{b, c::d}` gives `a::b` and `a::c::d`), Python names joined to
/// their module (`from a import b` gives `a.b`), C includes with their quotes or
/// angle brackets, ECMAScript module specifiers without quotes.
fn import_targets(node: Node, text: &impl Fn(Node) -> String) -> Vec<String> {
    let field = |f: &str| node.child_by_field_name(f);
    let python_name = |n: Node| {
        let name = n
            .child_by_field_name("name")
            .filter(|_| n.kind() == "aliased_import");
        text(name.unwrap_or(n))
    };
    let names: Vec<_> = node
        .children_by_field_name("name", &mut node.walk())
        .collect();
    match node.kind() {
        "use_declaration" => {
            let mut out = vec![];
            use_paths(
                field("argument").expect("use has an argument"),
                "",
                text,
                &mut out,
            );
            out
        }
        "preproc_include" => vec![text(field("path").expect("include has a path"))],
        // require('x') and import('x'): the first argument, when it is a string.
        "call_expression" => field("arguments")
            .and_then(|args| args.named_child(0))
            .filter(|arg| arg.kind() == "string")
            .map(|arg| text(arg).trim_matches(['"', '\'', '`']).to_owned())
            .into_iter()
            .collect(),
        "import_statement" | "export_statement" => match field("source") {
            Some(source) => vec![text(source).trim_matches(['"', '\'', '`']).to_owned()],
            None => names.into_iter().map(python_name).collect(),
        },
        "import_from_statement" => {
            let module = text(field("module_name").expect("from has a module"));
            let sep = if module.ends_with('.') { "" } else { "." };
            if names.is_empty() {
                return vec![module]; // from a import *
            }
            names
                .into_iter()
                .map(|n| format!("{module}{sep}{}", python_name(n)))
                .collect()
        }
        other => panic!("no import targets for {other}"),
    }
}

fn use_paths(node: Node, prefix: &str, text: &impl Fn(Node) -> String, out: &mut Vec<String>) {
    let join = |p: &str, x: &str| match (p, x) {
        ("", x) => x.to_owned(),
        (p, "self") => p.to_owned(),
        (p, x) => format!("{p}::{x}"),
    };
    match node.kind() {
        "use_list" => {
            for child in node.named_children(&mut node.walk()) {
                if !child.kind().ends_with("comment") {
                    use_paths(child, prefix, text, out);
                }
            }
        }
        "scoped_use_list" => {
            let path = node.child_by_field_name("path").map_or(String::new(), text);
            let list = node
                .child_by_field_name("list")
                .expect("scoped use list has a list");
            use_paths(list, &join(prefix, &path), text, out);
        }
        "use_as_clause" => {
            use_paths(
                node.child_by_field_name("path").expect("use as has a path"),
                prefix,
                text,
                out,
            );
        }
        "use_wildcard" => {
            let path = text(node);
            out.push(join(
                prefix,
                path.trim_end_matches('*').trim_end_matches("::"),
            ));
        }
        _ => {
            let path: String = text(node).split_whitespace().collect();
            out.push(join(prefix, &path));
        }
    }
}

fn int(n: usize) -> i32 {
    i32::try_from(n).expect("count fits in i32")
}
