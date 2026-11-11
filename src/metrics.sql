-- Applied on every start, after schema.sql. Metrics are the views named m_*: each
-- has `seq` (the commit, or the last commit of its window) and `value`, plus its
-- components, and a `comment on view` explaining it. Metrics count production
-- code only (path_roles); none is validated against defects yet.

-- ============ tuning ============
create or replace macro module_depth() as 2;    -- module depth where no manifest says otherwise
create or replace macro window_commits() as 100; -- code-touching commits per trailing window
create or replace macro window_max_weeks() as 26; -- windows spanning longer are not reported
create or replace macro max_commit_files() as 30; -- larger commits are bulk changes, left out of process metrics
create or replace macro rework_days() as 7;     -- days within which a repeat change counts as rework
create or replace macro clone_tokens() as 50;   -- smallest function counted by duplication
create or replace macro large_tokens() as 200;  -- functions above this count toward concentration
create or replace macro deep_nesting() as 3;    -- functions nested deeper than this count as deeply nested

-- ============ helpers ============
-- A mean's band of two standard errors, for windowed metrics: low(x) and high(x).
create or replace macro low(x) as avg(x) - 2 * stddev_samp(x) / sqrt(count(x));
create or replace macro high(x) as avg(x) + 2 * stddev_samp(x) / sqrt(count(x));

create or replace macro directory(path) as
  array_to_string(list_slice(string_split(path, '/'), 1, -2), '/');

-- Module roots: directories that held a build manifest at any commit, except the
-- repository root (a root manifest describes the whole repository, not a module).
-- Taking any commit keeps a path's module fixed over time.
create or replace view module_roots as
select distinct directory(path) as root from changes
where regexp_matches(path, '(^|/)(Cargo\.toml|package\.json|pyproject\.toml)$')
  and directory(path) <> '';

-- Each path's module: its nearest module root, else its directory cut at the
-- module depth. Files at the repository root share module ''.
create or replace view path_modules as
select p.path, coalesce(
    arg_max(r.root, len(r.root)),
    array_to_string(list_slice(string_split(p.path, '/'), 1,
                    least(module_depth(), len(string_split(p.path, '/')) - 1)), '/')) as module
from (select distinct path from changes) p
left join module_roots r on starts_with(p.path, r.root || '/')
group by p.path;

-- Each path's role, by common conventions: vendored and test code first, then
-- `code` for files tally parsed, else `other` (docs, configuration, data).
create or replace view path_roles as
select path, case
    when regexp_matches(path, '(^|/)(vendor|vendored|third_party|third-party|thirdparty|deps?|external|node_modules)/')
      then 'vendored'
    when regexp_matches(path, '(^|/)(tests?|testing|__tests__|specs?|testdata|test_data|fixtures?|bench|benches|benchmarks?|e2e)/')
      or regexp_matches(path, '(^|/)(test_[^/]*|[^/]*[_.](test|tests|spec))\.[A-Za-z0-9]+$')
      then 'test'
    when parsed then 'code'
    else 'other' end as role
from (select c.path, bool_or(s.blob is not null) as parsed
      from changes c left join spans s on s.blob = c.blob and s.kind = 'file'
      group by c.path);

-- File versions as intervals [since, until), clamped to one past the newest commit.
-- A per-commit metric that sums per-blob facts adds them at `since` and removes
-- them at `until`, then takes a running sum over commits: its cost grows with
-- changes, not with commits times code size.
create or replace view versions as
  select path, blob, since, least(until, (select max(seq) + 1 from commits)) as until
  from files;

create or replace view commit_info as
with production as (
  select seq, count(*) as files from changes join path_roles using (path)
  where role = 'code' group by seq),
c as (
  select c.*, split_part(c.message, chr(10), 1) as subject, coalesce(p.files, 0) as production_files
  from commits c left join production p using (seq))
select seq, id, time, author, message, subject,
  -- A defect fix in production behaviour: a fix keyword or a defect word in the
  -- subject, production code touched, and no sign of a test, CI, docs or lint fix.
  -- Checked on 150 hand-labelled RediSearch commits (validation/): precision 0.86,
  -- recall 0.89, against 0.48 and 0.89 for the keyword alone (is_fix_keyword).
  (regexp_matches(subject, '(?i)\b(fix|fixes|fixed|bug|hotfix|regression|revert)')
     or regexp_matches(subject, '(?i)(crash|leak|race\b|deadlock|hang|segfault|overflow|corrupt|wrong|incorrect|asan|use.after.free|double.free|null.pointer|panic)'))
    and production_files > 0
    and not regexp_matches(subject, '(?i)(test|flak|\bci\b|lint|typo|\bdocs?\b|readme|cmake|\bbuild\b|review|indent|format|chore|contributing|\blink)')
    as is_fix,
  regexp_matches(subject, '(?i)\b(fix|fixes|fixed|bug|hotfix|regression|revert)') as is_fix_keyword,
  regexp_matches(message, '(?i)co-authored-by:.*(claude|copilot|cursor|codex|devin|aider)') as is_agent,
  regexp_matches(author, '(?i)\[bot\]') as is_bot
from c;

create or replace view fns as
select f.blob, f.lang, f.name, f.line, f.end_line, f.tokens, f.comments, f.params, f.hash,
  f.start_byte, f.end_byte,
  case when f.lang in ('c', 'cpp')
         then (f.blob, f.start_byte, f.end_byte) not in (select blob, start_byte, end_byte from spans where kind = 'static')
       when f.lang = 'python' then not starts_with(f.name, '_')
       else (f.blob, f.start_byte, f.end_byte) in (select blob, start_byte, end_byte from spans where kind = 'pub')
  end as public
