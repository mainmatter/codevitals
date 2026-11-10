# tally: implementation spec

Revision 2: commit authors and messages, private captures, call and visibility
facts, and built-in metrics as SQL views (`src/metrics.sql`). Parallel parsing,
logging and the `metrics` subcommand replace the MVP's `sql` subcommand.

You are implementing `tally`, a single Rust CLI that records facts about a git
repository's history into a DuckDB file. Read this whole document before
writing code. It is the complete brief; nothing outside it is in scope.

## 1. What tally is

tally records two kinds of facts:

1. **What changed in each commit** along the first-parent history of one branch.
2. **What each blob contains**: a pure function of file content, parsed once
   per distinct blob with tree-sitter.

Every metric, aggregate and trend is SQL over those facts. The built-in metrics
are views in `src/metrics.sql`, which the binary applies at startup (3.1).

### Design rules

These rules decide every question this spec leaves open.

- **Every concept is a liability.** Add no table, column, flag, subcommand,
  config file, trait, or dependency that this spec does not name.
- **No metrics in Rust.** If something can be computed in SQL from stored
  facts, it must not be computed in Rust.
- **Append-only writes.** The only deletion is the version purge in section 5.2.
- **Idempotent and resumable.** Running `scan` twice changes nothing. An
  interrupted `scan`, rerun, ends in the same state as an uninterrupted one.
- **No hand-rolled parsing or hashing.** Use the named crates.
- **If the spec is impossible as written, stop and report.** Do not redesign.

### Non-goals

Do not build any of these: metric or trend computation in Rust, a config
file, path include/exclude options, progress bars, SCIP or any semantic analysis, PR gates or a `check` command,
LLM calls, extra C/C++ preprocessor grammars, rename detection, branches other
than the one first-parent chain.

## 2. CLI

Use `clap` with derive.

```
tally scan    [--repo <PATH>] [--db <PATH>] [REV]
tally metrics [--db <PATH>] [METRIC]...
```

| Argument | Default | Meaning |
|---|---|---|
| `--repo` | `.` | Path to a git repository |
| `--db` | `tally.duckdb` | DuckDB database file; created if missing |
| `REV` | `HEAD` | Revision whose first-parent history is scanned |
| `METRIC` | all | Metric views to export (3.1); `--help` lists them with their explanations |

- `scan` runs the walk (5.1), then the parse (5.2). On success it prints one
  line to stderr: `scanned <N> commits, <M> changes; parsed <K> blobs`.
- `metrics` prints one JSON object to stdout mapping each selected metric view
  to an array of row objects. Each row carries the commit `time` (ISO 8601 UTC,
  `2026-01-01T00:00:00Z`) after `seq`. An empty metric is `[]`. The JSON is
  built by DuckDB; ad-hoc queries use the `duckdb` CLI on the database file.
- Logs go to stderr through `tracing`, filtered by `RUST_LOG` (default `info`:
  phases and progress; `debug`: one line per commit and per blob, the blob
  logged before it is parsed).
- Errors go to stderr with a non-zero exit code.

## 3. Storage

Run this on every start, then `src/metrics.sql` (3.1). It is the whole schema.

```sql
create table if not exists commits (
  seq integer, id text, time timestamp, author text, message text);
create table if not exists changes (seq integer, path text, blob text);
create table if not exists spans (
  blob text, v integer, lang text, kind text, name text,
  line integer, end_line integer, tokens integer, comments integer,
  params integer, hash ubigint, errors integer);

create or replace view files as
select path, blob, since, until from (
  select path, blob, seq as since,
         coalesce(lead(seq) over (partition by path order by seq), 2147483647) as until
  from changes
) where blob is not null;
```

### `commits`: one row per scanned commit

| Column | Meaning |
|---|---|
| `seq` | Position on the first-parent chain, oldest = 0, no gaps |
| `id` | Commit id, 40 hex characters |
| `time` | Committer time in UTC, as `timestamp` (not `timestamptz`) |
| `author` | Author as `Name <email>`, lossy UTF-8 |
| `message` | The full raw commit message, so trailers such as `Co-authored-by` are available |

