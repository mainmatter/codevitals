-- The metrics: views with `seq`, `value` and components. Each comment's first line is
-- `CATEGORY: Title`; What, Read and Trend follow (help, dashboard).

create or replace view metrics.size as
select seq, sum(tokens) as value, sum(files) as files,
       sum(tokens) filter (lang_family(lang) = 'c') as c, sum(tokens) filter (lang_family(lang) = 'rust') as rust,
       sum(tokens) filter (lang_family(lang) = 'python') as python, sum(tokens) filter (lang_family(lang) = 'ecma') as ecma
from code_totals group by seq;

comment on view metrics.size is 'SIZE: Production code size

What: production code at each commit, in tokens (value) and files, and in tokens per language (c, rust, python, ecma); tests, vendored and non-code files excluded.

Read: context, not a score. A step marks a bulk commit, which often moves other charts too.

Trend: size is the strongest single predictor of defects: judge the other trends against it.';

create or replace view metrics.co_change_distance as
with c as (select * from code_changes
           qualify count(*) over (partition by seq) between 2 and max_commit_files()),
per_commit as (
  select a.seq, avg(tree_distance(a.path, b.path)) as distance
  from c a join c b on a.seq = b.seq and a.path < b.path group by a.seq),
-- Chance: the mean distance over all pairs of the window's files. Pairs share their
-- common ancestors, so it needs only depths and per-directory file counts.
files as (select distinct w.win, c.path from windows w join c using (seq)),
pairs_under as (
  select win, sum(n * (n - 1) / 2) as pairs from (
    select win, dir, count(*) as n from (select win, unnest(ancestors(path)) as dir from files)
    group by all)
  group by win),
expected as (
  select f.win,
         ((count(*) - 1) * sum(depth(f.path)) - 2 * coalesce(any_value(u.pairs), 0))
           / (count(*) * (count(*) - 1) / 2) as expected
  from files f left join pairs_under u using (win)
  group by f.win having count(*) > 1),
observed as (
  select w.win, avg(p.distance) as observed, low(p.distance) as low, high(p.distance) as high,
         count(*) as commits, any_value(w.span_weeks) as span_weeks
  from windows w join per_commit p using (seq) group by w.win)
select o.win as seq, o.observed / nullif(e.expected, 0) as value,
       o.low / nullif(e.expected, 0) as low, o.high / nullif(e.expected, 0) as high,
       o.observed, e.expected, o.commits, o.span_weeks
from observed o join expected e using (win);

comment on view metrics.co_change_distance is 'CHANGES: Distance between files changed together (vs chance)

What: how far apart in the directory tree the files changed by one commit are, relative to chance, over the last 100 code commits each week. Lower is better.

Read: 1 means changes pair up files as if at random; below 1, a commit''s files sit close together. observed is the mean distance in directory steps (0: same directory, 2: sibling directories), expected its chance level. The shaded band is a 95% confidence interval for this average: movement inside it is noise.

Trend: rising means work spreads across the tree: what changes together does not live together. Change coupling correlates with defects.';

create or replace view metrics.rework as
with v as (
  select x.seq, x.blob,
         c.time - lag(c.time) over (partition by x.path order by x.seq) as age
  from code_changes x join commit_times c using (seq)),
per_commit as (
  select seq, avg((age <= to_days(rework_days()))::int) as reworked, count(*) as modifications
  from v where age is not null and blob is not null group by seq)
select w.win as seq, avg(reworked) as value,
       low(reworked) as low, high(reworked) as high,
       count(*) as commits, sum(modifications) as modifications, any_value(w.span_weeks) as span_weeks
from windows w join per_commit p using (seq) group by w.win;

comment on view metrics.rework is 'CHANGES: Rework within 7 days

What: the share of edits to files already changed in the previous 7 days, over the last 100 code commits each week. Lower is better.

Read: 0.2 means a fifth of a typical commit''s edits rework last week''s code. Expect high values during feature work.

Trend: rising means code does not settle. A faster commit pace raises it too (see span_weeks). Unvalidated.';

-- Each non-bulk code commit weighs 1, split over its files.
create or replace view change_weights as
select seq, path, 1 / count(*) over (partition by seq) as w, count(*) over (partition by seq) as k
from code_changes
qualify count(*) over (partition by seq) <= max_commit_files();

create or replace view metrics.change_entropy as
with f as (select w.win, c.path, sum(c.w) as w from windows w join change_weights c using (seq) group by all),
p as (select win, family(path) as family, w / sum(w) over (partition by win) as p,
            w / sum(w) over (partition by win, family(path)) as p_family from f),
commits as (select w.win, avg(k) as files_per_commit, count(*) as commits, any_value(w.span_weeks) as span_weeks
            from windows w join (select distinct seq, k from change_weights) using (seq) group by w.win)
