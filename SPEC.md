# tally: specification

tally is a Rust CLI that records facts about a git repository's history into a
DuckDB file and exports metrics over them. This document holds the rules and
semantics; the code holds the details. The schema is `src/schema.sql`, the
metrics are `src/metrics.sql`, the queries are `langs/*.scm`, and the fixture
repository is `tests/fixture.sh`. Where this document and a test disagree, the
test is wrong or this document is stale: fix one of them.

## Design rules

These decide every question this document leaves open.

- **Every concept is a liability.** Add no table, column, flag, subcommand,
  file, trait or dependency without a reason that outweighs it.
- **No metrics in Rust.** Rust records facts and exports views. Anything that
  can be computed in SQL from stored facts is computed in SQL. The one exception
  is reachability in the import graph (`cones`, `src/graph.rs`): a transitive
  closure at every commit is beyond SQL's budget, so Rust derives each file's
  cone size and SQL computes the metrics from it.
- **Append-only, idempotent, resumable.** Running `scan` twice changes nothing.
  An interrupted `scan`, rerun, ends in the same state as an uninterrupted one.
  The only deletions are the version purge (parse, step 1) and the reset the
  divergence error prescribes.
- **Bounded.** Every connection sets `memory_limit` and
  `max_temp_directory_size` to 4 GB, so a runaway query fails instead of
  exhausting the machine.
- **Fail early and loudly.** Assert internal invariants; do not hedge against
  impossible states.
- **No hand-rolled parsing or hashing.** Use the named crates.

## Non-goals

A config file, path include/exclude options, progress bars, SCIP or other
semantic analysis, PR gates or a `check` command, LLM calls, C/C++
preprocessing, rename detection, branches other than one first-parent chain.

## CLI

```
tally scan    [--repo <PATH>] [--db <PATH>] [REV]
tally metrics [--db <PATH>] <OUTPUT> [METRIC]...
```

- `--repo` defaults to `.`, `--db` to `tally.duckdb` (created if missing), `REV`
  to `HEAD`.
- `scan` runs the walk, then the parse. On success it prints one line to
  stderr: `scanned <N> commits, <M> changes; parsed <K> blobs`.
- `metrics` first computes every helper view once, as a temp table shadowing
  the view for its connection, and rebuilds `cones` from `import_edges`; then it
  exports. Views and macros are re-applied on open only when the SQL differs
  from the one the database was last opened with (hash in `tally_sql()`).
- `metrics` writes one JSON object mapping each selected metric view (all when
  none is named) to an array of row objects. Each row has `seq`, then the
  commit `time` (ISO 8601 UTC), then the view's columns; an empty metric is
  `[]`. `OUTPUT` is a file, written beside the target and renamed into place,
  or `-` for stdout. An `OUTPUT` that names a metric is rejected. `--help`
  lists the metrics with their explanations.
- Logs go to stderr through `tracing`, filtered by `RUST_LOG` (default `info`:
  phases and progress; `debug`: one line per commit and per blob, the blob
  logged before it is parsed).
- Errors go to stderr with a non-zero exit code.

## Walk

Extends `commits` and `changes`; never reads blob contents.

1. The stored tip is the `commits` row with the greatest `seq`.
2. Collect `REV`'s first-parent chain, newest first, stopping at the stored tip
   or the root. If a tip is stored and was not reached, fail with
   `history diverged from the stored chain; reset the walk with: duckdb <db> "delete from commits; delete from changes" (parsed spans are kept), or delete the database file, then scan again`.
3. Number new commits oldest first, continuing after the stored tip (`seq` 0
   for an empty database). Store the committer time in UTC as `timestamp`, the
   author as `Name <email>`, and the full raw message.
4. Diff each commit's tree against its first parent's (the empty tree for a
   root). `B(tree, path)` is the blob id at `path` for a regular or executable
   file, and none otherwise. For every `path` where `B` differs, append
   `(seq, path, B(cur, path))`, none as `NULL`. So a rename is a `NULL` row
   plus a new row, a mode-only change has no row, and a merge is compared with
   its first parent only.