from spans f where f.kind = 'fn';

-- Changes to production code, with their module.
create or replace view code_changes as
select x.seq, x.path, x.blob, m.module
from changes x join path_roles r using (path) join path_modules m using (path)
where r.role = 'code';

-- Commits that change production code, numbered in history order.
create or replace view code_commits as
select seq, row_number() over (order by seq) as n, any_value(c.time) as time
from code_changes join commits c using (seq) group by seq;

-- Process metrics are computed over trailing windows of window_commits() code
-- commits, read off once per ISO week at the week's last code commit, so every
-- point has the same sample size. `win` is the window's last commit; windows
-- spanning more than window_max_weeks() are left out.
create or replace view windows as
with ends as (
  select seq as win, n from code_commits
  where n >= window_commits()
    and n in (select max(n) from code_commits group by date_trunc('week', time))),
members as (
  select e.win, c.seq, c.time
  from ends e join code_commits c on c.n > e.n - window_commits() and c.n <= e.n)
select win, seq,
       date_diff('day', min(time) over (partition by win), max(time) over (partition by win)) / 7
         as span_weeks
from members
qualify span_weeks <= window_max_weeks();

-- Per production blob: additive structure facts, computed once per blob. A
-- function's nesting is its deepest block minus its body block (blocks, not
-- branches, so an else-if chain does not count as nesting).
create or replace view blob_facts as
with nesting as (
  select f.blob, any_value(f.tokens) as tokens, any_value(f.public) as public,
         coalesce(max(b.depth) - min(b.depth), 0) as nesting
  from fns f
  left join spans b on b.blob = f.blob and b.kind = 'block'
    and b.start_byte >= f.start_byte and b.end_byte <= f.end_byte
  group by f.blob, f.start_byte, f.end_byte),
functions as (
  select blob, sum(tokens) as fn_tokens, sum(tokens * nesting) as nested_tokens,
         coalesce(sum(tokens) filter (nesting > deep_nesting()), 0) as deep_tokens,
         coalesce(sum(tokens) filter (not public), 0) as private_tokens,
         coalesce(sum(tokens) filter (tokens > large_tokens()), 0) as large_tokens
  from nesting group by blob),
counts as (
  select blob, any_value(lang) as lang, 1 as files,
    coalesce(sum(tokens) filter (kind = 'file'), 0) as tokens,
    count(*) filter (kind in ('branch', 'condition')) as decisions,
    count(*) filter (kind = 'global_state') as global_state,
    count(*) filter (kind = 'throw') as throws,
    count(*) filter (kind = 'goto') as gotos
  from spans group by blob)
select c.*, coalesce(columns(f.* exclude (blob)), 0)
from counts c left join functions f using (blob);

-- Production-code totals per commit and language: each file version adds its
-- blob's facts at `since` and removes them at `until`; a running sum per language,
-- carried to every commit, gives the totals.
create or replace view code_totals as
with v as (
  select v.since, v.until, f.* exclude (blob)
  from versions v join path_roles r on r.path = v.path and r.role = 'code'
  join blob_facts f using (blob)),
deltas as (
  select seq, lang, sum(columns(* exclude (seq, lang))) from (
    select since as seq, lang, columns(* exclude (since, until, lang)) from v
    union all
    select until, lang, -columns(* exclude (since, until, lang)) from v)
  group by seq, lang),
running as (
  select seq, lang, sum(columns(* exclude (seq, lang))) over (partition by lang order by seq)
  from deltas),
grid as (select c.seq, l.lang from commits c, (select distinct lang from running) l)
select g.seq, g.lang, r.* exclude (seq, lang)
from grid g asof join running r on g.lang = r.lang and g.seq >= r.seq
where r.files > 0;

-- ============ imports ============
-- A path's language family by extension (the extensions of LANGS in
-- src/extract.rs): families import only each other's files. C goes with C++, whose
-- extensions include .h, and the ECMAScript dialects go together. By path, not by
-- the blob's `lang`: a blob is parsed once, under the first path that has it.
create or replace macro family(path) as
  case when regexp_matches(lower(path), '\.(c|h|cc|cpp|cxx|hh|hpp|hxx)$') then 'c'
       when regexp_matches(lower(path), '\.rs$') then 'rust'
       when regexp_matches(lower(path), '\.py$') then 'python'
       when regexp_matches(lower(path), '\.(ts|mts|cts|tsx|js|mjs|cjs|jsx)$') then 'ecma' end;

-- `dir/rest`, or `rest` at the repository root.
create or replace macro subpath(dir, rest) as case when dir = '' then rest else dir || '/' || rest end;

-- A relative path with `.` segments and up to four `..` segments resolved.
create or replace macro normalize(p) as
  trim(regexp_replace(regexp_replace(regexp_replace(regexp_replace(
    regexp_replace('/' || p || '/', '/(\./)+', '/', 'g'),
    '/[^/]+/\.\./', '/'), '/[^/]+/\.\./', '/'), '/[^/]+/\.\./', '/'), '/[^/]+/\.\./', '/'), '/');

-- Leading directories two paths share: how near a candidate is to its importer.
create or replace macro shared_dirs(a, b) as
  len(list_filter(range(1, least(len(string_split(a, '/')), len(string_split(b, '/')))),
                  lambda i: string_split(a, '/')[1:i] = string_split(b, '/')[1:i]));

-- Whether an import's syntax says it names a file of this repository, so failing
-- to find one is a miss rather than an external dependency.
create or replace macro internal_import(path, name) as
  case family(path) when 'c' then starts_with(name, '"')
                    when 'rust' then split_part(name, '::', 1) in ('crate', 'self', 'super')
                    when 'python' then starts_with(name, '.')
                    when 'ecma' then regexp_matches(name, '^\.\.?/') end;