A database whose `commits` table lacks these columns predates revision 2:
opening it fails with a message to delete it and scan again.

### `changes`: one row per file whose blob changed in a commit

| Column | Meaning |
|---|---|
| `seq` | The commit that made the change |
| `path` | Repo-relative path, `/` separators, lossy UTF-8 |
| `blob` | Blob id (40 hex) at `path` after the commit; `NULL` if no blob is there |

### `spans`: parsed facts, keyed by blob

Columns are defined in section 6. A blob has spans if and only if it was
processed at the current extractor version.

### `files`: view

A file version `(path, blob)` is live at commit `s` when
`since <= s and s < until`. This view is the only way snapshot queries
reconstruct a tree.

### 3.1 Metrics

`src/metrics.sql` is embedded with `include_str!` and applied after the schema
on every start, so every database carries the current definitions and any
DuckDB client can read them. It defines:

- Helpers: the macros `module(path, depth)` (a path's directory, cut at
  `depth`) and `live()` (the files live at each commit), and the views
  `commit_info` (adds `subject`, `is_fix` from a keyword match on the subject,
  `is_agent` from a co-author trailer or author pattern), `fns` (one row per
  function with a syntactic `public` flag: Rust `pub`, JS/TS `export`, C/C++
  not `static`, Python no leading underscore) and `call_edges` (a heuristic
  call graph per commit: a call resolves only when its name matches exactly
  one function defined in the repository at that commit).
- Metrics: every view named `m_<name>`. Each has a `seq` column (the commit,
  or the first commit of its window) and a `value` column, plus its components.
  `comment on view` stores what it measures, which direction is better and the
  evidence behind it.

The rule "no metrics in Rust" still holds: Rust only applies the file and
exports the views it finds.

## 4. Constants

```rust
const V: i32 = 2;                    // extractor version
const MAX_BLOB_BYTES: usize = 1 << 20;
```

Bump `V` whenever `extract` output could change for any input: a rule change,
a query file change, or a grammar crate version change.

## 5. `scan`

### 5.1 Walk

Purpose: extend `commits` and `changes`. The walk never reads blob contents.

1. Read the last stored commit: the row of `commits` with the greatest `seq`.
2. Resolve `REV` to a commit. Collect its first-parent chain, newest first,
   stopping when you reach the last stored commit id or run out of parents.
3. If `commits` is non-empty and the last stored id was not reached, fail with:
   `history diverged from the stored chain; reset the walk with: duckdb <db> "delete from commits; delete from changes" (parsed spans are kept), or delete the database file, then scan again`.
   Do not touch `spans`.
4. Process the new commits oldest first. The first new commit gets
   `seq = last stored seq + 1`, or `0` for an empty database.
5. For each commit, compare its tree with the previous commit's tree on the
   chain. For the first commit of an empty database, compare with the empty
   tree.

Define `B(tree, path)` as the blob id at `path` when the entry is a regular or
executable file, and *none* otherwise (symlinks, submodules, trees, absent).
For every `path` where `B(prev, path) != B(cur, path)`, append
`(seq, path, B(cur, path))` to `changes`, with *none* stored as `NULL`.

Consequences to preserve:

- A rename appears as one `NULL` row for the old path and one row for the new
  path. Do not enable rename detection.
- A mode-only change produces no row.
- A merge commit is compared with its first parent only.

6. Insert the `commits` row and that commit's `changes` rows atomically. One
   transaction may cover several commits, but never part of one.

Prefer the git library's tree-to-tree diff, which skips unchanged subtrees.
If it cannot express the rule above, list both trees fully and compare.

### 5.2 Parse

Purpose: make `spans` complete for every blob in `changes` that has a known
language.

1. `delete from spans where v <> V`.
2. List blobs to process:

```sql
select blob, min(path) as path from changes
where blob is not null and blob not in (select blob from spans)
group by blob;
```

3. For each `(blob, path)`:
   - Pick the language from the lowercased extension of `path` (section 7).
     Unknown extension: skip, write nothing.
   - Read the blob. If it is larger than `MAX_BLOB_BYTES`, write exactly one
     row `(blob, V, lang, 'file', NULL, 1, NULL, NULL, NULL, NULL, NULL, NULL)`.
   - Otherwise call `extract(lang, bytes)` and write one row per returned span,
     with `blob`, `V` and `lang` filled in.
