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
    /// A `fn`'s structural hash (node kinds, not names); a fingerprint's hash; else 0.
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
    let mut spans = BTreeMap::<_, Captured>::new();
    let mut attributes = Vec::new();
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
                "attr" => attributes.push(c.node),
                private if private.starts_with('_') => {} // query-internal, e.g. for predicates
                kind => span = span.or(Some((kind, c.node))),
            }
        }
        if let Some((kind, node)) = span {
            let key = (node.start_byte(), Reverse(node.end_byte()), kind);
            let merged = spans.entry(key).or_insert(Captured::of(node));
            merged.name = merged.name.take().or(name.map(text));
            merged.params = merged.params.or(params);
        }
    }
    // Code compiled only for tests: each test attribute's item, attributes included.
    for attr in attributes {
        let Some((gate, range)) = test_gate(attr, &text) else {
            continue;
        };
        let (kind, name) = match gate.iter().any(Vec::is_empty) {
            true => ("test", None),
            false => ("test_if", Some(dnf_text(&gate))),
        };
        let key = (range.start.0, Reverse(range.end.0), kind);
        let merged = spans.entry(key).or_insert(Captured {
            start: range.start,
            end: range.end,
            node: None,
            name: None,
            params: None,
        });
        merged.name = merged.name.take().or(name);
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
        // The structural hash, of functions only: nothing reads other spans'.
        let hash = (kind == "fn").then(|| {
            let mut hashed = lang.name.as_bytes().to_vec();
            for l in inside {
                hashed.extend(l.2.to_le_bytes());
            }
            xxh3_64(&hashed)
        });
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
            hash: hash.unwrap_or(0),
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
    out.extend(fingerprints(lang, src, &leaves));
    for ((_, _, kind), captured) in spans {
        let Captured {
            start: (start, start_point),
            end: (end, end_point),
            node,
            name,
            params,
        } = captured;
        let mut s = span(kind, start..end, start_point, end_point);
        let stack = open.entry(kind).or_default();
        while stack.last().is_some_and(|&open_end| open_end <= start) {
            stack.pop();
        }
        s.depth = int(stack.len());
        stack.push(end);
        s.name = match node.filter(|n| n.kind() == "impl_item") {
            // `impl Trait for Type`: the type, as the trait implemented for it.
            Some(n) => impl_name(n, &text),
            None => name,
        };
        s.params = params.map(|p| {
            int(p
                .named_children(&mut p.walk())
                .filter(|&n| !is_comment(n))
                .count())
        });
        if kind == "import" {
            // One span per imported path, all over the statement's bytes.
            let node = node.expect("imports are captured nodes");
            for target in import_targets(node, &text) {
                out.push(Span {
                    name: Some(target),
                    ..s.clone()
                });
            }
        } else if kind == "fn" {
            let extras = node.map(|n| function_extras(n, src, &leaves, &s));
            out.push(s);
            out.extend(extras.into_iter().flatten());
        } else {
            out.push(s);
        }
    }
    out.extend(markers(src, comments.keys().copied()));
    out
}

/// Comment markers of unfinished or fragile code: TODO, FIXME, HACK, XXX (as
/// written, upper case) and "do not touch" / "don't touch" (any case). One `marker`
/// span per comment, named by its first marker.
fn markers(src: &[u8], comments: impl Iterator<Item = (usize, usize)>) -> Vec<Span> {
    let line_starts = line_starts(src);
    let mut out = Vec::new();
    for (start, end) in comments {
        let text = String::from_utf8_lossy(&src[start..end]);
        let words: Vec<&str> = text
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let lower = text.to_lowercase();
        let found = ["TODO", "FIXME", "HACK", "XXX"]
            .into_iter()
            .find(|m| words.contains(m))
            .or_else(|| {
                ["do not touch", "don't touch", "dont touch"]
                    .into_iter()
                    .find(|m| lower.contains(m))
                    .map(|_| "DO NOT TOUCH")
            });
        if let Some(marker) = found {
            out.push(Span {
                kind: "marker",
                name: Some(marker.to_owned()),
                line: line_of(&line_starts, start),
                end_line: line_of(&line_starts, end.saturating_sub(1).max(start)),
                tokens: 0,
                comments: int(words.len()),
                params: None,
                hash: 0,
                errors: None,
                start_byte: int(start),
                end_byte: int(end),
                depth: 0,
            });
        }
    }
    out
}

