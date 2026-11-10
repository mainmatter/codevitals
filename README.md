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
tally metrics [--db <PATH>] [METRIC]...
```

| Argument | Default | Meaning |
|---|---|---|
| `--repo` | `.` | Path to a git repository |
| `--db` | `tally.duckdb` | DuckDB database file; created if missing |
| `REV` | `HEAD` | Revision whose first-parent history is scanned |
| `METRIC` | all | Metric views to export; `tally metrics --help` explains each |

`scan` logs phases and progress to stderr. Set `RUST_LOG=debug` for one line
per commit and per blob (the blob is logged before it is parsed, so a hang
names its culprit), or `RUST_LOG=off` for silence.

`metrics` prints one JSON object mapping each metric view to an array of row
objects. Every row has `seq`, the commit `time` in ISO 8601 UTC, `value`, and
the metric's components:

```json
{"m_locality":[{"seq":0,"time":"2016-04-12T18:28:55Z","value":1.0,"files_per_commit":10.0}, ...],
 "m_scatter":[{"seq":0,"time":"2016-04-12T18:28:55Z","win":0,"value":0.97,"files":287,"changes":429}, ...]}
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
| `kind` | `file` (one per blob, the whole source), `fn`, `call`, `import`, a visibility marker (`pub` for Rust and JS/TS exports, `static` for C/C++), or any other capture name in the language's query file |
| `name` | Name of the function, called function or import; `NULL` for `file` and markers |
| `line`, `end_line` | 1-based first and last line |
| `tokens` | Code tokens inside the span, excluding comments and whitespace |
| `comments` | Words inside comments within the span |
| `params` | Number of parameters; `NULL` if not applicable |
| `hash` | XXH3 of the token kinds: equal for code that differs only in names and literals |
| `errors` | Parse error nodes, for `file` only |

A span counts everything inside it, so a nested function also counts toward
its parent and everything counts toward `file`. Blobs over 1 MiB get only a
`file` row with `NULL` counts.

### `files`: view

`(path, blob, since, until)`: the file version is live at commit `s` when
`since <= s and s < until`. Join it with `commits` to reconstruct the tree at
any commit.

## Metrics

| View | Measures | Better |
|---|---|---|
| `m_locality` | Share of the last 50 commits that stay inside one module | Higher |
| `m_scatter` | Entropy of changes across files, per 50 commits | Lower |
| `m_hidden_coupling` | Share of same-commit file pairs that span modules, per 100 commits | Lower |
| `m_call_coupling` | Share of resolved cross-file calls that cross modules, plus mutually calling module pairs; every 50th commit | Lower; pairs at zero |
| `m_encapsulation` | Share of function tokens in non-public functions | Higher |
| `m_duplication` | Share of function tokens whose structure occurs more than once | Lower |
| `m_interface_bloat` | Parameters per 100 function tokens | Lower |
| `m_concentration` | Share of function tokens in functions over 200 tokens | Lower |
| `m_sprawl` | Share of files in the largest directory | Lower |
| `m_rework` | Share of modifications hitting a file changed in the last 5 commits, per 50 commits | Lower |
| `m_fix_follow` | Share of changes followed by a fix to the same file within 20 commits; the outcome label | Lower |
| `m_naming` | Share of function names of three characters or fewer, plus lexical cohesion | Lower |

`tally metrics --help` and `select comment from duckdb_views()` give the full
explanation of each. Three combinations carry more signal than any single
metric: sprawl improving while locality and hidden coupling do not (fake
modularity); concentration improving while interface bloat worsens (splitting
without simplifying); call or hidden coupling rising while encapsulation falls
(entanglement creep).

The helper views `commit_info` (`subject`, `is_fix`, `is_agent`), `fns` (one
row per function with a `public` flag) and `call_edges` (a heuristic call graph)
are available for your own queries. Modules are directories at depth 2
(`module(path, 2)`); thresholds and windows are constants in `src/metrics.sql`.
Calibrate them against a repository you consider healthy, and validate any
metric against `m_fix_follow` before relying on it.

To add a metric, add a view named `m_<name>` with `seq` and `value` columns and
a `comment on view` to `src/metrics.sql`, and a panel to the Grafana dashboard.
The tests fail for a metric view without the right columns or without a
matching panel.

## Grafana

The Nix dev shell (`nix develop`, or `direnv allow` with the `.envrc`) has the
Rust toolchain, the `duckdb` CLI and `tally-grafana`:

```
tally scan --repo ../project
tally metrics > metrics.json
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