4. All rows of one blob are written atomically. One transaction may cover
   several blobs, but never part of one.

Blobs may be read and extracted in parallel (`rayon`), with one writer
appending whole blobs as they arrive.

Because the two loops are independent, bumping `V` or adding a language
backfills the whole history through 5.2 alone. Keep that property.

## 6. `extract`

```rust
pub fn extract(lang: Lang, src: &[u8]) -> Vec<Span>
```

`extract` is pure: no I/O, same input gives same output. The only global is
the memoized compiled query per language, which depends on nothing but section 7. It parses
`src` with the language's grammar, runs the language's query file, and returns
spans. It never fails; tree-sitter always returns a tree.

### 6.1 Captures

A query file may use any capture names. Three are special:

| Capture | Meaning |
|---|---|
| `@comment` | The node is a comment |
| `@name` | The name of the span captured in the same match |
| `@params` | The parameter list of the span captured in the same match |

A capture whose name starts with `_` is private to the query: it exists for
predicates such as `(#eq? @_s "static")` and creates no span.

**Any other capture name creates a span whose `kind` is the capture name.**
`@fn` and `@import` are ordinary span kinds with no special handling. This is
the extension point for language-specific facts: adding `(unsafe_block) @unsafe`
to a query file must work with no Rust change.

Rules:

- Each match contributes at most one span: the first non-special capture.
- A span's identity is `(kind, start byte, end byte)`. If several matches
  capture the same span, merge them: the first non-null `@name` and the first
  non-null `@params` win.
- Every blob also gets exactly one span of kind `file` covering the whole source.

### 6.2 Span columns

| Column | Definition |
|---|---|
| `kind` | `file` or the capture name |
| `name` | Text of the `@name` node, lossy UTF-8; `NULL` if none; always `NULL` for `file` |
| `line` | 1-based line of the span's first byte; `1` for `file` |
| `end_line` | 1-based line of the span's last byte. If the node ends at column 0, use the previous line. Never less than `line`. |
| `tokens` | Number of code leaves inside the span |
| `comments` | Number of whitespace-separated words in `@comment` nodes inside the span |
| `params` | Named children of the `@params` node that are not `@comment` nodes; `NULL` if no `@params` |
| `hash` | See 6.3 |
| `errors` | For `file`: number of nodes where `is_error()` or `is_missing()` is true. `NULL` for other kinds. |

A **code leaf** is a node with no children and non-zero byte length that is
not inside a `@comment` node. Named and anonymous leaves both count.

"Inside" means byte-range containment. A leaf or comment counts toward every
span that contains it, so a nested function also counts toward its parent, and
everything counts toward `file`.

### 6.3 Hash

`hash` is XXH3-64 (crate `xxhash-rust`) over this byte string:

```
lang name as ASCII bytes  ++  for each code leaf in the span, in source order: kind_id as u16 little-endian
```

`lang name` is the name from section 7 (for example `rust`). Identifiers and
literals share a node kind, so two functions that differ only in names and
literal values get the same hash. Compute it for every span, including `file`.

### 6.4 Worked example

Input, language `rust`:

```rust
use std::collections::HashMap;

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
```

Expected spans, with `tree-sitter-rust` 0.24.2:

| kind | name | line | end_line | tokens | comments | params | errors |
|---|---|---|---|---|---|---|---|
| file | NULL | 1 | 15 | 60 | 7 | NULL | 0 |
| import | `std::collections::HashMap` | 1 | 1 | 7 | 0 | NULL | NULL |
| fn | `add` | 4 | 7 | 18 | 3 | 2 | NULL |
| fn | `get` | 10 | 10 | 13 | 0 | 1 | NULL |
| fn | `plus` | 13 | 15 | 18 | 0 | 2 | NULL |

Also required: `hash(add) == hash(plus)`, and `hash(get)` differs from both.
The doc comment above `add` lies outside the function node, so it counts
toward `file` (7 words) but not toward `add` (3 words).