-- Each path's lifetimes [since, until): runs of consecutive versions.
create or replace view path_life as
with v as (
  select path, since, until,
         since <> coalesce(lag(until) over (partition by path order by since), -1) as starts
  from versions)
select path, min(since) as since, max(until) as until
from (select *, sum(starts::int) over (partition by path order by since) as run from v)
group by path, run;

-- The imports of each blob, one row per target as written, with the Rust inline
-- modules around it (`a/b` inside `mod a { mod b { ... } }`).
create or replace view blob_imports as
select distinct blob, name, scope from (
  select i.blob, i.name, coalesce(string_agg(m.name, '/' order by m.start_byte), '') as scope
  from spans i
  left join spans m on m.blob = i.blob and m.kind = 'module'
    and m.start_byte < i.start_byte and i.end_byte <= m.end_byte
  where i.kind = 'import'
  group by i.blob, i.name, i.start_byte, i.end_byte);

-- Every import any production path ever made.
create or replace view import_keys as
select distinct v.path, i.name, i.scope
from versions v join path_roles r on r.path = v.path and r.role = 'code'
join blob_imports i using (blob);

-- The files each import could name, by language convention, ranked: a higher rank
-- wins; among equal ranks the candidate sharing the most directories with the
-- importer, then the shallowest (as an include path would find it). A file is a
-- candidate if it exists at any commit; import_status picks among those alive.
--   C/C++: "x.h" next to the includer, else any path ending in x.h (<x.h> too).
--   Python: the longest dotted prefix naming a module (a/b.py or a/b/__init__.py),
--     relative to the package for leading dots, else next to the importer or at
--     any path ending in it.
--   ECMAScript: relative specifiers with the usual extensions and index files.
--   Rust: crate::, self::, super:: from the crate's src/ and the importer's module
--     (inline modules included), a child module of the importer's module, or a
--     workspace crate by directory name; the longest prefix naming a module file
--     (a/b.rs or a/b/mod.rs), else the module's own file. `mod` declarations are
--     not imports.
create or replace view import_candidates as
with c as (
  select *, regexp_extract(name, '^["<](.*)[">]$', 1) as inc from import_keys where family(path) = 'c'),
py as (
  select *, len(regexp_extract(name, '^\.*')) as dots, string_split(ltrim(name, '.'), '.') as segs
  from import_keys where family(path) = 'python'),
py_j as (
  select py.*, j, array_to_string(segs[1:j], '/') as stem,
         normalize(directory(path) || repeat('/..', dots - 1)) as base
  from py, range(0, len(segs) + 1) r(j)),
es as (
  select *, normalize(directory(path) || '/' || name) as base
  from import_keys where family(path) = 'ecma' and regexp_matches(name, '^\.\.?/')),
rs as (
  select *, string_split(name, '::') as segs, rtrim(regexp_extract(path, '^(.*?/)?src/', 0), '/') as src,
         case when regexp_matches(path, '(^|/)(mod|lib|main)\.rs$') then directory(path)
              else regexp_replace(path, '\.rs$', '') end
           || case when scope = '' then '' else '/' || scope end as module
  from import_keys where family(path) = 'rust'),
crates as (
  select distinct directory(path) as dir, replace(regexp_extract(directory(path), '[^/]*$'), '-', '_') as crate
  from changes where regexp_matches(path, '/Cargo\.toml$')),
rs_bases as (
  select path, name, scope, src as base, segs[2:] as rest, 3 as tier from rs where segs[1] = 'crate' and src <> ''
  union all
  select path, name, scope, normalize(module || repeat('/..', len(list_filter(segs, lambda s: s = 'super')))),
         list_filter(segs, lambda s: s not in ('self', 'super')), 3
  from rs where segs[1] in ('self', 'super')
  union all
  select path, name, scope, module, segs, 2 from rs where segs[1] not in ('crate', 'self', 'super')
  union all
  select rs.path, rs.name, rs.scope, k.dir || '/src', segs[2:], 1 from rs join crates k on k.crate = rs.segs[1]),
exact as (
  select path, name, scope, normalize(subpath(directory(path), inc)) as target, 2 as rank, false as fallback
  from c where starts_with(name, '"')
  union all
  select path, name, scope, unnest(case when j = 0 then [subpath(base, '__init__.py')]
         else [subpath(base, stem) || '.py', subpath(base, stem) || '/__init__.py'] end), j * 10 + 2,
         j = 0 and segs <> ['']
  from py_j where dots > 0
  union all
  select path, name, scope, unnest([subpath(directory(path), stem) || '.py',
                                     subpath(directory(path), stem) || '/__init__.py']), j * 10 + 2, false
  from py_j where dots = 0 and j > 0
  union all
  select path, name, scope,
         unnest([base, regexp_replace(base, '\.js$', '.ts'), regexp_replace(base, '\.js$', '.tsx')]
                || list_transform(['.ts', '.tsx', '.d.ts', '.js', '.jsx', '.mjs', '.cjs', '/index.ts',
                                   '/index.tsx', '/index.js', '/index.jsx'], lambda e: base || e)), 1, false
  from es
  union all
  select path, name, scope, unnest(t), tier * 1000 + j * 10 + unnest(pref), j = 0 and len(rest) > 0 from (
    select b.*, j,
           case when j > 0 then [subpath(base, array_to_string(rest[1:j], '/')) || '.rs',
                                 subpath(base, array_to_string(rest[1:j], '/')) || '/mod.rs']
                else [base || '.rs', subpath(base, 'mod.rs'), subpath(base, 'lib.rs'), subpath(base, 'main.rs')] end as t,
           case when j > 0 then [1, 1] else [1, 1, 1, 0] end as pref  -- a crate's name means its lib.rs
    from rs_bases b, range(0, len(rest) + 1) r(j)
    where j > 0 or tier <> 2)),
