-- One row: the layout and extractor versions of the data (db.rs), and the git
-- directory of the repository scanned, whose objects hold the blobs.
create table if not exists codevitals (schema integer, extractor integer, repo text);
-- The metric views (metrics.sql).
create schema if not exists metrics;
-- Candidate metrics without supporting evidence yet (experimental.sql), kept off
-- the dashboard; `cv validate` decides which graduate.
create schema if not exists experimental;
-- Per-file and per-function values behind the metrics (units.sql).
create schema if not exists units;

-- author_name, author_email: as the commit records them; `authors` applies .mailmap.
create table if not exists commits (
  seq integer, id text, time timestamp, author_name text, author_email text, message text);
-- The .mailmap of the newest scanned revision, one row per entry in file order
-- (walk.rs): commits whose email (and name, when given) match get the proper ones.
create table if not exists mailmap (
  line integer, proper_name text, proper_email text, commit_name text, commit_email text);
-- attr: the role .gitattributes give the path at that commit; test_features: the
-- cargo features they declare test-only there, joined by `,` (attributes.rs).
-- source: the path the file was renamed from (walk.rs), which has a row of its own
-- with a NULL blob. added, deleted: lines, against the file's previous blob (at the
-- source path for a rename); NULL for files codevitals does not parse (unknown
-- language, binary, generated, too large) and for the deletion a rename implies.
create table if not exists changes (
  seq integer, path text, blob text, attr text, test_features text, source text,
  added integer, deleted integer);
create table if not exists spans (
  blob text, v integer, lang text, kind text, name text,
  line integer, end_line integer, tokens integer, comments integer,
  params integer, hash ubigint, errors integer, start_byte integer, end_byte integer,
  depth integer);

-- Human judgments of pairs of functions (labels.rs, `cv label`): which of two the
-- rater would rather not have to change. Loaded from the labels file, which keeps them
-- across rescans; a function is its blob and byte range, so labels survive both.
-- n numbers each rater's judgments; repeat_of is the n of the rater's earlier
-- judgment of the same pair (sides may be swapped). choice: a, b or same.
-- a_commits, b_commits: the rater's commits to each function's file, then.
create table if not exists labels (
  rater text, n integer, time timestamp,
  a_blob text, a_start integer, a_end integer, a_path text,
  b_blob text, b_start integer, b_end integer, b_path text,
  choice text, ms integer, a_commits integer, b_commits integer, repeat_of integer);

-- Winnowed fingerprints of runs of tokens, for clones (extract.rs `fingerprints`):
-- many per blob, so kept apart from `spans`.
create table if not exists fingerprints (blob text, hash ubigint, start_byte integer, end_byte integer);

-- Derived from import_edges by graph.rs, not a fact.
create table if not exists cones (path text, since integer, until integer, cone integer, scc integer);

-- test_features is '' when none are declared, so it can be joined on.
create or replace view files as
select path, blob, test_features, since, until from (
  select path, blob, coalesce(test_features, '') as test_features, seq as since,
         coalesce(lead(seq) over (partition by path order by seq), 2147483647) as until
  from changes
) where blob is not null;
