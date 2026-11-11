# tally

tally records facts about a git repository's history into a DuckDB file. It
walks the first-parent history of one branch and stores which files changed in
each commit. It parses every distinct blob once with tree-sitter and stores
what it contains: functions, calls, imports, comments, visibility markers,
token counts and a structure hash.

Metrics are SQL views over those facts, defined in `src/metrics.sql` and
applied to the database on every start, so `tally metrics`, the `duckdb` CLI
and any other client read one definition. Each view stores an explanation of
what it measures, which direction is better, and how strong the evidence is.
Scans are append-only, idempotent and resumable: run `tally scan` again after
new commits and it records only what is new.

## Usage

```
tally scan    [--repo <PATH>] [--db <PATH>] [REV]
tally metrics [--db <PATH>] <OUTPUT> [METRIC]...
```

| Argument | Default | Meaning |
|---|---|---|
| `--repo` | `.` | Path to a git repository |
| `--db` | `tally.duckdb` | DuckDB database file; created if missing |
| `REV` | `HEAD` | Revision whose first-parent history is scanned |
| `OUTPUT` | required | JSON file to write, replaced atomically; `-` for stdout |
| `METRIC` | all | Metric views to export; `tally metrics --help` explains each |

`scan` logs phases and progress to stderr. Set `RUST_LOG=debug` for one line
per commit and per blob (the blob is logged before it is parsed, so a hang
names its culprit), or `RUST_LOG=off` for silence.

`metrics` prints one JSON object mapping each metric view to an array of row
objects. Every row has `seq`, the commit `time` in ISO 8601 UTC, `value`, and
the metric's components:

```json
{"m_size":[{"seq":0,"time":"2016-04-12T18:28:55Z","value":2798,"files":10}, ...],
 "m_hidden_coupling":[{"seq":0,"time":"2016-04-12T18:28:55Z","value":0.41,"observed":0.33,"expected":0.81,"commits":38}, ...]}
```

For ad-hoc queries, open the database with the `duckdb` CLI.

If the branch was rewritten so the stored history is no longer on its
first-parent chain, `scan` fails and tells you how to reset the walk with the
`duckdb` CLI. Parsed
spans survive a reset, since they are keyed by blob.

Languages: Rust (`rs`), C (`c`), C++ (`h cc cpp cxx hh hpp hxx`), Python
(`py`), TypeScript (`ts mts cts`), TSX (`tsx`), JavaScript (`js mjs cjs jsx`).

## Schema

### `commits`: one row per scanned commit

| Column | Meaning |
|---|---|
| `seq` | Position on the first-parent chain, oldest = 0, no gaps |
| `id` | Commit id, 40 hex characters |
| `time` | Committer time in UTC (`timestamp`) |
| `author` | `Name <email>` |
| `message` | The full commit message, trailers included |

### `changes`: one row per file whose blob changed in a commit

| Column | Meaning |
|---|---|
| `seq` | The commit that made the change |
| `path` | Repo-relative path with `/` separators |
| `blob` | Blob id at `path` after the commit; `NULL` if the file is gone |

Only regular and executable files count. Renames appear as a deletion plus an
addition; mode-only changes are not recorded; merges are compared with their
first parent.

### `spans`: parsed facts, keyed by blob

| Column | Meaning |
|---|---|
| `blob` | Blob id |
| `v` | Extractor version that produced the row |
| `lang` | Language name |
| `kind` | `file` (one per blob, the whole source), `fn`, `call`, `import`, `module` (a Rust inline `mod x { }`), a visibility marker (`pub` for Rust and JS/TS exports, `static` for C/C++), control-flow structure (`branch`, `condition`, `block`, `global_state`, `throw`, `goto`), or any other capture name in the language's query file |
| `name` | Name of the function, called function or module; for `import`, one target path as written (`crate::a::B`, `pkg.mod.name`, `"x.h"`, `./a`), one span per target; `NULL` for `file` and markers |
| `line`, `end_line` | 1-based first and last line |
| `tokens` | Code tokens inside the span, excluding comments and whitespace |
| `comments` | Words inside comments within the span |
| `params` | Number of parameters; `NULL` if not applicable |
| `hash` | XXH3 of the token kinds: equal for code that differs only in names and literals |
| `errors` | Parse error nodes, for `file` only |
| `start_byte`, `end_byte` | Byte range in the source, for containment between spans |
| `depth` | How many spans of the same kind enclose this one, e.g. a block's nesting |

A span counts everything inside it, so a nested function also counts toward
its parent and everything counts toward `file`. Blobs over 1 MiB get only a
`file` row with `NULL` counts.

### `cones`: derived from the import graph

`(path, since, until, cone, scc)`: from `since` to `until`, `path` reaches
`cone` other files through imports, in a strongly connected component of `scc`
files. Not a fact: `tally metrics` rebuilds it from `import_edges`, because a
transitive closure at every commit is too costly in SQL. Run `tally metrics`
after a scan before querying `m_dependency_reach` or `m_changed_file_reach` yourself.

### `files`: view

`(path, blob, since, until)`: the file version is live at commit `s` when
`since <= s and s < until`. Join it with `commits` to reconstruct the tree at
any commit.

## Metrics

Every metric counts production code only: `path_roles` sets aside tests,
vendored code and non-code files by common path conventions.