suffix as (
  select path, name, scope, inc as suffix, 1 as rank, false as fallback from c
  union all
  select path, name, scope, unnest([stem || '.py', stem || '/__init__.py']), j * 10 + 1, false
  from py_j where dots = 0 and j > 0),
paths as (select path, regexp_extract(path, '[^/]*$') as file from path_roles)
select path, name, scope, target, max(rank) as rank, arg_max(fallback, rank) as fallback from (
  select e.path, e.name, e.scope, e.target, e.rank, e.fallback from exact e join paths p on p.path = e.target
  union all
  select s.path, s.name, s.scope, p.path, s.rank, s.fallback from suffix s
  join paths p on p.file = regexp_extract(s.suffix, '[^/]*$')
    and (p.path = s.suffix or ends_with(p.path, '/' || s.suffix)))
group by all;

-- Each production import over time: from `since` to `until`, `path` imports `name`
-- and it resolves to `target`. class: resolved (one best candidate), ambiguous
-- (several equally good), external (outside the repository by its syntax, resolved
-- to a vendored file, or a quoted C include under a directory the repository never
-- had, as from a git submodule) or unresolved (it names this repository but no file
-- matches). fallback: resolved only to a module's own file, because no file matches
-- the rest of the path (a Rust crate root for `crate::a::B` without a.rs, or
-- __init__.py for Python `from . import x`). Resolution changes only when candidate
-- files appear or disappear.
create or replace view import_status as
with cand as (
  select c.path, c.name, c.scope, c.target, c.rank, c.fallback, shared_dirs(c.path, c.target) as near,
         len(string_split(c.target, '/')) as depth, r.role = 'vendored' as vendored, l.since, l.until
  from import_candidates c join path_life l on l.path = c.target join path_roles r on r.path = c.target),
bounds as (
  select path, name, scope, since as b from cand
  union select path, name, scope, until from cand
  union select path, name, scope, 0 from import_keys),
segments as (
  select path, name, scope, b as since,
         coalesce(lead(b) over (partition by path, name, scope order by b), 2147483647) as until
  from bounds),
best as (
  select s.path, s.name, s.scope, s.since, c.target, c.vendored, c.fallback
  from segments s join cand c on c.path = s.path and c.name = s.name and c.scope = s.scope
    and c.since <= s.since and s.until <= c.until
  qualify rank() over (partition by s.path, s.name, s.scope, s.since
                       order by c.vendored, c.rank desc, c.near desc, c.depth) = 1),
resolution as (
  select s.path, s.name, s.scope, s.since, s.until, count(b.target) as n, any_value(b.target) as target,
         bool_or(b.vendored) as vendored, bool_or(b.fallback) as fallback
  from segments s left join best b using (path, name, scope, since)
  group by s.path, s.name, s.scope, s.since, s.until),
directories as (
  select distinct unnest(list_slice(string_split(path, '/'), 1, -2)) as dir from changes)
select v.path, family(v.path) as family, i.name, greatest(v.since, r.since) as since, least(v.until, r.until) as until,
  case when r.n = 1 and not r.vendored then r.target end as target,
  case when r.vendored then 'external'
       when r.n > 1 then 'ambiguous'
       when r.n = 1 then 'resolved'
       when family(v.path) = 'c' and regexp_matches(i.name, '^"[^/"]+/')
            and regexp_extract(i.name, '^"([^/]+)/', 1) not in (select dir from directories) then 'external'
       when internal_import(v.path, i.name) then 'unresolved' else 'external' end as class,
  r.n = 1 and not r.vendored and r.fallback as fallback
from versions v
join path_roles pr on pr.path = v.path and pr.role = 'code'
join blob_imports i on i.blob = v.blob
join resolution r on r.path = v.path and r.name = i.name and r.scope = i.scope
  and r.since < v.until and v.since < r.until;

-- Production files as nodes of the import graph, while they exist.
create or replace view graph_files as
select l.path, family(l.path) as family, m.module, l.since, l.until
from path_life l join path_roles r on r.path = l.path and r.role = 'code'
join path_modules m on m.path = l.path;

-- The import graph between production files as intervals: `source` imports
-- `target` at every commit in [since, until), through one import or more.
create or replace view import_edges as
with s as (
  select i.path as source, i.target, i.since, i.until
  from import_status i join path_roles r on r.path = i.target and r.role = 'code'
  where i.class = 'resolved' and i.target <> i.path),
d as (
  select source, target, seq, sum(delta) as delta
  from (select source, target, since as seq, 1 as delta from s
        union all select source, target, until, -1 from s)
  group by all),
flips as (
  select source, target, seq, n > 0 as present from (
    select *, sum(delta) over (partition by source, target order by seq) as n from d)
  qualify present <> coalesce(lag(present) over (partition by source, target order by seq), false))
select source, target, since, until from (
  select source, target, present, seq as since, lead(seq) over (partition by source, target order by seq) as until
  from flips)
where present;

-- Reachability per language family at every commit where it changes: production
-- files alive, the cones they reach (from `cones`, which Rust derives from
-- import_edges; one entry per file, 0 for files reaching nothing), their sum and
-- the largest strongly connected component.
create or replace view reach as
with points as (
  select since as seq from graph_files union select until from graph_files
  union select since from cones union select until from cones),