5. A commit's row and its changes are written in one transaction; one
   transaction may hold several commits.

## Parse

Makes `spans` complete for every changed blob with a known language.

1. `delete from spans where v <> V`. Bump `V` (in `src/main.rs`) whenever
   `extract` output could change: a rule, query file or grammar version change.
   The next scan then re-parses all history.
2. List each blob in `changes` without spans, with `min(path)` as its path.
3. Pick the language from the lowercased extension of that path; skip unknown
   extensions. A blob over 1 MiB gets one row: kind `file`, line 1, all counts
   `NULL`. Otherwise store `extract`'s spans.
4. A blob's rows are written in one transaction; one transaction may hold
   several blobs. Blobs are read and extracted in parallel and appended by one
   writer.

Walk and parse are independent, so bumping `V` or adding a language backfills
history through the parse alone.

## Extract

`extract(lang, src) -> Vec<Span>` parses `src` with the language's grammar,
runs its query, and returns spans. It is pure; the only global is the memoized
compiled query per language. It never fails: tree-sitter always returns a tree.

Captures in a query file:

- `@comment` marks a comment, `@name` names the span of its match, `@params` is
  the span's parameter list.
- A capture starting with `_` is private to the query (for predicates such as
  `(#eq? @_s "static")`) and creates no span.
- Any other capture creates a span whose `kind` is the capture name, so
  `(unsafe_block) @unsafe` needs no Rust change.

A match contributes at most one span, its first span capture. A span's identity
is `(kind, start byte, end byte)`; matches of the same span merge, the first
non-null `@name` and `@params` winning. Every blob also gets one `file` span.

| Column | Definition |
|---|---|
| `name` | Text of `@name`, lossy UTF-8; `NULL` if none |
| `line`, `end_line` | 1-based lines of the first and last byte; a span ending at column 0 ends on the previous line; never `end_line < line` |
| `tokens` | Code leaves inside the span: nodes without children, of non-zero length, not inside a comment |
| `comments` | Whitespace-separated words of the comments inside the span |
| `params` | Named children of `@params` that are not comments; `NULL` without `@params` |
| `hash` | XXH3-64 of the language name, then each code leaf's `kind_id` as `u16` little-endian, in source order |
| `errors` | For `file`: nodes that are errors or missing; `NULL` otherwise |
| `start_byte`, `end_byte` | The span's byte range, for containment between spans |
| `depth` | Spans of the same kind that strictly contain this one |

"Inside" is byte-range containment, so a nested function also counts toward
its parent and everything counts toward `file`. Identifiers and literals share
a node kind, so functions differing only in names and literals hash equally.

## Metrics

`src/metrics.sql` is applied after the schema on every start, so every
database carries the current definitions and any DuckDB client can read them.
It starts with the tuning constants (module depth, window, horizons,
thresholds), then helpers (`path_modules`, `path_roles`, `live`, `versions`,
`commit_info`, `fns`, `code_changes`, `windows`, `blob_facts`,
`code_totals`, then the import graph: `import_status`, `graph_files`, `import_edges`,
`reach`), then the
metrics.

A metric is a view named `m_<name>` with `seq` (the commit, or the first
commit of its window) and `value` columns, its components, and a
`comment on view`: a category, then what it measures, how to read its chart
and what a trend implies, which direction is better and how strong the evidence
is. The Grafana dashboard (`grafana/generate.py`) has one panel per metric,
described by that comment.

