# codevitals

codevitals tracks code health over a repository's history. It records every commit
and parses every file version with tree-sitter into a DuckDB file, then computes
metrics over it in SQL: size, complexity, how changes spread, and
how files depend on each other. Rust, C, C++, Python, TypeScript and JavaScript.

## Install

Download a binary from the [releases](https://github.com/mainmatter/codevitals/releases),
or `cargo install --git https://github.com/mainmatter/codevitals`.

## Usage

```
cv scan                    # record history into codevitals.duckdb; rerun to add new commits
cv metrics metrics.json    # compute every metric into one JSON file
cv metrics --help          # list the metrics
cv units units.json        # the production files and functions behind them, at the newest commit
cv diff main~10 main       # each metric's change over a range, and the functions and files behind it
cv hotspots                # files ranked by recent changes times decision points
cv label                   # judge pairs of functions: which would you rather have to change?
cv validate                # does each per-function metric predict those judgments better than size?
cv experimental out.json   # candidate metrics without evidence yet (kept off the dashboard)
```

`cv diff` needs no scan: it parses the blobs the database lacks (with a scan that
reached BASE, only those that differ) and reports the metrics of one commit's code
(size, structure, complexity, non-local), as JSON or, with `--format text`, as:

```
nesting        1.1860 -> 1.2080 (+1.9%)  unvalidated
     +0.3%  crates/ignore/src/incremental.rs  parent_key
     +0.7%  crates/ignore/src/pathutil.rs (file)
```

Each contribution is a share of BASE's value, and contributions add up to the change.

To look at the result, import `grafana/dashboards/codevitals.json` into Grafana with
the Infinity data source and point its Metrics URL at the file, or run
`cv-grafana metrics.json` in the Nix dev shell. Each panel explains what it
measures and how to read it.

## In CI

```yaml
# .github/workflows/codevitals.yml
on:
  push:
    branches: [main]
permissions:
  contents: write
concurrency: # a later push's metrics must not be overwritten by an earlier one's
  group: codevitals
  cancel-in-progress: true
jobs:
  codevitals:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: mainmatter/codevitals@v0.1 # the codevitals version it runs: a release (v0.1.0), its line (v0.1), or a commit SHA
        # with:
        #   metrics: size co_change_distance # default: all
```

The action runs on Linux (x64, ARM64) and macOS runners, and downloads the release
binary matching its own version. Each push publishes `metrics.json` to the branch `codevitals-metrics`. In Grafana
Cloud, add an Infinity data source (with a read-only token for a private
repository), import the dashboard, and set its Metrics URL to
`https://raw.githubusercontent.com/<owner>/<repo>/codevitals-metrics/metrics.json`.

## What counts as code

Metrics count production code only, not tests, vendored or generated code, or
documentation. Path conventions decide, unless `.gitattributes` says otherwise:

```
third_party/**   linguist-vendored
src/proto/**     linguist-generated
integration/**   codevitals=test
crates/db/**     codevitals-test-features=mock-io,fixtures
```

Rust test code inside source files does not count either: `#[cfg(test)]` items,
`#[test]`-style functions (`#[tokio::test]`, `#[rstest]`, ...), modules declared with
`#[cfg(test)] mod x;`, and code behind test-only cargo features. A feature is
test-only when its name says so (`test`, `testing`, `test-util`, `test-support`, ...;
`test_feature` in `sql/helpers.sql`) or when `codevitals-test-features` declares it
for the path, as above.

## Caveats

- No metric is validated against defects yet: read trends within one
  repository, not absolute values.
- codevitals reads syntax, not build files: imports are resolved by convention, and
  re-exports, macros and paths used without an import are not followed.
- codevitals needs the full history: deepen shallow clones first.

## Development

`nix develop` (or direnv) gives the pinned toolchain, the `duckdb` CLI and
`cv-grafana`; `cargo test` runs in seconds. Metrics are views in
`sql/metrics.sql`; after changing one, run `python3 grafana/generate.py`.
Per-file and per-function values are the views of schema `units` (`sql/units.sql`).
Languages are rows in `src/languages.rs` with a query in `langs/`.

## License

[MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
