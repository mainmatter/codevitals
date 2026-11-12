# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2](https://github.com/mainmatter/codevitals/compare/v0.1.1...v0.1.2) - 2026-11-12

### Fixed

- fix CI

### Other

- Merge measure-correctly: faster, incremental scans and correct import cones
- Format label times with jiff instead of by hand
- Write validate tests' labels with one helper
- Declare metrics' and experimental's arguments once
- List the blobs to parse once
- Walk the tree once for facts
- Compute validate's agreement and signs in SQL
- Share test helpers and samples instead of copying them
- Remove the finished cognitive_offline experiment
- Share what walk, diff, db and the tests each rebuilt; no reader trusts a large ORDER BY
- Skip work parse never needs
- Reuse change_weights; classify each path once for parsing
- Fix edit thrash losing a file's lines across renames
- Take lines from bytes only in extract
- Checkpoint once per scan, not every 16 MB of log
- Winnow in linear time; find nodes again in one descent
- Compute the import graph's shared views once
- Build the metric views in the background during a scan
- Fix wrong and unstable import cones on large histories
- Append spans as Arrow record batches
- Copy the facts of unchanged subtrees from the version before
- Append spans with two writers
- Parse each file's history incrementally
- Make cv label and cv validate correct by construction
- Scan faster: parse less, diff in parallel, hash by rolling
- Fix running out of memory on large histories
- Bring cargo test back under ten seconds
- Consolidate shared Rust helpers
- Cache only the helpers a command reads
- Capture Rust panics as throws in the query
- Consolidate repeated SQL
- Hide cv label and cv validate from users
- Remove the whole-function duplication metric
- Add the remaining experimental candidates
- Add an experimental schema, with token-window clones
- Add modularity violations
- Add change span: production files per commit
- Add accretion
- Add orphaning
- Add a defect outcome from history
- Add cv validate
- Add cv label
- Add a labels table
- Stop leading with per-token ratios
- Split panics by intent
- Check Cognitive Complexity offline; do not implement it
- Add a per-function basket: decisions, nesting, parameters
- Add cv hotspots
- Add cv diff BASE HEAD
- Emit per-file and per-function tables
- Store lines added and deleted per change
- Normalize author identity with .mailmap
- Track renames
- Count in-file Rust tests as test code

## [0.1.1](https://github.com/mainmatter/codevitals/compare/v0.1.0...v0.1.1) - 2026-11-12

### Fixed

- fix renovate config
- fix workflows

### Other

- Add renovate.json
