//! The languages tally parses: how a path selects one, its tree-sitter grammar, and
//! the query that captures its facts.

use std::sync::OnceLock;

use tree_sitter::{Language, Query};

/// A language: a name, the file names and extensions that select it, a grammar and
/// a query file. Manifests (Cargo.toml, package.json) are languages too: their query
/// captures the package name, and they never count as code.
#[derive(Clone, Copy)]
pub struct Lang {
    pub name: &'static str,
    pub files: &'static [&'static str],
    pub exts: &'static [&'static str],
    pub(crate) grammar: fn() -> Language,
    query: &'static str,
}

/// Every span kind a query may capture, besides `file`, which every blob gets. The
/// helper captures `@comment`, `@name`, `@params` and `@_*` (for predicates) create
/// no span. A query capturing anything else fails to compile, so a typo cannot
/// silently create a kind no SQL reads.
pub const KINDS: &[&str] = &[
    "fn",
    "call",
    "import",
    "module",
    "pub",
    "static",
    "branch",
    "condition",
    "block",
    "global_state",
    "throw",
    "goto",
    "package",
    "alias",
    "alias_target",
    "base_url",
    // Not captured by a query: tally marks a blob of generated code with it.
    "generated",
];

pub const LANGS: &[Lang] = &[
    Lang {
        name: "rust",
        files: &[],
        exts: &["rs"],
        grammar: || tree_sitter_rust::LANGUAGE.into(),
        query: include_str!("../langs/rust.scm"),
    },
    Lang {
        name: "c",
        files: &[],
        exts: &["c"],
        grammar: || tree_sitter_c::LANGUAGE.into(),
        query: include_str!("../langs/c.scm"),
    },
    Lang {
        name: "cpp",
        files: &[],
        exts: &["h", "cc", "cpp", "cxx", "hh", "hpp", "hxx"],
        grammar: || tree_sitter_cpp::LANGUAGE.into(),
        query: include_str!("../langs/cpp.scm"),
    },
    Lang {
        name: "python",
        files: &[],
        exts: &["py"],
        grammar: || tree_sitter_python::LANGUAGE.into(),
        query: include_str!("../langs/python.scm"),
    },
    Lang {
        name: "typescript",
        files: &[],
        exts: &["ts", "mts", "cts"],
        grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        query: include_str!("../langs/ecma.scm"),
    },
    Lang {
        name: "tsx",
        files: &[],
        exts: &["tsx"],
        grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        query: include_str!("../langs/ecma.scm"),
    },
    Lang {
        name: "javascript",
        files: &[],
        exts: &["js", "mjs", "cjs", "jsx"],
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        query: include_str!("../langs/ecma.scm"),
    },
    Lang {
        name: "cargo",
        files: &["Cargo.toml"],
        exts: &[],
        grammar: || tree_sitter_toml_ng::LANGUAGE.into(),
        query: include_str!("../langs/cargo.scm"),
    },
    Lang {
        name: "tsconfig",
        files: &["tsconfig.json", "jsconfig.json", "tsconfig.base.json"],
        exts: &[],
        grammar: || tree_sitter_json::LANGUAGE.into(),
        query: include_str!("../langs/tsconfig.scm"),
    },
    Lang {
        name: "npm",
        files: &["package.json"],
        exts: &[],
        grammar: || tree_sitter_json::LANGUAGE.into(),
        query: include_str!("../langs/npm.scm"),
    },
];

impl Lang {
    /// The language selected by the file name of `path`, else by its lowercased
    /// extension.
    pub fn of(path: &str) -> Option<Lang> {
        let path = std::path::Path::new(path);
        let name = path.file_name()?.to_str()?;
        if let Some(lang) = LANGS.iter().find(|l| l.files.contains(&name)) {
            return Some(*lang);
        }
        let ext = path.extension()?.to_str()?.to_lowercase();
        LANGS
            .iter()
            .copied()
            .find(|l| l.exts.contains(&ext.as_str()))
    }

    /// The compiled query, memoized: compiling costs up to 13 ms (C++), far more
    /// than parsing a typical file. It depends only on `LANGS`, so `extract` stays pure.
    pub(crate) fn compiled_query(self) -> &'static Query {
        static QUERIES: OnceLock<Vec<Query>> = OnceLock::new();
        let queries = QUERIES.get_or_init(|| {
            let compile = |l: &Lang| {
                let query = Query::new(&(l.grammar)(), l.query).expect("query compiles");
                for capture in query.capture_names() {
                    assert!(
                        ["comment", "name", "params"].contains(capture)
                            || capture.starts_with('_')
                            || KINDS.contains(capture),
                        "the {} query captures @{capture}, which is not in KINDS",
                        l.name
                    );
                }
                query
            };
            LANGS.iter().map(compile).collect()
        });
        &queries[LANGS
            .iter()
            .position(|l| l.name == self.name)
            .expect("lang is in LANGS")]
    }
}
