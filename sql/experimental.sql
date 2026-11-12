-- Experimental metrics: candidates with no supporting evidence yet, kept off the
-- dashboard and out of metrics.json (`cv experimental` exports them). The defect check
-- and the labels of `cv validate` decide which graduate to `metrics`. Each comment
-- has the metrics' format.

-- Points where the experimental snapshot metrics are read: each week's last commit,
-- the window ends and the tip (reach_points).

-- Duplicated runs of production tokens. A fingerprint (extract.rs) that occurs twice
-- is a repeated run of 50 tokens, names and literals ignored; every repeated run of
-- 60 or more has one. The bytes of duplicated fingerprints, merged per file and
-- converted to tokens at the file's tokens per byte, approximate the duplicated tokens.
create or replace view experimental.clones as
with live as (
  select p.seq, v.path, v.blob, v.test_features
  from reach_points p join versions v on v.since <= p.seq and p.seq < v.until
  join path_roles r on r.path = v.path and r.role = 'code'),
fps as (
  select l.seq, l.path, s.hash, s.start_byte, s.end_byte
  from live l join code_spans s on s.blob = l.blob and s.test_features = l.test_features
  where s.kind = 'fingerprint'),
dup as (select * from fps qualify count(*) over (partition by seq, hash) > 1),
ordered as (
  select *, max(end_byte) over (partition by seq, path order by start_byte, end_byte
                                rows between unbounded preceding and 1 preceding) as reach
  from dup),
grouped as (
  select *, sum((reach is null or start_byte > reach)::int)
              over (partition by seq, path order by start_byte, end_byte) as island
  from ordered),
bytes as (
  select seq, path, sum(e - s) as bytes from (
    select seq, path, min(start_byte) as s, max(end_byte) as e from grouped group by seq, path, island)
  group by all),
files as (
  select l.seq, l.path, f.lang, f.tokens, fs.end_byte as size
  from live l join blob_facts f using (blob, test_features)
  join spans fs on fs.blob = l.blob and fs.kind = 'file'),
per_file as (
  select f.seq, f.lang, f.tokens, least(f.tokens, coalesce(b.bytes, 0) * f.tokens / nullif(f.size, 0)) as duplicated
  from files f left join bytes b using (seq, path))
select seq, sum(duplicated) / nullif(sum(tokens), 0) as value, sum(duplicated)::bigint as duplicated_tokens,
       family_ratio(duplicated, tokens, lang, 'c') as c, family_ratio(duplicated, tokens, lang, 'rust') as rust,
       family_ratio(duplicated, tokens, lang, 'python') as python, family_ratio(duplicated, tokens, lang, 'ecma') as ecma
from per_file group by seq;

comment on view experimental.clones is 'STRUCTURE: Duplicated token runs

What: the share of production tokens in runs of 50 or more tokens that occur more than once, names and literals ignored, within or across files, each week (value), the duplicated tokens, and per language (c, rust, python, ecma). Lower is better.

Read: 0.08 means 8% of the code repeats elsewhere. Approximate: runs are found by winnowed fingerprints (sure from 60 tokens) and measured in bytes.

Trend: rising means copy-paste. Cloning was among the strongest predictors of expert maintainability ratings in Schnappinger et al. (ICPC 2019, three Java systems); on their later public dataset class and method size predicted best. Unvalidated.';
