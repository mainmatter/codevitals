-- Tuning constants and the helpers the metrics build on.

-- ============ tuning ============
create or replace macro window_commits() as 100; -- code-touching commits per trailing window
create or replace macro window_max_weeks() as 26; -- windows spanning longer are not reported
create or replace macro max_commit_files() as 30; -- larger commits are bulk changes, left out of process metrics
create or replace macro rework_days() as 7;     -- days within which a repeat change counts as rework
create or replace macro large_tokens() as 200;  -- functions above this count toward concentration
create or replace macro deep_nesting() as 3;    -- functions nested deeper than this count as deeply nested
create or replace macro many_decisions() as 10;  -- functions with more decision points count as branchy
create or replace macro many_params() as 5;      -- functions with more parameters count as wide
create or replace macro hotspot_days() as 365; -- trailing period whose changes hotspots count
create or replace macro defect_window_days() as 182; -- windows of the defect check in cv validate
create or replace macro orphan_months() as 12;   -- an author without a commit for this long has left
create or replace macro cochange_min() as 3;      -- file pairs changed together this often in a window are coupled
create or replace macro small_file_tokens() as 300; -- new files below this are small (routing around)
create or replace macro stale_days() as 365;      -- files unchanged this long are long unchanged
create or replace macro fix_days() as 30;         -- a fix this soon after a touch follows it (risk per touch)
create or replace macro accretion_touches() as 3; -- files touched fewer times in a window are left out of accretion
create or replace macro orphaned_share() as 0.5; -- files with more of their changes by departed authors are orphaned
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

-- The spans of `kinds` outside the test ranges of their blob (a `file` span always
-- stays), per blob and declared test features. A left anti-join: a correlated
-- `not exists` plans as heavy mark joins.
create or replace macro production_spans(kinds) as table
select s.* exclude (end_byte_t)
from (select i.test_features, s.*, t.end_byte as end_byte_t
      from blob_inputs i join spans s on s.blob = i.blob
      left join test_ranges t on s.kind <> 'file' and t.blob = s.blob
        and t.test_features = i.test_features
        and t.start_byte <= s.start_byte and s.end_byte <= t.end_byte
      where list_contains(kinds, s.kind)) s
where end_byte_t is null;

-- The production spans the facts read (function_facts, blob_facts and the
-- experimental metrics); calls, blocks and imports are left out, as they are many.
create or replace view code_spans as
select * from production_spans(['file', 'fn', 'scope', 'module', 'branch', 'condition', 'throw',
  'goto', 'global_state', 'invariant', 'unhandled', 'unfinished', 'forward', 'doc', 'vocabulary',
  'marker']);

-- The role a path's name gives it, whatever its content: vendored, generated,
-- documentation or test, unless .gitattributes declare it code (`attr`); NULL when
-- the name says nothing. Parsing skips blobs whose every path has such a role.
create or replace macro path_convention(path, attr) as case
  when coalesce(attr, '') <> 'code'
       and regexp_matches(path, '(?i)(^|/)(vendor|vendored|third_party|third-party|thirdparty|deps?|external|node_modules|bower_components)/')
    then 'vendored'
  when coalesce(attr, '') <> 'code'
       and regexp_matches(path, '(?i)(\.min\.(js|mjs|cjs|css)$|(^|/)(dist|generated|__generated__)/|_pb2(_grpc)?\.py$|\.pb\.(h|cc|c|go|ts|js)$|[._]generated\.[a-z]+$)')
    then 'generated'
  when coalesce(attr, '') <> 'code'
       and regexp_matches(path, '^([Dd]ocs?|[Dd]ocumentation|[Ee]xamples|[Dd]emos?|[Ss]amples?)/')
    then 'documentation'
  when regexp_matches(path, '(?i)(^|/)([^/]*[-_])?(tests?|testing|testsuite|__tests__|__mocks__|specs?|testdata|test_data|fixtures?|bench|benches|benchmarks?|e2e|fuzz)/')
    or regexp_matches(path, '(?i)(^|/)(test_[^/]*|[^/]*[-_.](test|tests|spec))\.[A-Za-z0-9]+$')
    then 'test' end;

-- Blobs worth parsing: those with a path that may hold production code (neither its
-- newest .gitattributes role nor its name rules that out). Test, vendored, generated
-- and documentation files feed no metric.
create or replace view parse_candidates as
with newest as (
  select path, arg_max(attr, seq) filter (blob is not null) as attr from changes group by path)
select distinct c.blob
from changes c join newest n using (path)
where c.blob is not null
  and coalesce(n.attr, 'code') = 'code' and path_convention(c.path, n.attr) is null;

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
    when path_convention(path, attr) = 'vendored' then 'vendored'
    when coalesce(attr, '') <> 'code' and marked then 'generated'
    when path_convention(path, attr) is not null then path_convention(path, attr)
    when regexp_replace(path, '(/mod)?\.rs$', '') in (select module from test_modules) then 'test'
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

