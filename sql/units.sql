-- Per-unit tables: the values of production files and functions at every version, so
-- a repository-level value can be traced to the files and functions behind it. Each
-- row holds from commit `since` until before `until`.

-- Production files: blob_facts per version, with the file's lineage (renames keep it).
create or replace view units.files as
select v.path, v.lineage, v.blob, v.since, v.until, f.* exclude (blob, test_features)
from code_versions v join blob_facts f using (blob, test_features);

comment on view units.files is 'Production files at each version [since, until): path, lineage (a stable id across renames), blob, and the facts the metrics sum (tokens, decisions, throws, gotos, global_state, fn_tokens, nested_tokens, deep_tokens, private_tokens, large_tokens).';

-- Production functions per file version. `function` identifies a function across
-- versions: its file''s lineage, its qualified name, and its ordinal when the file has
-- several of that name (`#2`, ...). A renamed function, or one moved to another
-- file, gets a new identity.
create or replace view units.functions as
select v.path, v.lineage,
       v.lineage || ' ' || f.qualified || if(f.ordinal > 1, '#' || f.ordinal, '') as function,
       v.blob, v.since, v.until, f.* exclude (blob, test_features)
from code_versions v join function_facts f using (blob, test_features);

comment on view units.functions is 'Production functions at each version [since, until) of their file: function (lineage, qualified name and ordinal: an identity across versions), name, qualified, line, end_line, tokens, params, public, nesting (deepest block below the body), decisions, throws and gotos (in this function, not in functions inside it).';

-- Hotspots: live production files ranked by how often they changed in the trailing
-- hotspot_days() (non-bulk commits, renames followed) times how many decision points
-- they hold. Either alone ranks large or busy files; together they rank where change
-- meets complexity (Tornhill).
create or replace view units.hotspots as
with tip as (select max(seq) as seq, max(time) as time from commit_times),
recent as (
  select x.lineage, count(distinct x.seq) as changes
  from code_changes x join commit_times c using (seq), tip
  where c.time > tip.time - to_days(hotspot_days())
    and x.seq in (select seq from change_weights)
  group by x.lineage),
live as (select f.* from units.files f, tip where f.since <= tip.seq and tip.seq < f.until)
select l.path, l.lineage, coalesce(r.changes, 0) as changes, l.decisions, l.tokens,
       coalesce(r.changes, 0) * l.decisions as score,
       rank() over (order by coalesce(r.changes, 0) * l.decisions desc) as rank
from live l left join recent r using (lineage);

comment on view units.hotspots is 'Live production files ranked by changes in the trailing hotspot_days() (commits of at most max_commit_files() files; renames keep a file''s count) times their decision points: score = changes * decisions. Unvalidated.';

-- The defect check of `cv validate`: history cut into consecutive windows of
-- defect_window_days(); at each cut, every live production file with its static
-- values, its changes in the window before, and the fix commits (commit_info.is_fix)
-- that touch it in the window after. A cut needs a full window on both sides.
create or replace view units.defect_windows as
with bounds as (select min(time) as first, max(time) as last from commit_times),
cuts as (
  select k, b.first + to_days(k * defect_window_days()) as cut
  from bounds b, range(1, 1000) r(k)
  where b.first + to_days((k + 1) * defect_window_days()) <= b.last),
snap as (
  select c.k, c.cut, max(t.seq) as seq from cuts c join commit_times t on t.time <= c.cut group by all),
live as (
  select s.k, s.cut, f.* from snap s join units.files f on f.since <= s.seq and s.seq < f.until),
fns as (
  select s.k, u.lineage, max(u.decisions) as max_decisions, max(u.nesting) as max_nesting,
         max(u.params) as max_params
  from snap s join units.functions u on u.since <= s.seq and s.seq < u.until group by all),
touches as (
  select x.lineage, t.time, c.is_fix
  from code_changes x join commit_times t using (seq) join commit_info c using (seq)
  where x.blob is not null),