/// Byte offsets at which lines start.
fn line_starts(src: &[u8]) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            src.iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i + 1),
        )
        .collect()
}

/// The 1-based line of `byte`.
fn line_of(starts: &[usize], byte: usize) -> i32 {
    int(starts.partition_point(|&s| s <= byte))
}

/// Facts about one function beyond its span, each a span over the function's bytes:
/// `forward` when its body is a single call passing on all its parameters (a
/// pass-through wrapper); `doc` when a doc comment sits right above it (or a Python
/// docstring opens it), `tokens` the comment's distinct subtokens and `comments` how
/// many of them the signature has too (redundancy); `vocabulary`, `tokens` its
/// distinct token texts (Halstead's vocabulary: operators plus operands).
fn function_extras(node: Node, src: &[u8], leaves: &[(usize, usize, u16)], f: &Span) -> Vec<Span> {
    let text = |n: Node| String::from_utf8_lossy(&src[n.byte_range()]).into_owned();
    let mut out = Vec::new();
    let with = |kind: &'static str, tokens: usize, comments: usize| Span {
        kind,
        tokens: int(tokens),
        comments: int(comments),
        name: None,
        params: None,
        ..f.clone()
    };
    let body = node.child_by_field_name("body");
    // Vocabulary: distinct texts of the code leaves.
    let (lo, hi) = (
        leaves.partition_point(|l| l.0 < f.start_byte as usize),
        leaves.partition_point(|l| l.1 <= f.end_byte as usize),
    );
    let vocabulary: std::collections::HashSet<&[u8]> =
        leaves[lo..hi].iter().map(|l| &src[l.0..l.1]).collect();
    out.push(with("vocabulary", vocabulary.len(), 0));
    // Pass-through: the body's one statement is a call whose arguments are exactly
    // the parameters, each once.
    let params = node.child_by_field_name("parameters").or_else(|| {
        node.child_by_field_name("declarator")
            .and_then(|d| d.child_by_field_name("parameters"))
    });
    if let (Some(body), Some(params)) = (body, params) {
        let mut names: Vec<String> = params
            .named_children(&mut params.walk())
            .filter(|p| !p.kind().contains("comment") && !p.kind().starts_with("self"))
            .filter(|p| !(p.kind() == "identifier" && text(*p) == "self"))
            .filter_map(|p| param_name(p).map(text))
            .collect();
        // Comments and a docstring are no statements.
        let docstring = |n: &Node| {
            n.kind() == "expression_statement"
                && n.named_child(0).is_some_and(|s| s.kind() == "string")
        };
        let statements: Vec<Node> = body
            .named_children(&mut body.walk())
            .filter(|n| !n.kind().contains("comment") && !docstring(n))
            .collect();
        let mut call = match statements.as_slice() {
            [one] => Some(*one),
            [] if body.kind().ends_with("call_expression") || body.kind() == "call" => Some(body),
            _ => None,
        };
        while let Some(n) = call {
            match n.kind() {
                "expression_statement"
                | "return_statement"
                | "return_expression"
                | "await_expression" => call = n.named_child(0),
                _ => break,
            }
        }
        if let Some(call) = call.filter(|c| c.kind() == "call_expression" || c.kind() == "call") {
            let mut args: Vec<String> = call
                .child_by_field_name("arguments")
                .map(|a| {
                    a.named_children(&mut a.walk())
                        .filter(|n| !n.kind().contains("comment"))
                        .map(text)
                        .collect()
                })
                .unwrap_or_default();
            args.sort();
            names.sort();
            if !names.is_empty() && args == names {
                out.push(with("forward", f.tokens as usize, 0));
            }
        }
    }
    // Documentation: a doc comment above the function (or above what wraps it, an
    // export or decorator), or a Python docstring.
    let mut wrapped = node;
    while let Some(p) = wrapped.parent().filter(|p| {
        matches!(
            p.kind(),
            "export_statement"
                | "decorated_definition"
                | "lexical_declaration"
                | "variable_declarator"
        )
    }) {
        wrapped = p;
    }
    let mut above = wrapped.prev_named_sibling();
    while let Some(n) = above.filter(|n| n.kind().ends_with("attribute_item")) {
        above = n.prev_named_sibling();
    }
    let doc = above
        .filter(|n| {
            n.kind().contains("comment") && n.end_position().row + 1 >= wrapped.start_position().row
        })
        .map(text)
        .filter(|t| t.starts_with("///") || t.starts_with("/**"))
        .or_else(|| {
            let first = body?.named_child(0)?;
            (first.kind() == "expression_statement")
                .then(|| first.named_child(0))
                .flatten()
                .filter(|s| s.kind() == "string")
                .map(text)
        });
    if let Some(doc) = doc {
        let comment = subtokens(&doc);
        let signature_end = body.map_or(node.end_byte(), |b| b.start_byte());
        let signature = subtokens(&String::from_utf8_lossy(
            &src[node.start_byte()..signature_end],
        ));
        let shared = comment.intersection(&signature).count();
        out.push(with("doc", comment.len(), shared));
    }
    out
}