## 7. Languages

One static table maps extensions to a language. A language is a name, a
grammar, and a query file embedded with `include_str!`.

| Name | Extensions | Grammar crate | Query file |
|---|---|---|---|
| `rust` | `rs` | `tree-sitter-rust` | `langs/rust.scm` |
| `c` | `c` | `tree-sitter-c` | `langs/c.scm` |
| `cpp` | `h` `cc` `cpp` `cxx` `hh` `hpp` `hxx` | `tree-sitter-cpp` | `langs/cpp.scm` |
| `python` | `py` | `tree-sitter-python` | `langs/python.scm` |
| `typescript` | `ts` `mts` `cts` | `tree-sitter-typescript` | `langs/ecma.scm` |
| `tsx` | `tsx` | `tree-sitter-typescript` | `langs/ecma.scm` |
| `javascript` | `js` `mjs` `cjs` `jsx` | `tree-sitter-javascript` | `langs/ecma.scm` |

`.h` maps to `cpp` on purpose: the C++ grammar parses C headers acceptably,
and the reverse is not true.

Adding a language must require only: one grammar dependency, one row in this
table, one query file, one golden test.

### Query files

Use these verbatim. They were validated against the grammar versions below by
compiling each query and checking the output on sample code.

`langs/rust.scm`

```scheme
(function_item name: (identifier) @name parameters: (parameters) @params) @fn
[(line_comment) (block_comment)] @comment
(use_declaration argument: (_) @name) @import
(call_expression function: [(identifier) @name (scoped_identifier name: (identifier) @name) (field_expression field: (field_identifier) @name) (generic_function function: [(identifier) @name (scoped_identifier name: (identifier) @name) (field_expression field: (field_identifier) @name)])]) @call
(function_item (visibility_modifier)) @pub
```

`langs/c.scm`

```scheme
(function_definition) @fn
(function_definition declarator: (function_declarator declarator: (identifier) @name parameters: (parameter_list) @params)) @fn
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name parameters: (parameter_list) @params))) @fn
(comment) @comment
(preproc_include path: (_) @name) @import
(call_expression function: (identifier) @name) @call
(function_definition (storage_class_specifier) @_s (#eq? @_s "static")) @static
```

`langs/cpp.scm`

```scheme
(function_definition) @fn
(function_definition declarator: (function_declarator declarator: (_) @name parameters: (parameter_list) @params)) @fn
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (_) @name parameters: (parameter_list) @params))) @fn
(function_definition declarator: (reference_declarator (function_declarator declarator: (_) @name parameters: (parameter_list) @params))) @fn
(comment) @comment
(preproc_include path: (_) @name) @import
(call_expression function: [(identifier) @name (qualified_identifier name: (identifier) @name) (field_expression field: (field_identifier) @name)]) @call
(function_definition (storage_class_specifier) @_s (#eq? @_s "static")) @static
```

`langs/python.scm`

```scheme
(function_definition name: (identifier) @name parameters: (parameters) @params) @fn
(comment) @comment
(import_statement name: (_) @name) @import
(import_from_statement module_name: (_) @name) @import
(call function: [(identifier) @name (attribute attribute: (identifier) @name)]) @call
```

Python visibility comes from the leading underscore, in SQL (`fns`).

`langs/ecma.scm` (shared by `typescript`, `tsx`, `javascript`)

```scheme
(function_declaration name: (identifier) @name parameters: (formal_parameters) @params) @fn
(generator_function_declaration name: (identifier) @name parameters: (formal_parameters) @params) @fn
(method_definition name: (_) @name parameters: (formal_parameters) @params) @fn
(variable_declarator name: (identifier) @name value: (arrow_function)) @fn
(variable_declarator name: (identifier) @name value: (arrow_function parameters: (formal_parameters) @params)) @fn
(variable_declarator name: (identifier) @name value: (function_expression parameters: (formal_parameters) @params)) @fn
(comment) @comment
(import_statement source: (string) @name) @import
(call_expression function: [(identifier) @name (member_expression property: (property_identifier) @name)]) @call
(export_statement declaration: (function_declaration) @pub)
(export_statement declaration: (generator_function_declaration) @pub)
(export_statement declaration: (lexical_declaration (variable_declarator value: [(arrow_function) (function_expression)]) @pub))
```

