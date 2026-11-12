//! Offline check for item 9: Cognitive Complexity (SonarSource, 2017-2023 spec)
//! per production function at a repository's tip, beside decisions + nesting.
//! Usage: cognitive_offline REPO_DIR UNITS_JSON  (prints CSV)

use tree_sitter::{Node, Parser};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (repo, units) = (&args[1], &args[2]);
    let db = duckdb::Connection::open_in_memory().unwrap();
    let mut stmt = db
        .prepare(&format!(
            "select f.path, f.lang, f.start_byte, f.end_byte, f.decisions, f.nesting, f.tokens, f.function
             from (select unnest(functions) as f from read_json('{units}', maximum_object_size=100000000))"
        ))
        .unwrap();
    let rows: Vec<(String, String, i64, i64, i64, i64, i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    println!("function,cc,decisions,nesting,tokens");
    let mut cache: std::collections::HashMap<String, (Vec<u8>, tree_sitter::Tree)> = Default::default();
    for (path, lang, start, end, decisions, nesting, tokens, function) in rows {
        let (src, tree) = cache.entry(path.clone()).or_insert_with(|| {
            let src = std::fs::read(format!("{repo}/{path}")).unwrap();
            let mut p = Parser::new();
            let language: tree_sitter::Language = match lang.as_str() {
                "rust" => tree_sitter_rust::LANGUAGE.into(),
                "python" => tree_sitter_python::LANGUAGE.into(),
                l => panic!("{l}"),
            };
            p.set_language(&language).unwrap();
            let t = p.parse(&src, None).unwrap();
            (src, t)
        });
        let node = tree
            .root_node()
            .descendant_for_byte_range(start as usize, end as usize)
            .unwrap();
        let mut node = node;
        while (node.start_byte(), node.end_byte()) != (start as usize, end as usize) {
            node = node.parent().unwrap();
        }
        let cc = cognitive(node, &lang, src);
        println!("\"{function}\",{cc},{decisions},{nesting},{tokens}");
    }
}

/// Cognitive Complexity of the function `f`: its body's increments.
fn cognitive(f: Node, lang: &str, src: &[u8]) -> u32 {
    let body = f.child_by_field_name("body").unwrap();
    let mut total = 0;
    walk(body, 0, lang, src, &mut total);
    total
}

fn is_logical(n: Node, lang: &str, src: &[u8]) -> Option<String> {
    match (lang, n.kind()) {
        ("rust", "binary_expression") => {
            let op = n.child_by_field_name("operator")?.utf8_text(src).ok()?.to_owned();
            (op == "&&" || op == "||").then_some(op)
        }
        ("python", "boolean_operator") => {
            Some(n.child_by_field_name("operator")?.utf8_text(src).ok()?.to_owned())
        }
        _ => None,
    }
}

/// Adds the increments of `n` and its descendants at nesting level `level`.
fn walk(n: Node, level: u32, lang: &str, src: &[u8], total: &mut u32) {
    let kind = n.kind();
    // Structural increments (+1, plus nesting) and nesting-raising constructs.
    let (structural, nests) = match (lang, kind) {
        ("rust", "if_expression") => {
            // `else if` is an if whose parent is an else_clause: +1, no nesting increment.
            let else_if = n.parent().is_some_and(|p| p.kind() == "else_clause");
            *total += if else_if { 1 } else { 1 + level };
            // Condition at this level; consequence one deeper; else handled below.
            let lvl = if else_if { level } else { level };
            for c in children(n) {
                match c.kind() {
                    "else_clause" => {
                        // `else` (not `else if`): +1
                        let inner = children(c).into_iter().find(|x| x.is_named());
                        if inner.is_some_and(|x| x.kind() != "if_expression") {
                            *total += 1;
                        }
                        for x in children(c) {
                            if x.kind() == "if_expression" {
                                walk(x, lvl, lang, src, total);
                            } else {
                                walk(x, lvl + 1, lang, src, total);
                            }
                        }
                    }
                    "block" => walk(c, lvl + 1, lang, src, total),
                    _ => walk(c, lvl, lang, src, total),
                }
            }
            return;
        }
        ("rust", "match_expression" | "for_expression" | "while_expression" | "loop_expression") => (true, true),
        ("rust", "closure_expression" | "function_item") => (false, true),
        ("rust", "break_expression" | "continue_expression") => {
            if children(n).iter().any(|c| c.kind() == "label") {
                *total += 1;
            }
            (false, false)
        }
        ("python", "if_statement") => {
            *total += 1 + level;
            for c in children(n) {
                match c.kind() {
                    "elif_clause" => {
                        *total += 1;
                        for x in children(c) {
                            let deeper = x.kind() == "block";
                            walk(x, level + u32::from(deeper), lang, src, total);
                        }
                    }
                    "else_clause" => {
                        *total += 1;
                        for x in children(c) {
                            walk(x, level + 1, lang, src, total);
                        }
                    }
                    "block" => walk(c, level + 1, lang, src, total),
                    _ => walk(c, level, lang, src, total),
                }
            }
            return;
        }
        ("python", "for_statement" | "while_statement" | "except_clause" | "conditional_expression" | "match_statement") => (true, true),
        ("python", "else_clause") => {
            // else of for/while/try: +1
            *total += 1;
            (false, false)
        }
        ("python", "lambda" | "function_definition") => (false, true),
        _ => (false, false),
    };
    if structural {
        *total += 1 + level;
    }
    if let Some(op) = is_logical(n, lang, src) {
        // A new sequence of like operators: the parent is not the same operator.
        let same_parent = n.parent().and_then(|p| is_logical(p, lang, src)).is_some_and(|p| p == op);
        if !same_parent {
            *total += 1;
        }
    }
    let next = level + u32::from(nests);
    for c in children(n) {
        // The condition/iterable of a structure stays at its level; bodies nest.
        let body = matches!(c.kind(), "block" | "match_block") || !structural;
        walk(c, if body { next } else { level }, lang, src, total);
    }
}

fn children(n: Node) -> Vec<Node> {
    let mut c = n.walk();
    n.children(&mut c).collect()
}