-- Each commit's author after .mailmap, as git applies it: an entry naming the commit's
-- name and email wins over one naming its email alone (both case-insensitive); later
-- entries win over earlier ones. `identity` is the lowercased email, which tells
-- authors apart.
create or replace view authors as
with entries as (
  select lower(commit_email) as commit_email, lower(commit_name) as commit_name,
         arg_max(proper_name, line) filter (proper_name is not null) as proper_name,
         arg_max(proper_email, line) filter (proper_email is not null) as proper_email
  from mailmap group by all),
resolved as (
  select c.seq, c.author_name, c.author_email,
         coalesce(n.proper_name, if(n.commit_email is null, e.proper_name, null), c.author_name) as name,
         coalesce(n.proper_email, if(n.commit_email is null, e.proper_email, null), c.author_email) as email
  from commits c
  left join entries n on n.commit_email = lower(c.author_email) and n.commit_name = lower(c.author_name)
  left join entries e on e.commit_email = lower(c.author_email) and e.commit_name is null)
select seq, name, email, lower(email) as identity,
       regexp_matches(name || ' ' || email, '(?i)\[bot\]') as is_bot
from resolved;

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

-- Production file versions [since, until), with their lineage: what every
-- per-version fact of production code joins.
create or replace view code_versions as
select v.path, l.lineage, v.blob, v.test_features, v.since, v.until
from versions v join path_roles r on r.path = v.path and r.role = 'code'
join lineages l on l.path = v.path and l.seq = v.since;

-- How many commits each author identity made to each file lineage: a rater's
-- familiarity with the functions shown to them (labels).
create or replace view author_file_commits as
select a.identity, x.lineage, count(distinct x.seq) as commits
from lineages x join authors a using (seq)
group by all;

-- Each non-bulk code commit weighs 1, split over its files (lineages: a renamed file
-- stays one file).
create or replace view change_weights as
select seq, path, lineage, 1 / count(*) over (partition by seq) as w, count(*) over (partition by seq) as k
from code_changes
qualify count(*) over (partition by seq) <= max_commit_files();

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

-- Each production function of each blob (per declared test features): its
-- qualified name (the scopes, modules and functions around it, then its own name;
-- `?` when it has none), its ordinal among functions of that qualified name in the
-- blob, and its facts. Decisions, throws and gotos count in the innermost function
-- around them; nesting is the deepest block minus the body block, inner functions'
-- blocks included.
create or replace view function_facts as
with fns as (
  select blob, test_features, lang, start_byte, end_byte, coalesce(name, '?') as name,
         line, end_line, tokens, params, hash
  from code_spans where kind = 'fn'),
qualified as (
  select f.blob, f.test_features, f.start_byte, f.end_byte,
         string_agg(coalesce(s.name, '?'), '::' order by s.start_byte, s.end_byte desc) as outer_names
  from fns f join code_spans s
    on s.blob = f.blob and s.test_features = f.test_features and s.kind in ('scope', 'module', 'fn')
   and s.start_byte <= f.start_byte and f.end_byte <= s.end_byte
   and (s.start_byte, s.end_byte) <> (f.start_byte, f.end_byte)
  group by all),
innermost as (
  select s.blob, s.test_features, s.kind, s.name, s.lang, f.start_byte, f.end_byte
  from code_spans s join fns f
    on f.blob = s.blob and f.test_features = s.test_features
   and f.start_byte <= s.start_byte and s.end_byte <= f.end_byte
  where s.kind in ('branch', 'condition', 'throw', 'goto', 'invariant', 'unhandled', 'unfinished')
  qualify row_number() over (partition by s.blob, s.test_features, s.kind, s.start_byte, s.end_byte
                             order by f.start_byte desc, f.end_byte) = 1),
inner_sums as (
  select blob, test_features, start_byte, end_byte, sum(inner_tokens) as inner_tokens
  from (select o.blob, o.test_features, o.start_byte, o.end_byte, i.tokens as inner_tokens
        from fns i join fns o
          on o.blob = i.blob and o.test_features = i.test_features
         and o.start_byte <= i.start_byte and i.end_byte <= o.end_byte
         and (o.start_byte, o.end_byte) <> (i.start_byte, i.end_byte)
        qualify row_number() over (partition by i.blob, i.test_features, i.start_byte, i.end_byte
                                   order by o.start_byte desc, o.end_byte) = 1)
  group by all),
counts as (
  select blob, test_features, start_byte, end_byte,
         count(*) filter (kind in ('branch', 'condition')) as decisions,
         count(*) filter (kind = 'throw') as throws,
         count(*) filter (kind = 'goto') as gotos,
         count(*) filter (kind = 'invariant') as invariants,
         count(*) filter (kind = 'unhandled') as unhandled,
         count(*) filter (kind = 'unfinished') as unfinished
  from innermost group by all),