select win as seq, 1 / sum(p * p) as value, any_value(c.files_per_commit) as files_per_commit,
       power(2, -sum(p * log2(p))) as effective_files_h, any_value(c.commits) as commits,
       any_value(c.span_weeks) as span_weeks,
       1 / sum(p_family * p_family) filter (family = 'c') as c, 1 / sum(p_family * p_family) filter (family = 'rust') as rust,
       1 / sum(p_family * p_family) filter (family = 'python') as python, 1 / sum(p_family * p_family) filter (family = 'ecma') as ecma
from p join commits c using (win) group by win;

comment on view metrics.change_entropy is 'ENTROPY: Work spread: effective files

What: over how many files the work of the last 100 code commits is effectively spread, each week; each commit counts once. Per language (c, rust, python, ecma): over how many of that language''s files its share of the work is spread.

Read: 12 means as if spread evenly over 12 files. Files per commit is shown because bigger commits spread work by themselves.

Trend: rising means more places change at once. It predicted faults well in open-source projects (Hassan).';

create or replace view metrics.encapsulation as
select seq, sum(private_tokens) / sum(fn_tokens) as value,
       map(list(lang order by lang), list(private_tokens / fn_tokens order by lang)) as by_language,
       family_ratio(private_tokens, fn_tokens, lang, 'c') as c, family_ratio(private_tokens, fn_tokens, lang, 'rust') as rust,
       family_ratio(private_tokens, fn_tokens, lang, 'python') as python, family_ratio(private_tokens, fn_tokens, lang, 'ecma') as ecma
from code_totals where fn_tokens > 0 group by seq;

comment on view metrics.encapsulation is 'STRUCTURE: Encapsulated code share

What: the share of function code in non-public functions (no pub, static, not exported, leading underscore), overall and per language (c, rust, python, ecma). Higher is better.

Read: 0.6 means 60% of function code is internal. Visibility is written differently per language: compare each language over time, not languages with each other.

Trend: falling means the public surface grows faster than the code behind it.';

-- Change points per structure: each change adds its new contribution, removes its old.
create or replace view metrics.duplication as
with f as (
  select v.since, v.until, s.lang, s.hash, s.tokens
  from versions v join path_roles r on r.path = v.path and r.role = 'code'
  join code_spans s on s.blob = v.blob and s.test_features = v.test_features
    and s.kind = 'fn' and s.tokens >= clone_tokens()),
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
       map(list(lang order by lang), list(duplicated / tokens order by lang)) as by_language,
       family_ratio(duplicated, tokens, lang, 'c') as c, family_ratio(duplicated, tokens, lang, 'rust') as rust,
       family_ratio(duplicated, tokens, lang, 'python') as python, family_ratio(duplicated, tokens, lang, 'ecma') as ecma
from totals group by seq;

comment on view metrics.duplication is 'STRUCTURE: Duplicated code share

What: the share of function code (functions of 50+ tokens) whose structure occurs more than once, ignoring names and literals, overall and per language (c, rust, python, ecma). Lower is better.

Read: 0.05 means 5% of it has a structural twin.

Trend: rising means copy-paste instead of reuse. Whole-function clones only. Unvalidated.';

create or replace view metrics.concentration as
select seq, sum(large_tokens) / sum(fn_tokens) as value,
       map(list(lang order by lang), list(large_tokens / fn_tokens order by lang)) as by_language,
       family_ratio(large_tokens, fn_tokens, lang, 'c') as c, family_ratio(large_tokens, fn_tokens, lang, 'rust') as rust,
       family_ratio(large_tokens, fn_tokens, lang, 'python') as python, family_ratio(large_tokens, fn_tokens, lang, 'ecma') as ecma
from code_totals where fn_tokens > 0 group by seq;

comment on view metrics.concentration is 'STRUCTURE: Code in large functions

What: the share of function code in functions over 200 tokens, overall and per language (c, rust, python, ecma). Lower is better.

Read: 0.5 means half of it sits in large functions. Compare each language over time.

Trend: rising means logic piles up in a few large functions. Unvalidated.';

create or replace view metrics.ownership as
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

comment on view metrics.ownership is 'CHANGES: File ownership by top author

What: for files changed at least twice in the last 100 code commits, the share of changes made by their most active author, each week. Higher is better.

Read: 0.8 means a typical file''s main author made 80% of its changes. The shaded band is a 95% confidence interval for this average: movement inside it is noise.

Trend: falling means more people change code nobody owns, which went with more defects (Bird et al.). Squash merges credit the merger.';