/// The identifier a parameter binds: the parameter itself, or its pattern, name or
/// declarator.
fn param_name(p: Node) -> Option<Node> {
    if p.kind() == "identifier" {
        return Some(p);
    }
    for field in ["pattern", "name", "declarator", "left"] {
        if let Some(n) = p.child_by_field_name(field) {
            return param_name(n).or(Some(n));
        }
    }
    p.named_child(0).filter(|n| n.kind() == "identifier")
}

/// The distinct lower-case subtokens of `text`: words split at non-alphanumerics,
/// underscores and camelCase humps, of two letters or more.
fn subtokens(text: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        let mut current = String::new();
        let mut prev_lower = false;
        for c in word.chars() {
            if c.is_uppercase() && prev_lower && !current.is_empty() {
                out.insert(std::mem::take(&mut current).to_lowercase());
            }
            prev_lower = c.is_lowercase() || c.is_ascii_digit();
            current.push(c);
        }
        if !current.is_empty() {
            out.insert(current.to_lowercase());
        }
    }
    out.retain(|w| w.chars().count() >= 2 && !w.chars().all(|c| c.is_ascii_digit()));
    out
}

/// Tokens per hashed window of the clone fingerprints (k), and windows per winnowing
/// window (w). A fingerprint two places share is a run of CLONE_K (50) tokens that
/// repeats, names and literals ignored (only node kinds are hashed); every repeated
/// run of at least `CLONE_K + CLONE_W - 1` (70) tokens shares one (Schleimer,
/// Wilkerson and Aiken's winnowing).
pub const CLONE_K: usize = 50;
pub const CLONE_W: usize = 21;

