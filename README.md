# codevitals

codevitals tracks code health over a repository's history. It records every commit
and parses every file version with tree-sitter into a DuckDB file, then computes
metrics over it in SQL: size, complexity, duplication, how changes spread, and
how files depend on each other. Rust, C, C++, Python, TypeScript and JavaScript.

## Install

Download a binary from the [releases](https://github.com/mainmatter/codevitals/releases),
or `cargo install --git https://github.com/mainmatter/codevitals`.

## Usage

```
cv scan                    # record history into codevitals.duckdb; rerun to add new commits
cv metrics metrics.json    # compute every metric into one JSON file
cv metrics --help          # list the metrics
```

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
```

## Caveats

- No metric is validated against defects yet: read trends within one
  repository, not absolute values.
- codevitals reads syntax, not build files: imports are resolved by convention, and
  re-exports, macros and paths used without an import are not followed.
- Tests inside source files (Rust `#[cfg(test)]`) count as production code.
- codevitals needs the full history: deepen shallow clones first.

## Development

`nix develop` (or direnv) gives the pinned toolchain, the `duckdb` CLI and
`cv-grafana`; `cargo test` runs in seconds. Metrics are views in
`sql/metrics.sql`; after changing one, run `python3 grafana/generate.py`.
Languages are rows in `src/languages.rs` with a query in `langs/`.

## License

[MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