create or replace view metrics.decisions as
select seq, 100 * sum(decisions) / nullif(sum(tokens), 0) as value,
       map(list(lang order by lang), list(100 * decisions / nullif(tokens, 0) order by lang)) as by_language,
       100 * family_ratio(decisions, tokens, lang, 'c') as c, 100 * family_ratio(decisions, tokens, lang, 'rust') as rust,
       100 * family_ratio(decisions, tokens, lang, 'python') as python, 100 * family_ratio(decisions, tokens, lang, 'ecma') as ecma
from code_totals group by seq;

comment on view metrics.decisions is 'COMPLEXITY: Decision points per 100 tokens

What: decision points (if, loops, match arms, catch, ternaries, &&, ||) per 100 production tokens, overall and per language (c, rust, python, ecma).

Read: 5 means one decision every 20 tokens. Languages differ: compare each language over time, not languages with each other.

Trend: rising means more paths to understand and test per line. Unvalidated.';

create or replace view metrics.nesting as
select seq, sum(nested_tokens) / nullif(sum(fn_tokens), 0) as value,
       sum(deep_tokens) / nullif(sum(fn_tokens), 0) as deep_share,
       map(list(lang order by lang), list(nested_tokens / nullif(fn_tokens, 0) order by lang)) as by_language,
       family_ratio(nested_tokens, fn_tokens, lang, 'c') as c, family_ratio(nested_tokens, fn_tokens, lang, 'rust') as rust,
       family_ratio(nested_tokens, fn_tokens, lang, 'python') as python, family_ratio(nested_tokens, fn_tokens, lang, 'ecma') as ecma
from code_totals group by seq;

comment on view metrics.nesting is 'COMPLEXITY: Block nesting below function body

What: how many blocks deep function code sits below the function body, weighted by tokens, overall and per language (c, rust, python, ecma); deep_share is the code in functions nested more than 3 deep.

Read: 1.5 means typical code is one to two blocks deep. Else-if chains stay flat.

Trend: rising means more conditions to keep in mind while reading. Unvalidated.';

create or replace view metrics.global_state as
select seq, 1000 * sum(global_state) / nullif(sum(tokens), 0) as value,
       map(list(lang order by lang), list(1000 * global_state / nullif(tokens, 0) order by lang)) as by_language,
       1000 * family_ratio(global_state, tokens, lang, 'c') as c, 1000 * family_ratio(global_state, tokens, lang, 'rust') as rust,
       1000 * family_ratio(global_state, tokens, lang, 'python') as python, 1000 * family_ratio(global_state, tokens, lang, 'ecma') as ecma
from code_totals group by seq;

comment on view metrics.global_state is 'NONLOCAL: Global mutable state per 1,000 tokens

What: declarations of global mutable state per 1,000 production tokens, overall and per language (c, rust, python, ecma): Rust static mut, C/C++ file-scope variables, Python global, JS/TS module-level let and var.

Read: the forms differ: compare each language over time, not languages with each other.

Trend: rising means behaviour depends on more state outside the code in view.';

create or replace view metrics.exceptions as
select seq, 1000 * sum(throws) / nullif(sum(tokens), 0) as value,
       map(list(lang order by lang), list(1000 * throws / nullif(tokens, 0) order by lang)) as by_language,
       1000 * family_ratio(throws, tokens, lang, 'c') as c, 1000 * family_ratio(throws, tokens, lang, 'rust') as rust,
       1000 * family_ratio(throws, tokens, lang, 'python') as python, 1000 * family_ratio(throws, tokens, lang, 'ecma') as ecma
from code_totals group by seq;

comment on view metrics.exceptions is 'NONLOCAL: Throws and panics per 1,000 tokens

What: non-local exits per 1,000 production tokens, overall and per language (c, rust, python, ecma): throw, raise, Rust panics, unwrap and expect, C longjmp.

Read: in Rust mostly unwrap and expect; elsewhere throws a distant caller must handle. Compare each language over time.

Trend: rising means control leaves functions in more places a caller cannot see. Unvalidated.';

create or replace view metrics.goto as
select seq, 1000 * sum(gotos) / nullif(sum(tokens), 0) as value,
       map(list(lang order by lang), list(1000 * gotos / nullif(tokens, 0) order by lang)) as by_language
from code_totals where lang in ('c', 'cpp') group by seq;

comment on view metrics.goto is 'NONLOCAL: goto per 1,000 C/C++ tokens

What: goto statements per 1,000 C/C++ tokens.

Read: mostly the goto-cleanup idiom, which is benign: watch spikes, not the level.

Trend: rising outside error cleanup means jumps that defeat top-to-bottom reading. Unvalidated.';

-- A file's reach: the files of its family it depends on through its imports and
-- theirs (`cones`). Families never import each other, so each is a column.
create or replace view metrics.dependency_reach as
with families as (
  select seq, family, any_value(files) as files, sum(cone * n) as total, max(scc) as largest_scc
  from reach group by all having any_value(files) > 1),