/// `fingerprint` spans: the winnowed hashes of every CLONE_K-token window of the
/// code leaves, each over the bytes of its window, `tokens` CLONE_K.
fn fingerprints(lang: Lang, src: &[u8], leaves: &[(usize, usize, u16)]) -> Vec<Span> {
    // Manifests (selected by file name, no extension) are no code.
    if leaves.len() < CLONE_K || lang.exts.is_empty() {
        return vec![];
    }
    let hashes: Vec<u64> = leaves
        .windows(CLONE_K)
        .map(|w| {
            let mut bytes = lang.name.as_bytes().to_vec();
            for l in w {
                bytes.extend(l.2.to_le_bytes());
            }
            xxh3_64(&bytes)
        })
        .collect();
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(
            src.iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i + 1),
        )
        .collect();
    let line = |byte: usize| int(line_starts.partition_point(|&s| s <= byte));
    let mut picked: Vec<usize> = Vec::new();
    let w = CLONE_W.min(hashes.len());
    for start in 0..=hashes.len() - w {
        // The rightmost minimum of the window.
        let mut best = start;
        for i in start..start + w {
            if hashes[i] <= hashes[best] {
                best = i;
            }
        }
        if picked.last() != Some(&best) {
            picked.push(best);
        }
    }
    picked
        .into_iter()
        .map(|i| {
            let (start, end) = (leaves[i].0, leaves[i + CLONE_K - 1].1);
            Span {
                kind: "fingerprint",
                name: None,
                line: line(start),
                end_line: line(end.saturating_sub(1).max(start)),
                tokens: int(CLONE_K),
                comments: 0,
                params: None,
                hash: hashes[i],
                errors: None,
                start_byte: int(start),
                end_byte: int(end),
                depth: 0,
            }
        })
        .collect()
}

/// When code is test code: a disjunction of conjunctions of feature names.
type Gate = Vec<Vec<String>>;

/// The name of a Rust impl block: `Type`, or `<Type as Trait>` for a trait's.
fn impl_name(node: Node, text: &impl Fn(Node) -> String) -> Option<String> {
    let squash = |n: Node| text(n).split_whitespace().collect::<Vec<_>>().join(" ");
    let ty = squash(node.child_by_field_name("type")?);
    Some(match node.child_by_field_name("trait") {
        Some(t) => format!("<{ty} as {}>", squash(t)),
        None => ty,
    })
}

/// A span to emit: its start and end (byte and point), the node it was captured
/// as (none for test spans, which start at an attribute and end with its item),
/// its name and its parameter list.
struct Captured<'tree> {
    start: (usize, Point),
    end: (usize, Point),
    node: Option<Node<'tree>>,
    name: Option<String>,
    params: Option<Node<'tree>>,
}

impl<'tree> Captured<'tree> {
    fn of(node: Node<'tree>) -> Self {
        Captured {
            start: (node.start_byte(), node.start_position()),
            end: (node.end_byte(), node.end_position()),
            node: Some(node),
            name: None,
            params: None,
        }
    }
}

/// When the code a Rust attribute applies to is compiled only for tests, as a
/// disjunction of conjunctions of feature names (an empty conjunction: always test
/// code), and that code's range. Test attributes are `#[test]`-style attributes
/// (`#[test]`, `#[tokio::test]`, `#[bench]`, `#[rstest]`, ...) and `#[cfg(P)]` where P
/// is false whenever `test` is: `test`, `all(test, ..)`, or a feature that the SQL
/// then decides is test-only (`test_feature` in helpers.sql, or declared in
/// .gitattributes). An outer attribute applies to the next item, a sibling in the
/// syntax tree; an inner one (`#![cfg(test)]`) to the item or file containing it.
fn test_gate(attr: Node, text: &impl Fn(Node) -> String) -> Option<(Gate, Range<(usize, Point)>)> {
    let attribute = attr.named_child(0).filter(|a| a.kind() == "attribute")?;
    let path = text(attribute.named_child(0)?);
    let last = path.rsplit("::").next().unwrap_or_default();
    let gate = if path == "cfg" {
        let arguments = text(attribute.child_by_field_name("arguments")?);
        let mut tokens = cfg_tokens(&arguments).into_iter().peekable();
        if tokens.next().as_deref() != Some("(") {
            return None;
        }
        cfg_predicate(&mut tokens)?
    } else if [
        "test",
        "bench",
        "rstest",
        "quickcheck",
        "proptest",
        "test_case",
    ]
    .contains(&last)
        || last.ends_with("_test")
    {
        vec![vec![]]
    } else {
        return None;
    };
    if gate.is_empty() {
        return None;
    }
    let range = if attr.kind() == "inner_attribute_item" {
        let parent = attr.parent()?;
        let item = match parent.kind() {
            "source_file" => parent,
            _ => parent
                .parent()
                .filter(|p| p.kind() != "source_file")
                .unwrap_or(parent),
        };
        let start = match item.kind() {
            "source_file" => (0, Point::new(0, 0)),
            _ => (item.start_byte(), item.start_position()),
        };
        start..(item.end_byte(), item.end_position())
    } else {
        let mut item = attr.next_named_sibling()?;
        while item.kind().ends_with("attribute_item") || item.kind().ends_with("comment") {
            item = item.next_named_sibling()?;
        }
        (attr.start_byte(), attr.start_position())..(item.end_byte(), item.end_position())
    };
    Some((gate, range))
}