Metrics count production code only (`path_roles`), so the mix of tests and
vendored code does not drive them. State metrics are computed at every commit.
Process metrics use a trailing window of `window_commits()` code-touching
commits read off once per ISO week, so every point has the same sample size;
each commit weighs the same, commits over `max_commit_files()` files are left
out as bulk changes, and windows spanning more than `window_max_weeks()` are not
reported. Metrics over modules report a lift: the
observed share divided by its chance level for the same module sizes, keeping
both as components. Size is its own series (`m_size`), never a divisor. No
metric is validated against defects; validation is a separate study at the level
of files and commits, with size and churn as covariates and a placebo outcome,
not a regression on repository-level series.

Structure facts are aggregated once per blob (`blob_facts`) and carried to every
commit and language by change points (`code_totals`); a function's nesting is its
deepest block minus its body block, so else-if chains stay flat.

Imports resolve to files by each language family's conventions (C with C++,
the ECMAScript dialects together; family by extension, not by the blob's
`lang`), in `import_candidates`: C includes next to the includer, else any path
ending in the include; Python's longest dotted prefix naming a module, relative
to the package for leading dots; ECMAScript relative specifiers with the usual
extensions and index files; Rust `crate::`, `self::` and `super::` (inline
modules included), child modules, and workspace crates by directory name, to the
longest prefix naming a module file. `mod` declarations are not imports. Among
candidates alive at a commit the highest rank wins, then the one sharing most
directories with the importer, then the shallowest; a tie is `ambiguous`.
`import_status` classifies every import as `resolved`, `ambiguous`, `external`
(by syntax, resolved to a vendored file, or a quoted C include under a directory
the repository never had, as from a git submodule) or `unresolved`, as intervals
that change only when candidate files appear or disappear, and flags `fallback`
resolutions that matched only a module's own file; `m_import_coverage`
reports both rates. The graph (`import_edges`) is between production files.
Files import only files of their own family, so dependency metrics compute per
family, report each family as a column, and combine them into `value`; the
import-coupling null rewires imports within a family only. A merged
header-and-implementation node was tried and dropped: it turned mutual use
between C files into cycles that dominated propagation cost.

Per-commit metrics must not join every commit with every live span: that grows
with commits times code size. Compute facts once per blob or per file version,
then add them at a version's `since`, remove them at its `until` (the
`versions` view), and take a running sum over `commits`; `code_totals` does this
for every column of `blob_facts`. A value that is not a sum (duplication, which
depends on how many copies of a structure are live) is tracked per group: each
change of a group adds its new contribution and removes its old one. Medians and
maxima are evaluated only at the commits where their inputs change.

## Languages

| Name | Extensions | Grammar crate | Query |
|---|---|---|---|
| `rust` | `rs` | `tree-sitter-rust` | `langs/rust.scm` |
| `c` | `c` | `tree-sitter-c` | `langs/c.scm` |
| `cpp` | `h` `cc` `cpp` `cxx` `hh` `hpp` `hxx` | `tree-sitter-cpp` | `langs/cpp.scm` |
| `python` | `py` | `tree-sitter-python` | `langs/python.scm` |
| `typescript` | `ts` `mts` `cts` | `tree-sitter-typescript` | `langs/ecma.scm` |
| `tsx` | `tsx` | `tree-sitter-typescript` | `langs/ecma.scm` |
| `javascript` | `js` `mjs` `cjs` `jsx` | `tree-sitter-javascript` | `langs/ecma.scm` |

