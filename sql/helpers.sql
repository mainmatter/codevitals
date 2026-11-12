-- Tuning constants and the helpers the metrics build on.

-- ============ tuning ============
create or replace macro window_commits() as 100; -- code-touching commits per trailing window
create or replace macro window_max_weeks() as 26; -- windows spanning longer are not reported
create or replace macro max_commit_files() as 30; -- larger commits are bulk changes, left out of process metrics
create or replace macro rework_days() as 7;     -- days within which a repeat change counts as rework
create or replace macro clone_tokens() as 50;   -- smallest function counted by duplication
create or replace macro large_tokens() as 200;  -- functions above this count toward concentration
create or replace macro deep_nesting() as 3;    -- functions nested deeper than this count as deeply nested
-- Cargo features whose code counts as test code by name alone (more may be declared
-- per path in .gitattributes: codevitals-test-features=a,b).
create or replace macro test_feature(f) as
  regexp_full_match(f, '(?i)(_*tests?|testing|test[-_]?(utils?|support|helpers?|fixtures?|mocks?)|__.*test.*)');

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

-- A parsed language's family (`lang` of `spans`, `name` of LANGS), as `family` gives it.
create or replace macro lang_family(lang) as
  case when lang in ('c', 'cpp') then 'c'
       when lang in ('rust', 'python') then lang
       when lang in ('typescript', 'tsx', 'javascript') then 'ecma' end;

-- The ratio sum(num) / sum(den) over one family's rows of a per-language aggregate,
-- such as `code_totals` grouped by seq: a metric's column for that family.
create or replace macro family_ratio(num, den, lang, fam) as
  sum(num) filter (lang_family(lang) = fam) / nullif(sum(den) filter (lang_family(lang) = fam), 0);

-- Whether a `test_if` span's code is test code: `gate` (conjunctions joined by `|`,
-- features by `&`) holds when every feature of some conjunction is test-only, by
-- name or because `declared` (features joined by `,`) lists it.
create or replace macro test_gate(gate, declared) as
  list_bool_or(list_transform(string_split(gate, '|'), lambda c:
    list_bool_and(list_transform(string_split(c, '&'), lambda f:
      test_feature(f) or list_contains(string_split(declared, ','), f)))));

-- Every blob with the test features declared where it occurs ('' for none): blob
-- facts depend on both.
create or replace view blob_inputs as
select distinct blob, test_features from files;

-- The outermost ranges of each blob compiled only for tests (Rust #[cfg(test)],
-- #[test] functions, code behind test-only features), under its declared features.
create or replace view test_ranges as
with t as (
  select distinct i.blob, i.test_features, s.start_byte, s.end_byte, s.tokens
  from blob_inputs i join spans s on s.blob = i.blob
  where s.kind = 'test' or (s.kind = 'test_if' and test_gate(s.name, i.test_features)))
select * from t a
where not exists (
  select 1 from t b
  where b.blob = a.blob and b.test_features = a.test_features
    and b.start_byte <= a.start_byte and b.end_byte >= a.end_byte
    and (b.start_byte < a.start_byte or b.end_byte > a.end_byte));

-- Spans of production code: every span outside the blob's test ranges (the `file`
-- span always stays), per blob and declared test features.
create or replace view code_spans as
select i.test_features, s.*
from blob_inputs i join spans s on s.blob = i.blob
where s.kind not in ('test', 'test_if')
  and (s.kind = 'file' or not exists (
    select 1 from test_ranges t
    where t.blob = s.blob and t.test_features = i.test_features
      and t.start_byte <= s.start_byte and s.end_byte <= t.end_byte));

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
-- Rust module files declared only for tests (`#[cfg(test)] mod x;`) by the newest
-- version of a file:
-- x.rs or x/mod.rs beside a mod.rs, lib.rs or main.rs, else in a directory named
-- after the declaring file.
test_modules as (
  select distinct case when regexp_matches(f.path, '(^|/)(mod|lib|main)\.rs$')
                       then concat_ws('/', nullif(directory(f.path), ''), m.name)
                       else regexp_replace(f.path, '\.rs$', '') || '/' || m.name end as module
  from files f
  join spans m on m.blob = f.blob and m.kind = 'mod_decl'
  join test_ranges t on t.blob = f.blob and t.test_features = f.test_features
    and t.start_byte <= m.start_byte and m.end_byte <= t.end_byte
  where f.since = (select max(since) from files g where g.path = f.path)),
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
      or regexp_replace(path, '(/mod)?\.rs$', '') in (select module from test_modules)
      then 'test'
    when parsed and family(path) is not null then 'code'
    else 'other' end as role
  from p)
select path, if(attr is not null and attr <> 'code', attr, role) as role from conventions;

comment on view path_roles is 'Each path''s role: code (production), test, vendored, generated, documentation or other. What the repository''s .gitattributes declare wins (linguist-vendored, linguist-generated, linguist-documentation, codevitals=<role>); otherwise path conventions, generation markers and minified JavaScript decide. A Rust file declared only for tests (#[cfg(test)] mod x;) is test; test code inside a production file is subtracted from its facts (code_spans).';

