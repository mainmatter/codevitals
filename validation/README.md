# Labelling `is_fix`

`is_fix_sample.csv` holds 150 RediSearch commits for checking the `is_fix`
keyword rule (`is_fix` of the view `commit_info` in `sql/helpers.sql`) before any metric is
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

# Defects after a cut (`cv validate`, 2026-11-12)

History cut every 182 days; at each cut the live production files are ranked by a
metric, and the share of the next 182 days' fix touches (`is_fix`) in the files
ranked first that hold 20% of the tokens is counted, pooled over cuts. `random` is
a fixed pseudo-random order, the level any ranking must beat.

| Ranking | ripgrep (20 cuts, 248 fixes) | tokio (19, 576) | requests (30, 180) |
|---|---|---|---|
| random | 0.234 | 0.188 | 0.450 |
| tokens (size) | 0.093 | 0.151 | 0.183 |
| changes | 0.210 | 0.273 | 0.744 |
| decisions | 0.125 | 0.184 | 0.644 |
| max_decisions | 0.206 | 0.201 | 0.617 |
| max_nesting | 0.246 | 0.193 | 0.556 |
| max_params | 0.206 | 0.194 | 0.561 |
| throws | 0.165 | 0.208 | 0.628 |
| unhandled | 0.181 | 0.240 | (all 0) |
| hotspot | 0.202 | 0.208 | 0.739 |

No static metric beats change count where change count beats random (tokio,
requests); on ripgrep nothing beats random by a margin its 248 fixes could show.
Size ranks worst everywhere: large files spend the 20% budget on few files, and
fixes per token are higher in small files. `is_fix` recall may be as low as 0.6,
and requests has 20 production files, so its random level is noisy.
