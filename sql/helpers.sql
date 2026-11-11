-- Tuning constants and the helpers the metrics build on.

-- ============ tuning ============
create or replace macro window_commits() as 100; -- code-touching commits per trailing window
create or replace macro window_max_weeks() as 26; -- windows spanning longer are not reported
create or replace macro max_commit_files() as 30; -- larger commits are bulk changes, left out of process metrics
create or replace macro rework_days() as 7;     -- days within which a repeat change counts as rework
create or replace macro clone_tokens() as 50;   -- smallest function counted by duplication
create or replace macro large_tokens() as 200;  -- functions above this count toward concentration
create or replace macro deep_nesting() as 3;    -- functions nested deeper than this count as deeply nested

-- ============ helpers ============
-- A mean's 95% confidence band: two standard errors.
create or replace macro low(x) as avg(x) - 2 * stddev_samp(x) / sqrt(count(x));
create or replace macro high(x) as avg(x) + 2 * stddev_samp(x) / sqrt(count(x));

create or replace macro directory(path) as
  array_to_string(list_slice(string_split(path, '/'), 1, -2), '/');

-- Directories above a path: 0 at the root.
create or replace macro depth(path) as len(string_split(path, '/')) - 1;

-- The directories containing a path, outermost first.
create or replace macro ancestors(path) as
  list_transform(range(1, len(string_split(path, '/'))),
                 lambda i: array_to_string(string_split(path, '/')[1:i], '/'));

create or replace macro shared_dirs(a, b) as
  len(list_filter(range(1, least(len(string_split(a, '/')), len(string_split(b, '/')))),
                  lambda i: string_split(a, '/')[1:i] = string_split(b, '/')[1:i]));

-- Directory steps between two files: 0 in one directory, 2 in siblings.
create or replace macro tree_distance(a, b) as depth(a) + depth(b) - 2 * shared_dirs(a, b);

-- A path's language family by extension (LANGS in languages.rs), NULL if not code.
-- By path, not by the blob's `lang`: a blob is parsed once, under its first path.
create or replace macro family(path) as
  case when regexp_matches(lower(path), '\.(c|h|cc|cpp|cxx|hh|hpp|hxx)$') then 'c'
       when regexp_matches(lower(path), '\.rs$') then 'rust'
       when regexp_matches(lower(path), '\.py$') then 'python'
       when regexp_matches(lower(path), '\.(ts|mts|cts|tsx|js|mjs|cjs|jsx)$') then 'ecma' end;

-- Each path's role, from its newest version: what .gitattributes declare (`code`:
-- first-party whatever the conventions), else conventions, case aside.
create or replace view path_roles as
with p as (
  select c.path,
         arg_max(c.attr, c.seq) filter (c.blob is not null) as attr,
         bool_or(s.tokens is not null) as parsed,
         arg_max(g.blob is not null, c.seq) filter (c.blob is not null) as marked
  from changes c
  left join spans s on s.blob = c.blob and s.kind = 'file'
  left join spans g on g.blob = c.blob and g.kind = 'generated'
  group by c.path),
conventions as (
  select path, attr, case
    when coalesce(attr, '') <> 'code'
         and regexp_matches(path, '(?i)(^|/)(vendor|vendored|third_party|third-party|thirdparty|deps?|external|node_modules|bower_components)/')
      then 'vendored'
    when coalesce(attr, '') <> 'code'
         and (marked or regexp_matches(path, '(?i)(\.min\.(js|mjs|cjs|css)$|(^|/)(dist|generated|__generated__)/|_pb2(_grpc)?\.py$|\.pb\.(h|cc|c|go|ts|js)$|[._]generated\.[a-z]+$)'))
      then 'generated'
    when coalesce(attr, '') <> 'code'
         and regexp_matches(path, '^([Dd]ocs?|[Dd]ocumentation|[Ee]xamples|[Dd]emos?|[Ss]amples?)/')
      then 'documentation'
    when regexp_matches(path, '(?i)(^|/)([^/]*[-_])?(tests?|testing|testsuite|__tests__|__mocks__|specs?|testdata|test_data|fixtures?|bench|benches|benchmarks?|e2e|fuzz)/')
      or regexp_matches(path, '(?i)(^|/)(test_[^/]*|[^/]*[-_.](test|tests|spec))\.[A-Za-z0-9]+$')
      then 'test'
    when parsed and family(path) is not null then 'code'
    else 'other' end as role
  from p)
select path, if(attr is not null and attr <> 'code', attr, role) as role from conventions;

comment on view path_roles is 'Each path''s role: code (production), test, vendored, generated, documentation or other. What the repository''s .gitattributes declare wins (linguist-vendored, linguist-generated, linguist-documentation, tally=<role>); otherwise path conventions, generation markers and minified JavaScript decide. Tests inside source files (Rust #[cfg(test)] modules) count as code.';

-- File versions as [since, until). Per-commit totals add a version's facts at since,
-- remove them at until and sum: cost grows with changes, not commits times code.
create or replace view versions as
  select path, blob, since, least(until, (select max(seq) + 1 from commits)) as until
  from files;

-- Commit times for windows and ages: each the median of itself and its neighbours
-- (a lone wrong clock does not count), never earlier than the one before.
create or replace view commit_times as
select seq, max(t) over (order by seq rows unbounded preceding) as time from (
  select seq, greatest(least(a, b), least(greatest(a, b), c)) as t from (
    select seq, coalesce(lag(time) over w, time) as a, time as b, coalesce(lead(time) over w, time) as c
    from commits window w as (order by seq)));

create or replace view code_changes as
select x.seq, x.path, x.blob
from changes x join path_roles r using (path)
where r.role = 'code';

create or replace view code_commits as
select seq, row_number() over (order by seq) as n, any_value(c.time) as time
from code_changes join commit_times c using (seq) group by seq;

-- Trailing windows of window_commits() code commits, read at each ISO week's last
-- code commit; windows spanning over window_max_weeks() are left out.
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

-- Additive structure facts per blob. Nesting: deepest block minus the body block.
create or replace view blob_facts as
with nesting as (
  select f.blob, any_value(f.tokens) as tokens,
         -- Syntactic visibility: pub/export markers, C/C++ not static, Python no `_`.
         any_value(case when f.lang in ('c', 'cpp')
                          then (f.blob, f.start_byte, f.end_byte) not in (select blob, start_byte, end_byte from spans where kind = 'static')
                        when f.lang = 'python' then not starts_with(f.name, '_')
                        else (f.blob, f.start_byte, f.end_byte) in (select blob, start_byte, end_byte from spans where kind = 'pub')
                   end) as public,
         coalesce(max(b.depth) - min(b.depth), 0) as nesting
  from spans f
  left join spans b on b.blob = f.blob and b.kind = 'block'
    and b.start_byte >= f.start_byte and b.end_byte <= f.end_byte
  where f.kind = 'fn'
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

-- Production totals per commit and language, from blob_facts by change points.
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