| View | Measures | Better |
|---|---|---|
| `m_size` | Production code tokens (`value`) and files at each commit; context for every other chart | Neither |
| `m_hidden_coupling` | How often files changed in one commit sit in different modules, as a lift over chance | Lower |
| `m_rework` | Share of a commit's modifications to files already changed in the previous 7 days | Lower |
| `m_change_entropy` | Over how many files work is effectively spread (order-2 effective number), with files per commit beside it | Lower |
| `m_module_entropy` | Over how many modules work is effectively spread, with the number of live modules beside it | Lower |
| `m_ownership` | How concentrated each file's changes are in its most active author | Higher |
| `m_decisions` | Decision points (branches, `&&`/`||`) per 100 tokens, overall and `by_language` | Lower |
| `m_nesting` | Mean block nesting below function bodies, weighted by function tokens, and the share nested deeper than 3 | Lower |
| `m_global_state` | Declarations of global mutable state per 1,000 tokens | Lower |
| `m_exceptions` | Throws, raises and panics (Rust `unwrap`/`expect` included) per 1,000 tokens | Lower |
| `m_goto` | `goto` per 1,000 C/C++ tokens | Lower |
| `m_encapsulation` | Share of function tokens in non-public functions, overall and `by_language` | Higher |
| `m_duplication` | Share of function tokens whose structure occurs more than once, overall and `by_language` | Lower |
| `m_concentration` | Share of function tokens in functions over 200 tokens, overall and `by_language` | Lower |
| `m_dependency_reach` | Share of its language's code a typical file depends on through its imports and theirs (its *reach*), per language | Lower |
| `m_cross_module_imports` | How often imports cross module boundaries, relative to chance, per language | Lower |
| `m_changed_file_reach` | Where changed files rank among their language's files by reach (50% = typical) | Lower |
| `m_import_coverage` | Share of imports naming this repository that resolve to a file, per language: the coverage of the three above | Neither (a check) |

A *lift* divides an observed value by its chance level, so 1 means no
difference: for hidden coupling, module labels shuffled among the files
involved (an exact null), with `observed` and `expected` kept as components;
for cross-module imports, every import rewired at random within its language family
keeping how much each module imports and is imported. Files import only files of
their own language family, so the dependency metrics report each family as a
column (`c`, `rust`, `python`, `ecma`); read those, since the combined value
moves when the language mix changes. Size is a series of its own, never a divisor: judge the
other trends against it.

Size, structure and dependencies are computed at every commit. The change and entropy metrics
use a trailing window of the last 100 commits that touched production code,
read off once per ISO week, so every point has the same sample size;
`span_weeks` says how much calendar time a window covers, and windows spanning
more than 26 weeks are left out. `low` and `high` span two standard errors of one
window; neighbouring windows share most of their commits, so a run of points
moving together is one change, not many. `tally metrics --help` and
`select comment from duckdb_views()` explain each metric: what it measures,
how to read the chart, and what a trend implies.

None of the metrics is validated against defects yet. Read trends within one
repository, not absolute values or comparisons between repositories, and gate
on nothing until a metric has passed a validation study.

The helper views `commit_info` (`subject`, `is_fix`, `is_agent` for coding-agent
co-author trailers, `is_bot` for `[bot]` authors), `fns` (one row per function
with a `public` flag), `import_status` (every import over time, classified
`resolved`, `ambiguous`, `external` or `unresolved`, with its target file),
`import_edges` (the import graph between production files as intervals),
`path_roles` and `path_modules` are
available for your own queries. A file's module is its nearest directory that ever held a
`Cargo.toml`, `package.json` or `pyproject.toml` (never the repository root),
else its directory at a fixed depth. The module depth, windows and thresholds
are named constants at the top of `src/metrics.sql`.

To add a metric, add a view named `m_<name>` with `seq` and `value` columns and
a `comment on view` (a category, then What, Read and Trend paragraphs) to
`src/metrics.sql`, then regenerate the Grafana dashboard with
`python3 grafana/generate.py`. The tests fail for a metric view without the
right columns or without a matching panel.

## Development

Work inside the Nix dev shell: it has the Rust toolchain pinned in
`rust-toolchain.toml`, the `duckdb` CLI, Python for the dashboard generator, and
`tally-grafana`. Building in and out of it alternately rebuilds everything
(DuckDB included), since the compiler and environment differ. With direnv the
shell loads on entering the directory, for terminals and editors alike (VS Code:
the direnv extension):

```
nix profile install nixpkgs#direnv     # once per machine
echo 'eval "$(direnv hook zsh)"' >> ~/.zshrc
direnv allow                           # once per checkout
```

Without direnv, use `nix develop`.

## Grafana

In the dev shell:

```
tally scan --repo ../project
tally metrics metrics.json
tally-grafana metrics.json      # http://127.0.0.1:3000/d/tally
```

`tally-grafana` starts a throwaway Grafana with the Infinity data source and
the dashboard in `grafana/`, serving the file on `127.0.0.1:8123`. The
dashboard reads its data from the `metrics_url` variable, so the same
dashboard can point at a `metrics.json` published by CI.

## Adding a language

Add the grammar crate, one row in `LANGS` in `src/extract.rs`, a query file in
`langs/`, and a golden test in `tests/extract.rs`. Any capture name in a query
file other than `@comment`, `@name` and `@params` becomes a span kind, so
`(unsafe_block) @unsafe` in `langs/rust.scm` records unsafe blocks with no Rust
change. Captures starting with `_` create no span; use them for predicates,
as in `(storage_class_specifier) @_s (#eq? @_s "static")`. Bump `V` in `src/main.rs` whenever extraction output could change; the
next scan re-parses all history.