/// The tokens of a `cfg` predicate: identifiers, string literals, `(`, `)`, `,`, `=`.
fn cfg_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => s.extend(chars.next()),
                        c => s.push(c),
                    }
                }
                tokens.push(format!("\"{s}"));
            }
            '(' | ')' | ',' | '=' => tokens.push(c.to_string()),
            c if c.is_whitespace() => {}
            c => {
                let mut word = c.to_string();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || "()=,\"".contains(c) {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                tokens.push(word);
            }
        }
    }
    tokens
}

/// Most conjunctions a test gate keeps; a larger one counts as production code.
const MAX_GATE_TERMS: usize = 16;

/// One `cfg` predicate, as the condition on test-only features under which it is
/// false without `test`: a disjunction of conjunctions, empty when it can be true in
/// a production build whatever the features. Consumes the predicate and the `,` or
/// `)` after it; `None` when the predicate does not parse.
fn cfg_predicate(tokens: &mut std::iter::Peekable<impl Iterator<Item = String>>) -> Option<Gate> {
    let word = tokens.next()?;
    let gate = match tokens.peek().map(String::as_str) {
        Some("(") => {
            tokens.next();
            let mut parts = Vec::new();
            while tokens.peek().map(String::as_str) != Some(")") {
                parts.push(cfg_predicate(tokens)?);
                if tokens.peek().map(String::as_str) == Some(",") {
                    tokens.next();
                }
            }
            tokens.next();
            match word.as_str() {
                // all(..) is test-only when any part is.
                "all" => parts.into_iter().flatten().collect(),
                // any(..) is test-only when every part is.
                "any" if !parts.is_empty() => {
                    parts.into_iter().try_fold(vec![vec![]], |acc, part| {
                        let product: Vec<Vec<String>> = acc
                            .iter()
                            .flat_map(|a| part.iter().map(move |p| [a.clone(), p.clone()].concat()))
                            .collect();
                        (product.len() <= MAX_GATE_TERMS).then_some(product)
                    })?
                }
                // not(..) and unknown predicates: assume a production build can satisfy them.
                _ => vec![],
            }
        }
        Some("=") => {
            tokens.next();
            let value = tokens.next()?;
            match (word.as_str(), value.strip_prefix('"')) {
                ("feature", Some(feature)) => vec![vec![feature.to_owned()]],
                _ => vec![],
            }
        }
        _ if word == "test" => vec![vec![]],
        _ => vec![],
    };
    let mut gate: Gate = gate
        .into_iter()
        .map(|mut c| {
            c.sort();
            c.dedup();
            c
        })
        .collect();
    gate.sort();
    gate.dedup();
    if gate.iter().any(Vec::is_empty) {
        gate = vec![vec![]];
    }
    (gate.len() <= MAX_GATE_TERMS).then_some(gate)
}

/// A test gate as `test_if` spans name it: conjunctions joined by `|`, their features
/// by `&` (characters no cargo feature name contains).
fn dnf_text(gate: &[Vec<String>]) -> String {
    gate.iter()
        .map(|c| c.join("&"))
        .collect::<Vec<_>>()
        .join("|")
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