alive as (
  select p.seq, f.family, count(*) as files from points p
  join graph_files f on f.since <= p.seq and p.seq < f.until group by all),
reaching as (
  select p.seq, f.family, sum(c.cone) as total, max(c.scc) as largest_scc, list(c.cone) as cones
  from points p join cones c on c.since <= p.seq and p.seq < c.until
  join (select distinct path, family from graph_files) f using (path) group by all)
select a.seq, a.family, a.files, coalesce(r.total, 0) as total, coalesce(r.largest_scc, 1) as largest_scc,
       list_resize(coalesce(r.cones, []), a.files, 0) as cones
from alive a left join reaching r using (seq, family);

-- ============ metrics ============
create or replace view m_size as
select seq, sum(tokens) as value, sum(files) as files
from code_totals group by seq;

create or replace view m_hidden_coupling as
with c as (select * from code_changes
           qualify count(*) over (partition by seq) between 2 and max_commit_files()),
per_commit as (
  select a.seq, avg((a.module <> b.module)::int) as cross_share
  from c a join c b on a.seq = b.seq and a.path < b.path group by a.seq),
-- Chance level: the share of distinct-file pairs crossing modules if module labels
-- were shuffled among the files of the window's eligible commits (an exact null).
sizes as (select w.win, c.module, count(distinct c.path) as n from windows w join c using (seq) group by all),
expected as (
  select win, 1 - sum(n * (n - 1)) / (sum(n) * (sum(n) - 1)) as expected
  from sizes group by win),
observed as (
  select w.win, avg(p.cross_share) as observed, low(p.cross_share) as low, high(p.cross_share) as high,
         count(*) as commits, any_value(w.span_weeks) as span_weeks
  from windows w join per_commit p using (seq) group by w.win)
select o.win as seq, o.observed / nullif(e.expected, 0) as value,
       o.low / nullif(e.expected, 0) as low, o.high / nullif(e.expected, 0) as high,
       o.observed, e.expected, o.commits, o.span_weeks
from observed o join expected e using (win);

create or replace view m_rework as
with v as (
  select x.seq, x.blob,
         c.time - lag(c.time) over (partition by x.path order by x.seq) as age
  from code_changes x join commits c using (seq)),
per_commit as (
  select seq, avg((age <= to_days(rework_days()))::int) as reworked, count(*) as modifications
  from v where age is not null and blob is not null group by seq)
select w.win as seq, avg(reworked) as value,
       low(reworked) as low, high(reworked) as high,
       count(*) as commits, sum(modifications) as modifications, any_value(w.span_weeks) as span_weeks
from windows w join per_commit p using (seq) group by w.win;

-- Each non-bulk code commit has weight 1, split evenly over the files it changes.
create or replace view change_weights as
select seq, path, module, 1 / count(*) over (partition by seq) as w, count(*) over (partition by seq) as k
from code_changes
qualify count(*) over (partition by seq) <= max_commit_files();

create or replace view m_change_entropy as
with f as (select w.win, c.path, sum(c.w) as w from windows w join change_weights c using (seq) group by all),
p as (select win, w / sum(w) over (partition by win) as p from f),
commits as (select w.win, avg(k) as files_per_commit, count(*) as commits, any_value(w.span_weeks) as span_weeks
            from windows w join (select distinct seq, k from change_weights) using (seq) group by w.win)
select win as seq, 1 / sum(p * p) as value, any_value(c.files_per_commit) as files_per_commit,
       power(2, -sum(p * log2(p))) as effective_files_h, any_value(c.commits) as commits,
       any_value(c.span_weeks) as span_weeks
from p join commits c using (win) group by win;

create or replace view m_module_entropy as
with m as (select w.win, c.module, sum(c.w) as w from windows w join change_weights c using (seq) group by all),
p as (select win, w / sum(w) over (partition by win) as p from m),
live_modules as (
  select w.win, count(distinct pm.module) as modules
  from (select distinct win from windows) w
  join files f on f.since <= w.win and w.win < f.until
  join path_roles r on r.path = f.path and r.role = 'code'
  join path_modules pm on pm.path = f.path
  group by w.win)
select p.win as seq, 1 / sum(p * p) as value, any_value(l.modules) as modules,
       power(2, -sum(p * log2(p))) as effective_modules_h,
       any_value(c.commits) as commits, any_value(c.span_weeks) as span_weeks
from p join live_modules l using (win)
join (select win, count(*) as commits, any_value(span_weeks) as span_weeks from windows group by win) c using (win)
group by p.win;

create or replace view m_encapsulation as
select seq, sum(private_tokens) / sum(fn_tokens) as value,
       map(list(lang order by lang), list(private_tokens / fn_tokens order by lang)) as by_language
from code_totals where fn_tokens > 0 group by seq;

-- Duplicated tokens change only where a function structure's instance count or
-- tokens change: each change of a structure adds its new contribution and removes
-- its old one, as for code_totals.
create or replace view m_duplication as
with f as (
  select v.since, v.until, s.lang, s.hash, s.tokens
  from versions v join path_roles r on r.path = v.path and r.role = 'code'
  join spans s on s.blob = v.blob and s.kind = 'fn' and s.tokens >= clone_tokens()),
d as (
  select lang, hash, seq, sum(n) as n, sum(t) as t
  from (select lang, hash, since as seq, 1 as n, tokens as t from f
        union all select lang, hash, until, -1, -tokens from f)
  group by all),
structures as (
  select lang, hash, seq, t, if(n > 1, t, 0) as duplicated from (
    select lang, hash, seq, sum(n) over w as n, sum(t) over w as t
    from d window w as (partition by lang, hash order by seq))),