counted as (
  select l.k, l.lineage,
         count(*) filter (t.time > l.cut - to_days(defect_window_days()) and t.time <= l.cut) as changes,
         count(*) filter (t.is_fix and t.time > l.cut and t.time <= l.cut + to_days(defect_window_days())) as fixes
  from live l left join touches t using (lineage) group by all)
select l.k, l.cut, l.path, l.lineage, l.tokens, c.changes, c.fixes, l.decisions,
       coalesce(f.max_decisions, 0) as max_decisions, coalesce(f.max_nesting, 0) as max_nesting,
       coalesce(f.max_params, 0) as max_params, l.throws, l.unhandled, l.invariants, l.global_state,
       c.changes * l.decisions as hotspot, 0 as random
from live l join counted c using (k, lineage) left join fns f using (k, lineage);

comment on view units.defect_windows is 'Per cut of history (k, every defect_window_days()) and live production file: static values (tokens, decisions, max_decisions, max_nesting, max_params, throws, unhandled, invariants, global_state), changes in the window before the cut, hotspot (changes * decisions), random (0: ranks files in a fixed pseudo-random order), and fixes: fix commits (commit_info.is_fix) touching the file in the window after.';

-- Orphaning per file at every weekly window point (`windows`): the share of the
-- file's past changes (non-bulk commits, renames followed) made by authors with no
-- commit in the orphan_months() before that point.
create or replace view units.orphaning as
with points as (
  select distinct w.win as seq, c.time from windows w join commit_times c on c.seq = w.win),
active as (
  select distinct p.seq, a.identity
  from points p join commit_times t on t.seq <= p.seq and t.time > p.time - to_months(orphan_months())
  join authors a on a.seq = t.seq),
history as (
  select p.seq, x.lineage, a.identity
  from points p join code_changes x on x.seq <= p.seq join authors a on a.seq = x.seq
  where x.blob is not null and x.seq in (select seq from change_weights)),
per_file as (
  select h.seq, h.lineage, count(*) as changes, avg((ac.identity is null)::int) as orphaned
  from history h left join active ac using (seq, identity) group by all)
select p.seq, f.path, f.lineage, f.tokens, coalesce(o.changes, 0) as changes, coalesce(o.orphaned, 0) as orphaned
from points p join units.files f on f.since <= p.seq and p.seq < f.until
left join per_file o on o.seq = p.seq and o.lineage = f.lineage;

comment on view units.orphaning is 'Per weekly window point (seq) and live production file: changes (non-bulk, renames followed, up to then) and orphaned, the share of them by authors with no commit in the orphan_months() before. Unvalidated.';

-- Accretion per touch: each change to a production file that has a previous version
-- (pure moves left out), and whether it raised the decision points or the nesting of
-- a function that existed before it (same `function` identity; new functions do
-- not count).
create or replace view units.touches as
with t as (
  select seq, lineage, path from (
    select *, lag(blob) over (partition by lineage order by seq) as previous from code_changes)
  where blob is not null and previous is not null and blob <> previous),
raised as (
  select distinct c.since as seq, c.lineage
  from units.functions c join units.functions p on p.function = c.function and p.until = c.since
  where c.decisions > p.decisions or c.nesting > p.nesting)
select t.seq, t.lineage, t.path, r.lineage is not null as raised
from t left join raised r using (seq, lineage);

comment on view units.touches is 'Each change (seq) to a production file with a previous version, pure moves left out, and raised: whether it added decision points or nesting to a function that existed before (same function identity). Unvalidated.';

-- Coupled pairs per weekly window (cochange_pairs), and whether either file imports
-- the other there.
create or replace view units.coupled_pairs as
with edges as (
  select least(source, target) as x, greatest(source, target) as y, since, until from import_edges),
e as (
  select distinct p.win, p.a, p.b
  from cochange_pairs p join edges e using (x, y)
  where e.since <= p.win and p.win < e.until)
select p.win as seq, p.a, p.b, p.commits, e.a is not null as imported
from cochange_pairs p left join e using (win, a, b);

comment on view units.coupled_pairs is 'Per weekly window (seq): pairs of live production files of one language family changed together by at least cochange_min() non-bulk commits of the window, and imported: whether either imports the other then. Unvalidated.';