medians as (
  select seq, min(cone) filter (cumulative >= files / 2) as median_reach from (
    select seq, cone, sum(n) over (partition by seq order by cone) as cumulative,
           sum(n) over (partition by seq) as files
    from (select seq, cone, sum(n) as n from reach group by all))
  group by seq),
costs as (select *, total / (files * (files - 1)) as cost from families)
select f.seq, sum(f.total / (f.files - 1)) / sum(f.files) as value,
       sum(f.total) / sum(f.files) as mean_reach, any_value(m.median_reach) as median_reach,
       max(f.largest_scc) as largest_cycle, sum(f.files) as files,
       any_value(f.cost) filter (f.family = 'c') as c, any_value(f.cost) filter (f.family = 'rust') as rust,
       any_value(f.cost) filter (f.family = 'python') as python, any_value(f.cost) filter (f.family = 'ecma') as ecma
from costs f join medians m using (seq) group by f.seq;

comment on view metrics.dependency_reach is 'DEPENDENCIES: Share of its language''s code a file depends on

What: the share of the other files of its language that a typical production file depends on, through its imports and theirs (its reach), each week, per language (c, rust, python, ecma). Lower is better.

Read: 0.1 means a file depends on a tenth of its language''s code. mean_reach and median_reach count files; largest_cycle is the largest group of files importing each other.

Trend: rising means a change anywhere can affect, and requires understanding, more of the code (MacCormack et al.).';

create or replace view metrics.import_distance as
with e as (
  select distinct p.seq, family(e.source) as family, e.source, e.target
  from reach_points p join import_edges e on e.since <= p.seq and p.seq < e.until),
-- Chance: imports rewired at random within the family, keeping each file's counts.
shared as (
  select seq, family, sum(o * i) as shared from (
    select seq, family, dir, sum(o) as o, sum(i) as i from (
      select seq, family, unnest(ancestors(source)) as dir, 1 as o, 0 as i from e
      union all select seq, family, unnest(ancestors(target)), 0, 1 from e)
    group by all)
  group by all),
lifts as (
  select seq, family, count(*) as edges,
         avg(tree_distance(source, target))
           / nullif((sum(depth(source)) + sum(depth(target))) / count(*)
                    - 2 * coalesce(any_value(s.shared), 0) / (count(*) * count(*)), 0) as lift
  from e left join shared s using (seq, family) group by seq, family)
select seq, sum(edges * lift) / sum(edges) filter (lift is not null) as value, sum(edges) as edges,
       any_value(lift) filter (family = 'c') as c, any_value(lift) filter (family = 'rust') as rust,
       any_value(lift) filter (family = 'python') as python, any_value(lift) filter (family = 'ecma') as ecma
from lifts group by seq;

comment on view metrics.import_distance is 'DEPENDENCIES: Distance imports reach (vs chance)

What: how far through the directory tree imports reach, relative to chance, each week, per language (c, rust, python, ecma). Lower is better.

Read: 1 means imports reach as far as if rewired at random; below 1, files mostly import their neighbours. edges is the number of importing file pairs.

Trend: rising means code depends on code ever further away: the tree stops matching the structure. Moving files steps it.';

create or replace view metrics.import_coverage as
with per_family as (
  select p.seq, s.family,
         count(*) filter (s.class = 'resolved') as resolved, count(*) filter (s.fallback) as fallback,
         count(*) filter (s.class <> 'external') as internal, count(*) as imports
  from reach_points p join import_status s on s.since <= p.seq and p.seq < s.until
  group by all)
select seq, sum(resolved) / nullif(sum(internal), 0) as value,
       sum(fallback) / nullif(sum(resolved), 0) as fallback_share,
       1 - sum(internal) / sum(imports) as external_share, sum(imports) as imports,
       any_value(resolved / nullif(internal, 0)) filter (family = 'c') as c,
       any_value(resolved / nullif(internal, 0)) filter (family = 'rust') as rust,
       any_value(resolved / nullif(internal, 0)) filter (family = 'python') as python,
       any_value(resolved / nullif(internal, 0)) filter (family = 'ecma') as ecma
from per_family group by seq;

comment on view metrics.import_coverage is 'DEPENDENCIES: How complete the dependency metrics are, per language

What: the share of each language''s imports of this repository''s own code that codevitals could follow to a file, each week (c, rust, python, ecma).

Read: 1 means the dependency metrics see every import; 0.8 means they miss a fifth and read too low. fallback_share counts imports followed only to a crate root or __init__.py, which overstates reach.

Trend: a drop means the dependency metrics became less complete, not that the code changed.';