`@call` spans dominate the table (about four in five rows). Each span finds its
leaves by binary search over the ordered leaves, never by scanning all spans
per leaf.

Validated with: `tree-sitter` 0.26.1, `tree-sitter-rust` 0.24.2,
`tree-sitter-c` 0.24.2, `tree-sitter-cpp` 0.23.4, `tree-sitter-python` 0.25.0,
`tree-sitter-typescript` 0.23.2, `tree-sitter-javascript` 0.25.0. Pin the Rust
crates to these versions if they exist; otherwise use the nearest and let the
tests in section 9 tell you what changed.

### Accepted quirks

These are known and intended for the MVP. Do not fix them.

- C/C++ `f(void)` reports `params = 1`.
- A JS/TS arrow function with one bare parameter (`x => x`) reports `params = NULL`.
- Anonymous functions passed as arguments are not spans; their tokens count
  toward the enclosing span.
- Python docstrings are strings, not comments.
- `import a, b` in Python yields one import span named `a`.
- C/C++ macros that expand to syntax cause local misparses. They show up in
  `errors`; they never fail a run.
- A blob reachable under two extensions is parsed once, by `min(path)`.
- Visibility is syntactic: a Rust `pub` (including `pub(crate)`) inside a
  private module counts as public.
- Call resolution in `call_edges` is by name only; most calls do not resolve.

## 8. Project

```
Cargo.toml
src/main.rs      clap CLI, schema setup, walk, parse loop
src/extract.rs   Lang table, Span, extract
src/metrics.rs   metric catalog and JSON export
src/schema.sql   section 3, embedded with include_str!
src/metrics.sql  section 3.1, embedded with include_str!
langs/*.scm      section 7
grafana/         Infinity data source and dashboard provisioning
flake.nix        dev shell: toolchain, duckdb CLI, `tally-grafana`
tests/           section 9
README.md        section 10
```

Dependencies, and no others without a stated reason:

| Crate | Use |
|---|---|
| `clap` (derive, string) | CLI; `string` lets `--help` list metrics read from the catalog |
| `gix` | Repository access, first-parent walk, tree diff, blob read |
| `tree-sitter` + six grammar crates | Parsing and queries |
| `duckdb` (features `bundled`, `json`) | Storage, SQL and the JSON export |
| `rayon` | Parallel blob extraction |
| `tracing`, `tracing-subscriber` (env-filter) | Logging |
| `xxhash-rust` (feature `xxh3`) | Span hash |
| `anyhow` | Error propagation |

Dev-dependencies: `tempfile`; `insta` is allowed for golden tests.

Rust stable, current edition. `cargo clippy -- -D warnings` and `cargo fmt
--check` must pass. Target size is about 550 lines of Rust; treat a much larger
result as a sign that something out of scope crept in.

## 9. Tests

All tests must pass with `cargo test`. Tests may shell out to `git`.

### 9.1 Queries compile

For every language in the table, the query compiles against its grammar.

### 9.2 `extract` golden tests

- The worked example in 6.4, exactly.
- At least one sample per language covering: a named function with
  parameters, a comment, an import, and for C a pointer-returning function.
- Empty input returns exactly one `file` span with `line = 1`, `end_line = 1`,
  `tokens = 0`, `errors = 0`.
- Broken input (for example `int f( {{{ ` as C) returns a `file` span with
  `errors >= 1` and does not panic.
- Purity: calling `extract` twice on the same input returns equal output.

### 9.3 Fixture repository

Build this repository in a temp directory at test time:

```bash
git init -q -b main . && git config user.email t@t && git config user.name t
export GIT_AUTHOR_DATE="2026-01-01T00:00:00Z" GIT_COMMITTER_DATE="2026-01-01T00:00:00Z"
printf '# readme\n' > README.md
printf 'fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n' > a.rs
git add -A && git commit -qm c0
export GIT_AUTHOR_DATE="2026-01-02T00:00:00Z" GIT_COMMITTER_DATE="2026-01-02T00:00:00Z"
printf 'fn add(a: i32, b: i32) -> i32 {\n    // sum\n    a + b\n}\n' > a.rs
mkdir src && printf 'fn plus(x: i32, y: i32) -> i32 {\n    x + y\n}\n' > src/b.rs
git add -A && git commit -qm c1
export GIT_AUTHOR_DATE="2026-01-03T00:00:00Z" GIT_COMMITTER_DATE="2026-01-03T00:00:00Z"
git mv src/b.rs src/c.rs && git rm -q README.md && git commit -qm c2
git checkout -q -b side
export GIT_AUTHOR_DATE="2026-01-04T00:00:00Z" GIT_COMMITTER_DATE="2026-01-04T00:00:00Z"
printf 'fn d() {}\n' > d.rs && git add -A && git commit -qm s1
printf 'fn d() -> i32 { 1 }\n' > d.rs && git add -A && git commit -qm s2
git checkout -q main
export GIT_AUTHOR_DATE="2026-01-05T00:00:00Z" GIT_COMMITTER_DATE="2026-01-05T00:00:00Z"
git merge -q --no-ff side -m merge
```

After `tally scan`:

`commits` has 4 rows, `seq` 0 to 3, with times 2026-01-01, 01-02, 01-03 and
01-05 at 00:00:00. The two side-branch commits are absent.

`changes` has exactly these 8 rows:

| seq | path | blob |
|---|---|---|
| 0 | `README.md` | blob of the readme |
| 0 | `a.rs` | blob of `a.rs` version 1 |
| 1 | `a.rs` | blob of `a.rs` version 2 |
| 1 | `src/b.rs` | blob of `plus` |
| 2 | `README.md` | NULL |
| 2 | `src/b.rs` | NULL |
| 2 | `src/c.rs` | blob of `plus` (same id as seq 1) |
| 3 | `d.rs` | blob of `d.rs` final version |

`files` has exactly these 6 rows:

| path | since | until |
|---|---|---|
| `README.md` | 0 | 2 |
| `a.rs` | 0 | 1 |
| `a.rs` | 1 | 2147483647 |
| `d.rs` | 3 | 2147483647 |
| `src/b.rs` | 1 | 2 |
| `src/c.rs` | 2 | 2147483647 |

`spans` has 8 rows: one `file` and one `fn` for each of the 4 Rust blobs. The
readme has none. Both versions of `a.rs` report `tokens = 18` for `add`; only
version 2 reports `comments = 2`. That equality is the point: comments and
whitespace do not change `tokens`.

### 9.4 Behavior tests on the fixture

- **Idempotence.** A second `scan` leaves all three row counts unchanged.
- **Incremental.** Scan at `c1`, then scan at `main`. The database equals a
  fresh single scan of `main`.
- **Resume.** Call the walk alone, then run a full `scan`. The database
  equals a fresh single scan.
- **Version bump.** With spans present, change the stored `v` of all rows to a
  different number, then `scan`. All spans are rebuilt at `V`; `commits` and
  `changes` are untouched.
- **Divergence.** Scan `main`, then scan `side`. The stored tip (the merge
  commit) is not on `side`'s first-parent chain, so the command fails with the
  message from 5.1 and changes nothing.
- **Oversized blob.** A file larger than `MAX_BLOB_BYTES` with a known
  extension yields exactly one `file` span with `tokens` NULL.
- **`metrics`.** Selected metrics export as exact JSON on the fixture, all
  metrics export by default in definition order, an unknown name fails.

### 9.5 Metric views

On the fixture: the helper views return the expected rows; `m_locality`,
`m_hidden_coupling`, `m_interface_bloat`, `m_encapsulation` and `m_naming`
return hand-checked values; every `m_*` view has `seq` and `value`. The Grafana
dashboard has one panel per metric view, described by the view's comment, and
no series for a view that does not exist.

## 10. README

The README contains a two-paragraph description, the CLI usage, the schema
with column meanings, the metrics, and how to view them in Grafana. The MVP's
cookbook queries, kept here for reference, were validated on the fixture:

Change locality, per commit:

```sql
select seq, count(*) as files, count(distinct split_part(path, '/', 1)) as roots
from changes group by seq order by seq;
```

Fixture result: `(0,2,2) (1,2,2) (2,3,2) (3,1,1)`.

Size over time:

```sql
select c.seq, count(*) as files, sum(s.tokens) as tokens, sum(s.comments) as comment_words
from commits c
join files f on c.seq >= f.since and c.seq < f.until
join spans s on s.blob = f.blob and s.kind = 'file'
group by c.seq order by c.seq;
```

Fixture result: `(0,1,18,0) (1,2,36,2) (2,2,36,2) (3,3,45,2)`.

Function size and parameter distribution:

```sql
select c.seq, count(*) as fns, quantile_cont(s.tokens, 0.9) as tokens_p90, max(s.params) as params_max
from commits c
join files f on c.seq >= f.since and c.seq < f.until
join spans s on s.blob = f.blob and s.kind = 'fn'
group by c.seq order by c.seq;
```

Duplicated-function ratio (the token threshold is the user's choice; real
repositories should use 50 or more):

```sql
with live as (
  select c.seq, s.hash, s.tokens
  from commits c
  join files f on c.seq >= f.since and c.seq < f.until
  join spans s on s.blob = f.blob and s.kind = 'fn' and s.tokens >= 10
), g as (select seq, hash, sum(tokens) as t, count(*) as n from live group by seq, hash)
select seq, coalesce(sum(t) filter (n > 1), 0) / sum(t) as dup_ratio
from g group by seq order by seq;
```

Fixture result: `(0,0.0) (1,1.0) (2,1.0) (3,1.0)`.

Hotspots:

```sql
select path, count(*) as changes from changes group by path order by changes desc, path limit 20;
```

Largest functions at the latest commit:

```sql
select f.path, s.name, s.line, s.tokens
from files f join spans s on s.blob = f.blob and s.kind = 'fn'
where (select max(seq) from commits) >= f.since and (select max(seq) from commits) < f.until
order by s.tokens desc limit 20;
```

## 11. Build order

1. `extract` with the tests in 9.1 and 9.2. Do not continue until they pass.
2. Schema setup and the walk, with the `commits`, `changes` and `files`
   expectations in 9.3.
3. The parse loop, with the `spans` expectations in 9.3.
4. `metrics`.
5. The behavior tests in 9.4 and the metric tests in 9.5.
6. README.

## 12. Verify, don't assume

This spec was validated with a prototype in another language, so the
semantics, SQL and query files are known to work. The Rust API details below
are not verified. Check each against current crate documentation.

- **gix:** walking only first parents; a tree-to-tree diff with rename
  tracking off; an empty tree to diff the root commit against; reading a blob
  by id; resolving `REV`.
- **tree-sitter:** how the grammar crates expose their language (the
  TypeScript crate exposes two); the iterator type that query matches return
  in the pinned version; `kind_id`, `is_error`, `is_missing`.
- **duckdb:** how the Appender interacts with explicit transactions, since
  5.1 and 5.2 require atomic groups of rows; writing a `u64` into `ubigint`;
  storing the commit time as a plain `timestamp` (the prototype used
  `make_timestamp(<epoch seconds> * 1000000)`); printing query results as a
  table through the Arrow re-export. The bundled build lacks the ICU
  extension, so avoid `timestamptz` and any function that needs ICU.
- **Token counts** in 6.4 and 9.3 are tied to the grammar versions in section
  7. If a pinned version differs and a count changes, confirm the cause, then
  update the expectation deliberately. Never loosen a test to make it pass.

## 13. Done means

- `cargo test`, `cargo clippy -- -D warnings` and `cargo fmt --check` pass.
- `tally scan` on the fixture produces exactly the tables in 9.3.
- `tally scan` on tally's own repository completes, and a second run reports
  `scanned 0 commits, 0 changes; parsed 0 blobs`.
- Nothing listed under Non-goals exists in the code.
- A short note lists every place the implementation deviates from this spec
  and why. An empty list is the expected outcome.