changes as (
  select lang, seq, sum(duplicated - previous_duplicated) as duplicated, sum(t - previous_t) as tokens
  from (select *, coalesce(lag(duplicated) over w, 0) as previous_duplicated,
               coalesce(lag(t) over w, 0) as previous_t
        from structures window w as (partition by lang, hash order by seq))
  group by lang, seq),
running as (
  select lang, seq, sum(duplicated) over w as duplicated, sum(tokens) over w as tokens
  from changes window w as (partition by lang order by seq)),
grid as (select c.seq, l.lang from commits c, (select distinct lang from running) l),
totals as (
  select g.seq, g.lang, r.duplicated, r.tokens
  from grid g asof join running r on g.lang = r.lang and g.seq >= r.seq
  where r.tokens > 0)
select seq, sum(duplicated) / sum(tokens) as value,
       map(list(lang order by lang), list(duplicated / tokens order by lang)) as by_language
from totals group by seq;

create or replace view m_concentration as
select seq, sum(large_tokens) / sum(fn_tokens) as value,
       map(list(lang order by lang), list(large_tokens / fn_tokens order by lang)) as by_language
from code_totals where fn_tokens > 0 group by seq;

create or replace view m_ownership as
with changes_by_author as (
  select w.win, x.path, c.author, count(*) as n
  from windows w join change_weights x using (seq) join commits c using (seq)
  group by all),
files as (
  select win, path, max(n) / sum(n) as top_share
  from changes_by_author group by win, path having sum(n) >= 2),
spans as (select win, any_value(span_weeks) as span_weeks from windows group by win)
select f.win as seq, avg(f.top_share) as value,
       low(f.top_share) as low, high(f.top_share) as high,
       count(*) as files, any_value(s.span_weeks) as span_weeks
from files f join spans s using (win) group by f.win;

create or replace view m_decisions as
select seq, 100 * sum(decisions) / nullif(sum(tokens), 0) as value,
       map(list(lang order by lang), list(100 * decisions / nullif(tokens, 0) order by lang)) as by_language
from code_totals group by seq;

create or replace view m_nesting as
select seq, sum(nested_tokens) / nullif(sum(fn_tokens), 0) as value,
       sum(deep_tokens) / nullif(sum(fn_tokens), 0) as deep_share,
       map(list(lang order by lang), list(nested_tokens / nullif(fn_tokens, 0) order by lang)) as by_language
from code_totals group by seq;

create or replace view m_global_state as
select seq, 1000 * sum(global_state) / nullif(sum(tokens), 0) as value,
       map(list(lang order by lang), list(1000 * global_state / nullif(tokens, 0) order by lang)) as by_language
from code_totals group by seq;

create or replace view m_exceptions as
select seq, 1000 * sum(throws) / nullif(sum(tokens), 0) as value,
       map(list(lang order by lang), list(1000 * throws / nullif(tokens, 0) order by lang)) as by_language
from code_totals group by seq;

create or replace view m_goto as
select seq, 1000 * sum(gotos) / nullif(sum(tokens), 0) as value,
       map(list(lang order by lang), list(1000 * gotos / nullif(tokens, 0) order by lang)) as by_language
from code_totals where lang in ('c', 'cpp') group by seq;

-- Dependency metrics build on a file's reach: the production files of its language
-- family it depends on through its imports and theirs (`cones`). Files import only
-- files of their own family, so each metric reports the families as columns (c,
-- rust, python, ecma) and combines them into `value`.
create or replace view m_dependency_reach as
with r as (
  select c.seq, f.* exclude (seq) from commits c
  join (select distinct family from reach) k on true
  asof join reach f on f.family = k.family and c.seq >= f.seq
  where f.files > 1),
families as (select *, total / (files * (files - 1)) as cost from r)
select seq, sum(total / (files - 1)) / sum(files) as value,
       sum(total) / sum(files) as mean_reach, list_aggregate(flatten(list(cones)), 'median') as median_reach,
       max(largest_scc) as largest_cycle, sum(files) as files,
       any_value(cost) filter (family = 'c') as c, any_value(cost) filter (family = 'rust') as rust,
       any_value(cost) filter (family = 'python') as python, any_value(cost) filter (family = 'ecma') as ecma
from families group by seq;

create or replace view m_cross_module_imports as
with m as (select distinct path, family, module from graph_files),
e as (
  select e.since, e.until, ms.family, ms.module as source, mt.module as target
  from import_edges e join m ms on ms.path = e.source join m mt on mt.path = e.target),
counts as (
  select family, seq, sum(n) as edges, sum(n * (source <> target)::int) as crossing from (
    select family, since as seq, source, target, 1 as n from e
    union all select family, until, source, target, -1 from e)
  group by all),
-- Chance level: a module's out-stubs times its in-stubs, summed, over edges squared,
-- is the share of edges staying inside modules if every import were rewired at
-- random within its family, keeping each module's import counts (a configuration
-- model). The sum changes only where a module's stubs change.
stubs as (
  select family, module, seq, sum(o) as o, sum(i) as i from (
    select family, source as module, since as seq, 1 as o, 0 as i from e
    union all select family, source, until, -1, 0 from e
    union all select family, target, since, 0, 1 from e
    union all select family, target, until, 0, -1 from e)
  group by all),
products as (
  select family, module, seq, sum(o) over w * sum(i) over w as p
  from stubs window w as (partition by family, module order by seq)),
within as (
  select family, seq, sum(p - coalesce(lag_p, 0)) as dp from (
    select *, lag(p) over (partition by family, module order by seq) as lag_p from products)
  group by all),