-- File versions as [since, until). Per-commit totals add a version's facts at since,
-- remove them at until and sum: cost grows with changes, not commits times code.
create or replace view versions as
  select path, blob, test_features, since, least(until, (select max(seq) + 1 from commits)) as until
  from files;

-- Commit times for windows and ages: each the median of itself and its neighbours
-- (a lone wrong clock does not count), never earlier than the one before.
create or replace view commit_times as
select seq, max(t) over (order by seq rows unbounded preceding) as time from (
  select seq, greatest(least(a, b), least(greatest(a, b), c)) as t from (
    select seq, coalesce(lag(time) over w, time) as a, time as b, coalesce(lead(time) over w, time) as c
    from commits window w as (order by seq)));

-- A file's lineage: one id for a file across renames, `born:path` of the version
-- that started it. A path's life runs from a change that gives it a blob after none
-- to its deletion; a life that starts as a rename (`source`) continues the lineage
-- of the source's life, which the same commit ends.
create or replace view lineages as
with recursive c as (
  select seq, path, blob, source,
         blob is not null and lag(blob) over (partition by path order by seq) is null as starts
  from changes),
rows as (
  select seq, path, blob, source,
         max(seq) filter (starts) over (partition by path order by seq rows unbounded preceding) as born
  from c),
lives as (
  select l.path, l.born, l.source, s.born as source_born
  from rows l left join rows s on s.path = l.source and s.seq = l.seq and s.blob is null
  where l.seq = l.born),
lineage as (
  select path, born, born || ':' || path as lineage from lives where source_born is null
  union all
  select l.path, l.born, p.lineage
  from lineage p join lives l on l.source = p.path and l.source_born = p.born)
select r.seq, r.path, r.blob, r.source, g.lineage
from rows r join lineage g using (path, born);

-- Changes to production code, each with its file's lineage. The deletion that a
-- rename implies is no change of its own: the lineage changes once, at its new path.
create or replace view code_changes as
select x.seq, x.path, x.blob, x.lineage
from lineages x join path_roles r using (path)
where r.role = 'code'
  and not (x.blob is null and (x.seq, x.path) in (select seq, source from changes where source is not null));

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

-- Additive structure facts per blob and declared test features, production code only
-- (code_spans). Nesting: deepest block minus the body block.
create or replace view blob_facts as
with nesting as (
  select f.blob, f.test_features, any_value(f.tokens) as tokens,
         -- Syntactic visibility: pub/export markers, C/C++ not static, Python no `_`.
         any_value(case when f.lang in ('c', 'cpp')
                          then (f.blob, f.start_byte, f.end_byte) not in (select blob, start_byte, end_byte from spans where kind = 'static')
                        when f.lang = 'python' then not starts_with(f.name, '_')
                        else (f.blob, f.start_byte, f.end_byte) in (select blob, start_byte, end_byte from spans where kind = 'pub')
                   end) as public,
         coalesce(max(b.depth) - min(b.depth), 0) as nesting
  from code_spans f
  left join spans b on b.blob = f.blob and b.kind = 'block'
    and b.start_byte >= f.start_byte and b.end_byte <= f.end_byte
  where f.kind = 'fn'
  group by f.blob, f.test_features, f.start_byte, f.end_byte),
functions as (
  select blob, test_features, sum(tokens) as fn_tokens, sum(tokens * nesting) as nested_tokens,
         coalesce(sum(tokens) filter (nesting > deep_nesting()), 0) as deep_tokens,
         coalesce(sum(tokens) filter (not public), 0) as private_tokens,
         coalesce(sum(tokens) filter (tokens > large_tokens()), 0) as large_tokens
  from nesting group by blob, test_features),
tests as (
  select blob, test_features, sum(tokens) as test_tokens from test_ranges group by all),
counts as (
  select blob, test_features, any_value(lang) as lang,
    coalesce(sum(tokens) filter (kind = 'file'), 0) as tokens,
    count(*) filter (kind in ('branch', 'condition')) as decisions,
    count(*) filter (kind = 'global_state') as global_state,
    count(*) filter (kind = 'throw') as throws,
    count(*) filter (kind = 'goto') as gotos
  from code_spans group by blob, test_features)
select c.blob, c.test_features, c.lang,
       -- A file that is test code throughout is no production file.
       if(c.tokens > 0 and c.tokens = coalesce(t.test_tokens, 0), 0, 1) as files,
       c.tokens - coalesce(t.test_tokens, 0) as tokens,
       c.* exclude (blob, test_features, lang, tokens),
       coalesce(columns(f.* exclude (blob, test_features)), 0)
from counts c left join functions f using (blob, test_features)
left join tests t using (blob, test_features);

-- Production totals per commit and language, from blob_facts by change points.
create or replace view code_totals as
with v as (
  select v.since, v.until, f.* exclude (blob, test_features)
  from versions v join path_roles r on r.path = v.path and r.role = 'code'
  join blob_facts f using (blob, test_features)),
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