nesting as (
  select f.blob, f.test_features, f.start_byte, f.end_byte,
         coalesce(max(b.depth) - min(b.depth), 0) as nesting
  from fns f left join spans b on b.blob = f.blob and b.kind = 'block'
    and b.start_byte >= f.start_byte and b.end_byte <= f.end_byte
  group by all)
select f.blob, f.test_features, f.lang, f.start_byte, f.end_byte, f.line, f.end_line, f.name,
       concat_ws('::', q.outer_names, f.name) as qualified,
       row_number() over (partition by f.blob, f.test_features, qualified order by f.start_byte) as ordinal,
       f.tokens, f.tokens - coalesce(i.inner_tokens, 0) as own_tokens, f.params, f.hash,
       -- Syntactic visibility: pub/export markers, C/C++ not static, Python no `_`.
       case when f.lang in ('c', 'cpp')
              then (f.blob, f.start_byte, f.end_byte) not in (select blob, start_byte, end_byte from spans where kind = 'static')
            when f.lang = 'python' then not starts_with(f.name, '_')
            else (f.blob, f.start_byte, f.end_byte) in (select blob, start_byte, end_byte from spans where kind = 'pub')
       end as public,
       n.nesting, coalesce(c.decisions, 0) as decisions, coalesce(c.throws, 0) as throws,
       coalesce(c.gotos, 0) as gotos, coalesce(c.invariants, 0) as invariants,
       coalesce(c.unhandled, 0) as unhandled, coalesce(c.unfinished, 0) as unfinished
from fns f join nesting n using (blob, test_features, start_byte, end_byte)
left join qualified q using (blob, test_features, start_byte, end_byte)
left join counts c using (blob, test_features, start_byte, end_byte)
left join inner_sums i using (blob, test_features, start_byte, end_byte);

-- Additive structure facts per blob and declared test features, production code only
-- (code_spans), with the function facts summed.
create or replace view blob_facts as
with functions as (
  select blob, test_features, sum(tokens) as fn_tokens, sum(tokens * nesting) as nested_tokens,
         coalesce(sum(tokens) filter (nesting > deep_nesting()), 0) as deep_tokens,
         coalesce(sum(tokens) filter (not public), 0) as private_tokens,
         coalesce(sum(tokens) filter (tokens > large_tokens()), 0) as large_tokens
  from function_facts group by blob, test_features),
tests as (
  select blob, test_features, sum(tokens) as test_tokens from test_ranges group by all),
counts as (
  select blob, test_features, any_value(lang) as lang,
    coalesce(sum(tokens) filter (kind = 'file'), 0) as tokens,
    count(*) filter (kind in ('branch', 'condition')) as decisions,
    count(*) filter (kind = 'global_state') as global_state,
    count(*) filter (kind = 'throw') as throws,
    count(*) filter (kind = 'goto') as gotos,
    count(*) filter (kind = 'invariant') as invariants,
    count(*) filter (kind = 'unhandled') as unhandled,
    count(*) filter (kind = 'unfinished') as unfinished
  from code_spans group by blob, test_features)
select c.blob, c.test_features, c.lang,
       -- A file that is test code throughout is no production file.
       if(c.tokens > 0 and c.tokens = coalesce(t.test_tokens, 0), 0, 1) as files,
       c.tokens - coalesce(t.test_tokens, 0) as tokens,
       c.* exclude (blob, test_features, lang, tokens),
       coalesce(columns(f.* exclude (blob, test_features)), 0)
from counts c left join functions f using (blob, test_features)
left join tests t using (blob, test_features);

-- Pairs of production files of one language family (files import only their own
-- family) changed together by at least cochange_min() non-bulk commits of a weekly
-- window, both live at its end, at their paths then. A helper, so `cv metrics`
-- materializes it: inlined into the import join below it plans badly.
create or replace view cochange_pairs as
with c as (
  select w.win, x.seq, x.lineage, family(x.path) as family
  from windows w join change_weights x using (seq)),
pairs as (
  select a.win, a.lineage as la, b.lineage as lb, count(*) as commits
  from c a join c b on a.win = b.win and a.seq = b.seq and a.family = b.family and a.lineage < b.lineage
  group by all having count(*) >= cochange_min()),
ends as (select win, la as lineage from pairs union select win, lb from pairs),
paths as (
  select e.win, e.lineage, x.path, x.blob
  from ends e asof join code_changes x on x.lineage = e.lineage and e.win >= x.seq)
select p.win, pa.path as a, pb.path as b, least(pa.path, pb.path) as x, greatest(pa.path, pb.path) as y,
       p.commits