running as (
  select family, seq, sum(coalesce(edges, 0)) over w as edges, sum(coalesce(crossing, 0)) over w as crossing,
         sum(coalesce(dp, 0)) over w as within_stubs
  from counts full join within using (family, seq) window w as (partition by family order by seq)),
grid as (select c.seq, f.family from commits c, (select distinct family from running) f),
per_family as (
  select g.seq, g.family, r.edges, r.crossing / r.edges as observed,
         1 - r.within_stubs / (r.edges * r.edges) as expected
  from grid g asof join running r on g.family = r.family and g.seq >= r.seq
  where r.edges > 0),
lifts as (select *, observed / nullif(expected, 0) as lift from per_family)
select seq, sum(edges * lift) / sum(edges) filter (lift is not null) as value, sum(edges) as edges,
       any_value(lift) filter (family = 'c') as c, any_value(lift) filter (family = 'rust') as rust,
       any_value(lift) filter (family = 'python') as python, any_value(lift) filter (family = 'ecma') as ecma
from lifts group by seq;

-- A changed file's rank is the percentile of its reach among the live files of its
-- family (ties count half): 0.5 is a typical file whatever the reach sizes.
create or replace view m_changed_file_reach as
with t as (
  select distinct x.seq, x.path, family(x.path) as family
  from (select * from code_changes qualify count(*) over (partition by seq) <= max_commit_files()) x
  where x.blob is not null and x.seq in (select seq from windows)),
ranks as (
  select t.seq, coalesce(c.cone, 0) as cone,
         (len(list_filter(r.cones, lambda x: x < coalesce(c.cone, 0)))
          + len(list_filter(r.cones, lambda x: x = coalesce(c.cone, 0))) / 2) / r.files as percentile
  from t asof join reach r on r.family = t.family and t.seq >= r.seq
  left join cones c on c.path = t.path and c.since <= t.seq and t.seq < c.until
  where r.files > 1),
per_commit as (select seq, avg(percentile) as percentile, avg(cone) as cone from ranks group by seq)
select w.win as seq, avg(p.percentile) as value, low(p.percentile) as low, high(p.percentile) as high,
       avg(p.cone) as reach, count(*) as commits, any_value(w.span_weeks) as span_weeks
from windows w join per_commit p using (seq) group by w.win;

create or replace view m_import_coverage as
with d as (
  select seq, family, class, fallback, sum(n) as n from (
    select since as seq, family, class, fallback, 1 as n from import_status
    union all select until, family, class, fallback, -1 from import_status)
  group by all),
running as (
  select seq, family, class, fallback, sum(n) over (partition by family, class, fallback order by seq) as n
  from d),
grid as (select c.seq, k.* from commits c, (select distinct family, class, fallback from d) k),
per_family as (
  select g.seq, g.family,
         coalesce(sum(r.n) filter (g.class = 'resolved'), 0) as resolved,
         coalesce(sum(r.n) filter (g.fallback), 0) as fallback,
         coalesce(sum(r.n) filter (g.class <> 'external'), 0) as internal,
         sum(r.n) as imports
  from grid g asof join running r
    on g.family = r.family and g.class = r.class and g.fallback = r.fallback and g.seq >= r.seq
  group by g.seq, g.family having sum(r.n) > 0)
select seq, sum(resolved) / nullif(sum(internal), 0) as value,
       sum(fallback) / nullif(sum(resolved), 0) as fallback_share,
       1 - sum(internal) / sum(imports) as external_share, sum(imports) as imports,
       any_value(resolved / nullif(internal, 0)) filter (family = 'c') as c,
       any_value(resolved / nullif(internal, 0)) filter (family = 'rust') as rust,
       any_value(resolved / nullif(internal, 0)) filter (family = 'python') as python,
       any_value(resolved / nullif(internal, 0)) filter (family = 'ecma') as ecma
from per_family group by seq;

-- ============ explanations (stored in the database) ============
-- Metric comments open with a category, then What, Read and Trend paragraphs:
-- they are the Grafana panel descriptions.
comment on view path_modules is 'Each path''s module: the nearest directory that held a Cargo.toml, package.json or pyproject.toml at any commit (not the repository root), else the path''s directory cut at module_depth().';
comment on view path_roles is 'Each path''s role by common conventions: vendored (vendor, third_party, deps, node_modules, ...), test (tests/, spec/, fixtures/, benches/, test_*.py, *_test.*, *.spec.*), code (parsed by tally), or other. Tests inside source files (Rust #[cfg(test)] modules) count as code.';
comment on view commit_info is 'Commits with derived labels. is_fix: a defect fix in production behaviour (fix keyword or defect word, production code touched, no test/CI/docs/lint words); precision 0.86 and recall 0.89 on 150 hand-labelled commits, measured on the labels it was designed from. is_fix_keyword: the keyword alone (precision 0.48). is_agent: a co-author trailer names a coding agent; in squash merges it means an agent helped somewhere in the change. is_bot: the author is a [bot] account.';
comment on view fns is 'One row per function span with a syntactic visibility flag. public: Rust pub, JS/TS export, C/C++ not static, Python no leading underscore. A proxy for API surface, not proof of external use. Markers match functions by byte range.';

comment on view m_size is 'SIZE. What: production code at each commit, in tokens (value) and files; tests, vendored and non-code files excluded.

Read: context, not a score. A step marks a bulk commit, which often moves other charts too.

Trend: size is the strongest single predictor of defects: judge the other trends against it.';

comment on view m_hidden_coupling is 'CHANGES. What: how often files changed in the same commit sit in different modules, relative to chance, over the last 100 code commits each week. Lower is better.