`.h` is C++ on purpose: the C++ grammar parses C headers acceptably, not the
reverse. Every query captures functions (`fn`), calls (`call`), imports (one
`import` span per target path: Rust `use` trees flattened, Python names joined
to their module), Rust inline modules (`module`),
comments, visibility where the syntax shows it (`pub` for Rust and JS/TS
exports, `static` for C/C++; Python's comes from the name, in SQL), and
control-flow structure: `branch` (if, loops, match and case arms, catch,
ternaries), `condition` (`&&`, `||`, `and`, `or`, `??`), `block` (for nesting),
`global_state` (Rust `static mut`, C/C++ file-scope non-const variables, Python
`global`/`nonlocal`, JS/TS module-level `let`/`var`), `throw` (throw, raise,
Rust panics, `unwrap` and `expect`, C `longjmp`) and `goto` (C/C++). Adding a
language takes one grammar crate, one row in `LANGS`, one query file and one
golden test.

## Accepted quirks

- C/C++ `f(void)` reports `params = 1`; a JS/TS arrow function with one bare
  parameter reports `params = NULL`.
- Anonymous functions passed as arguments are not spans; their tokens count
  toward the enclosing span. Python docstrings are strings, not comments.
- Import resolution is heuristic: C include paths are guessed by suffix, Python
  roots by suffix, Rust crate names by directory name (a crate renamed in its
  `Cargo.toml` is external), and ECMAScript bare specifiers are external even
  for workspace packages. Rust paths used without `use` and `#[path]` modules
  are not seen. Re-exports are not followed: an import of an item a Rust crate
  root or an `index.ts` re-exports is an edge to that root, so roots reach much
  of their crate (`fallback` counts the Rust and Python cases).
- C/C++ macros that expand to syntax cause local misparses, visible in
  `errors`; they never fail a run.
- A blob reachable under two extensions is parsed once, by `min(path)`.
- Visibility is syntactic: a Rust `pub` (including `pub(crate)`) inside a
  private module is public.
- Calls are recorded as facts but resolved by nothing: by bare name they would
  match across languages and receivers, so the import graph stands in for them.
- Roles follow path conventions: tests inside source files (Rust
  `#[cfg(test)]`) count as production code, and a `deps/` or `external/`
  directory of first-party code counts as vendored.
- A path's module is its nearest directory that held a `Cargo.toml`,
  `package.json` or `pyproject.toml` at any commit (the repository root never
  counts), else its directory at a fixed depth. C/C++ has no manifest, so its
  modules are directories; incidental manifests (test fixtures, examples) make
  small modules.
- Windowed series report the window's first commit, and the newest window is
  partial until it fills.

## Tests

`cargo test`, `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` pass. Tests come in three layers:

- `tests/extract.rs`: `extract` on source text: the worked example (token
  counts tied to the grammar versions in `Cargo.lock`), one golden test per
  language covering functions, calls, imports, comments and visibility, empty
  and broken input, and purity.
- `tests/scan.rs`: the walk and the parse on git repositories written in process
  with gix (`tests/common`). One test runs `tests/fixture.sh` with real git and
  requires the same commit ids, so the in-process fixture is the spec's; it is
  the only test that needs `git`. The others pin exact `commits`, `changes`,
  `files` and `spans`, idempotence, incremental scans, version bumps,
  divergence, oversized blobs, refusing old databases, modules and roles of conventional paths,
  import resolution in every language family over time, and squashing.
- `tests/metrics.rs`: hand-checked metric values and the `seq`/`value` contract
  on the fixture, the JSON export, the dashboard matching the views, renaming
  invariance, and properties on synthetic facts written straight into the
  database (no git): lifts are 1 under a null of thousands of commits at 5, 50 and
  200 modules, import coupling is 1 under random rewiring (hubs included),
  dependency metrics on a small graph by hand, and later commits never rewrite
  past windows. `src/graph.rs` checks its cones against breadth-first search.

## Dependencies

| Crate | Use |
|---|---|
| `clap` (derive, string) | CLI; `string` lets `--help` list metrics read from the catalog |
| `gix` | Repository access, first-parent walk, tree diff, blob read |
| `tree-sitter` + six grammar crates | Parsing and queries |
| `duckdb` (bundled, json) | Storage, SQL and the JSON export |
| `xxhash-rust` (xxh3) | Span hash |
| `rayon` | Parallel blob extraction |
| `tracing`, `tracing-subscriber` (env-filter) | Logging |
| `anyhow` | Error propagation |

Dev-dependency: `tempfile`. The Nix flake provides the toolchain
(`rust-toolchain.toml`), the `duckdb` CLI and `tally-grafana`.