from pairs p
join paths pa on pa.win = p.win and pa.lineage = p.la and pa.blob is not null
join paths pb on pb.win = p.win and pb.lineage = p.lb and pb.blob is not null;

-- Production totals per commit and language, from blob_facts by change points.
create or replace view code_totals as
with v as (
  select v.since, v.until, f.* exclude (blob, test_features)
  from code_versions v join blob_facts f using (blob, test_features)),
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

-- A count of code_totals (global_state, throws, invariants, unhandled, unfinished)
-- per 1,000 production tokens at each commit (value), the count itself, and the
-- same per language and family.
create or replace macro per_kilotoken(which) as table
select seq, 1000 * sum(n) / nullif(sum(tokens), 0) as value, sum(n) as count,
       map(list(lang order by lang), list(1000 * n / nullif(tokens, 0) order by lang)) as by_language,
       1000 * family_ratio(n, tokens, lang, 'c') as c, 1000 * family_ratio(n, tokens, lang, 'rust') as rust,
       1000 * family_ratio(n, tokens, lang, 'python') as python, 1000 * family_ratio(n, tokens, lang, 'ecma') as ecma
from (select seq, lang, tokens, n from code_totals
      unpivot (n for measure in (global_state, throws, invariants, unhandled, unfinished))
      where measure = which)
group by seq;

-- Per-function measures over production code at every commit, as histograms built
-- from change points: for each measure (decisions, nesting, params) and value, the
-- functions with that value and their tokens. Commits times values, not commits
-- times functions.
create or replace view function_histograms as
with f as (
  select v.since, v.until, x.tokens, x.decisions, x.nesting, x.params
  from code_versions v join function_facts x using (blob, test_features)),
m as (
  select since, until, tokens, measure, value from f
  unpivot (value for measure in (decisions, nesting, params))
  where value is not null),
deltas as (
  select measure, value, seq, sum(n) as n, sum(t) as t from (
    select measure, value, since as seq, 1 as n, tokens as t from m
    union all select measure, value, until, -1, -tokens from m)
  group by all),
running as (
  select measure, value, seq, sum(n) over w as n, sum(t) over w as t
  from deltas window w as (partition by measure, value order by seq)),
grid as (
  select c.seq, k.measure, k.value from commits c, (select distinct measure, value from running) k)
select g.seq, g.measure, g.value, r.n, r.t
from grid g asof join running r on g.measure = r.measure and g.value = r.value and g.seq >= r.seq
where r.n > 0;

-- Tail statistics per commit and measure: the 90th percentile (nearest rank) and the
-- maximum over functions, and the share of function tokens in functions above
-- `threshold`.
create or replace macro function_tail(which, threshold) as table
with h as (select * from function_histograms where measure = which),
c as (select *, sum(n) over (partition by seq order by value) as cumulative,
             sum(n) over (partition by seq) as functions, sum(t) over (partition by seq) as tokens
      from h)
select seq, min(value) filter (cumulative >= 0.9 * functions) as p90, max(value) as max,
       coalesce(sum(t) filter (value > threshold), 0) / any_value(tokens) as share,
       any_value(functions) as functions
from c group by seq;

-- Commits with derived labels. is_fix: a defect fix in production behaviour: a fix
-- keyword or a defect word in the subject, production code touched, and no sign of a
-- test, CI, docs or lint fix. Checked on 150 hand-labelled RediSearch commits
-- (validation/README.md): precision 0.86, recall 0.89 (but possibly as low as 0.6),
-- against 0.48 and 0.89 for the keyword alone (is_fix_keyword). is_agent: a co-author
-- trailer names a coding agent; in squash merges it means an agent helped somewhere.
create or replace view commit_info as
with production as (
  select seq, count(*) as files from code_changes group by seq),
c as (
  select c.*, a.name || ' <' || a.email || '>' as author, a.is_bot,
         split_part(c.message, chr(10), 1) as subject, coalesce(p.files, 0) as production_files,
         regexp_matches(split_part(c.message, chr(10), 1), '(?i)\b(fix|fixes|fixed|bug|hotfix|regression|revert)')
           as is_fix_keyword
  from commits c join authors a using (seq) left join production p using (seq))
select seq, id, time, author, message, subject, production_files,
  (is_fix_keyword
     or regexp_matches(subject, '(?i)(crash|leak|race\b|deadlock|hang|segfault|overflow|corrupt|wrong|incorrect|asan|use.after.free|double.free|null.pointer|panic)'))
    and production_files > 0
    and not regexp_matches(subject, '(?i)(test|flak|\bci\b|lint|typo|\bdocs?\b|readme|cmake|\bbuild\b|review|indent|format|chore|contributing|\blink)')
    as is_fix,
  is_fix_keyword,
  regexp_matches(message, '(?i)co-authored-by:.*(claude|copilot|cursor|codex|devin|aider)') as is_agent,
  is_bot
from c;
