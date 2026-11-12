# Labelling `is_fix`

`is_fix_sample.csv` holds 150 RediSearch commits for checking the `is_fix`
keyword rule (`is_fix` in `commit_info.sql`, a view to apply to a scanned database) before any metric is
validated against it.

| Stratum | Commits | Measures |
|---|---|---|
| `flagged` | 60 | Precision: how often a commit the rule calls a fix is one |
| `suspicious` | 60 | Recall: unflagged commits whose subject mentions crash, leak, race, wrong, broken, fail, ... |
| `random` | 30 | The baseline: how many fixes hide among ordinary commits |

Fill in `label` for every row:

- `fix`: the commit repairs incorrect behaviour of production code: a defect,
  crash, leak, wrong result or regression, including a revert of a defective
  change.
- `not`: anything else, including fixes that only touch tests, CI, the build,
  lint, warnings, typos or documentation, refactors, and features.
- `unsure`: the subject and file counts are not enough to decide. Look the commit
  up with `git show <commit>` in the RediSearch repository if you want, or leave it
  unsure; unsure rows are reported separately, not guessed.

`production_files`, `test_files` and `other_files` count the files the commit
changed in each role (`path_roles`), as a hint. `note` is free text.

## Results (labelled 2026-11-11)

136 rows decided (14 unsure). Precision and recall are estimated over the whole
history by weighting each stratum with its population (1,338 flagged, 246
suspicious, 4,066 other commits).

| Rule | Precision | Recall |
|---|---|---|
| Keyword in subject (`is_fix_keyword`) | 0.48 | 0.89 |
| Keyword or defect word, production code touched, no test/CI/docs/lint words (`is_fix`) | 0.86 | 0.89 |

Most false positives touch no production code (flaky tests, CI, docs, build) or
name a test, CI, docs or lint fix. Most missed fixes say crash, leak, deadlock,
hang or wrong without a fix keyword. The `is_fix` rule was designed after seeing
these labels, so its figures are optimistic. No fix appeared among 27 random
unflagged commits, which still allows up to about 450 missed fixes among the
4,066 others (95% upper bound), so recall may be as low as about 0.6.

# Cognitive Complexity against decisions + nesting (2026-11-12)

Before implementing Cognitive Complexity (SonarSource), it was computed offline
per production function at the tip of one Rust and one Python repository
(`examples/cognitive_offline.rs`; it reproduces the specification's examples:
`sumOfPrimes` 7, `getWords` 1) and compared, by Spearman rank correlation, with
decision points plus maximum nesting per function (`units.functions`):

| Repository | Functions | ρ(CC, decisions + nesting) | ρ, functions with either > 0 | ρ(CC, decisions) | ρ(CC, tokens) |
|---|---|---|---|---|---|
| ripgrep (Rust) | 2,174 | 0.971 | 0.815 (594) | 0.982 | 0.686 |
| requests (Python) | 268 | 0.989 | 0.969 (147) | 0.992 | 0.803 |

Both exceed 0.9, so Cognitive Complexity is not implemented: per function it
ranks code almost as decisions plus nesting already do. Run it with
`cargo run --release --example cognitive_offline REPO units.json` on the output of
`cv units`. Caveat: three quarters of ripgrep's functions score 0 on both, which
lifts the overall correlation; among the functions with any branching it is 0.815.