Read: 1 means module boundaries make no difference to what changes together; below 1, modules change independently. Dashed lines span two standard errors.

Trend: rising means modules must change together more, through coupling no import shows. Change coupling correlates with defects.';

comment on view m_rework is 'CHANGES. What: the share of edits to files already changed in the previous 7 days, over the last 100 code commits each week. Lower is better.

Read: 0.2 means a fifth of a typical commit''s edits rework last week''s code. Expect high values during feature work.

Trend: rising means code does not settle. A faster commit pace raises it too (see span_weeks). Unvalidated.';

comment on view m_change_entropy is 'ENTROPY. What: over how many files the work of the last 100 code commits is effectively spread, each week; each commit counts once.

Read: 12 means as if spread evenly over 12 files. Files per commit is shown because bigger commits spread work by themselves.

Trend: rising means more places change at once. It predicted faults well in open-source projects (Hassan).';

comment on view m_module_entropy is 'ENTROPY. What: over how many modules the work of the last 100 code commits is effectively spread, each week.

Read: 3 means as if spread evenly over 3 modules; it cannot exceed the module count (right axis).

Trend: rising while the module count holds means work scatters; rising with it may only mean the code was split.';

comment on view m_encapsulation is 'STRUCTURE. What: the share of function code in non-public functions (no pub, static, not exported, leading underscore). Higher is better.

Read: 0.6 means 60% of function code is internal. Visibility is written differently per language: compare by_language over time.

Trend: falling means the public surface grows faster than the code behind it.';

comment on view m_duplication is 'STRUCTURE. What: the share of function code (functions of 50+ tokens) whose structure occurs more than once, ignoring names and literals. Lower is better.

Read: 0.05 means 5% of it has a structural twin.

Trend: rising means copy-paste instead of reuse. Whole-function clones only. Unvalidated.';

comment on view m_concentration is 'STRUCTURE. What: the share of function code in functions over 200 tokens. Lower is better.

Read: 0.5 means half of it sits in large functions. Compare by_language over time.

Trend: rising means logic piles up in a few large functions. Unvalidated.';

comment on view m_ownership is 'CHANGES. What: for files changed at least twice in the last 100 code commits, the share of changes made by their most active author, each week. Higher is better.

Read: 0.8 means a typical file''s main author made 80% of its changes. Dashed lines span two standard errors.

Trend: falling means more people change code nobody owns, which went with more defects (Bird et al.). Squash merges credit the merger.';

comment on view m_decisions is 'COMPLEXITY. What: decision points (if, loops, match arms, catch, ternaries, &&, ||) per 100 production tokens.

Read: 5 means one decision every 20 tokens. Languages differ: compare by_language over time.

Trend: rising means more paths to understand and test per line. Unvalidated.';

comment on view m_nesting is 'COMPLEXITY. What: how many blocks deep function code sits below the function body, weighted by tokens; deep_share is the code in functions nested more than 3 deep.

Read: 1.5 means typical code is one to two blocks deep. Else-if chains stay flat.

Trend: rising means more conditions to keep in mind while reading. Unvalidated.';

comment on view m_global_state is 'NONLOCAL. What: declarations of global mutable state per 1,000 production tokens: Rust static mut, C/C++ file-scope variables, Python global, JS/TS module-level let and var.

Read: the forms differ: compare by_language over time.

Trend: rising means behaviour depends on more state outside the code in view.';

comment on view m_exceptions is 'NONLOCAL. What: non-local exits per 1,000 production tokens: throw, raise, Rust panics, unwrap and expect, C longjmp.

Read: in Rust mostly unwrap and expect; elsewhere throws a distant caller must handle. Compare by_language.

Trend: rising means control leaves functions in more places a caller cannot see. Unvalidated.';

comment on view m_goto is 'NONLOCAL. What: goto statements per 1,000 C/C++ tokens.

Read: mostly the goto-cleanup idiom, which is benign: watch spikes, not the level.

Trend: rising outside error cleanup means jumps that defeat top-to-bottom reading. Unvalidated.';

comment on view m_dependency_reach is 'DEPENDENCIES. What: the share of the other files of its language that a typical production file depends on, through its imports and theirs (its reach), per language (c, rust, python, ecma). Lower is better.

Read: 0.1 means a file depends on a tenth of its language''s code. mean_reach and median_reach count files; largest_cycle is the largest group of files importing each other.

Trend: rising means a change anywhere can affect, and requires understanding, more of the code (MacCormack et al.).';

comment on view m_cross_module_imports is 'DEPENDENCIES. What: how often imports cross module boundaries, relative to chance, per language (c, rust, python, ecma). Lower is better.

Read: 1 means modules make no difference to what imports what; below 1, files mostly import within their module.

Trend: rising means modules lean on each other more and stop matching the code''s structure. Steps when modules are split or merged.';

comment on view m_changed_file_reach is 'DEPENDENCIES. What: how the files changed by commits rank among the files of their language by reach (the files they depend on), over the last 100 code commits each week.

Read: 50% means work lands on typical files; 70% means on files that depend on more than 70% of their peers do. Active files usually rank high: read the trend. reach is their mean reach in files.

Trend: rising means work concentrates on code that needs more context to change safely. Unvalidated.';

comment on view m_import_coverage is 'DEPENDENCIES. What: the share of imports naming this repository that resolve to a file, per language (c, rust, python, ecma). External code is left out.

Read: the dependency charts miss what this misses. fallback_share counts imports resolved only to a crate root or __init__.py, which overstates reach.

Trend: a drop means the dependency charts read too low, not that the code changed.';
