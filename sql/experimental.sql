-- Experimental metrics: candidates with no supporting evidence yet, kept off the
-- dashboard and out of metrics.json (`cv experimental` exports them). The defect check
-- and the labels of `cv validate` decide which graduate to `metrics`. Each comment
-- has the metrics' format.

-- Production file versions live at each point of reach_points (each week's last
-- commit, the window ends, the tip): the snapshots the metrics below read.
create or replace view experimental_live as
select p.seq, v.path, v.blob, v.test_features
from reach_points p join code_versions v on v.since <= p.seq and p.seq < v.until;

-- Duplicated runs of production tokens. A fingerprint (extract.rs) that occurs twice
-- is a repeated run of 50 tokens, names and literals ignored; every repeated run of
-- 70 or more has one. The bytes of duplicated fingerprints, merged per file and
-- converted to tokens at the file's tokens per byte, approximate the duplicated tokens.
create or replace view experimental.clones as
with live as (select * from experimental_live),
-- Fingerprints outside test code (a left anti-join on the test ranges).
fps as (
  select l.seq, l.path, f.hash, f.start_byte, f.end_byte
  from live l join fingerprints f on f.blob = l.blob
  left join test_ranges t on t.blob = f.blob and t.test_features = l.test_features
    and t.start_byte <= f.start_byte and f.end_byte <= t.end_byte
  where t.blob is null),
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

Read: 0.08 means 8% of the code repeats elsewhere. Approximate: runs are found by winnowed fingerprints (sure from 70 tokens) and measured in bytes.

Trend: rising means copy-paste. Cloning was among the strongest predictors of expert maintainability ratings in Schnappinger et al. (ICPC 2019, three Java systems); on their later public dataset class and method size predicted best. Unvalidated.';


-- Production functions at each point, with the extras extract.rs adds per function.
create or replace view experimental_functions as
with extras as (
  select blob, test_features, start_byte, end_byte,
         bool_or(kind = 'forward') as forward, bool_or(kind = 'doc') as documented,
         max(tokens) filter (kind = 'doc') as doc_subtokens,
         max(comments) filter (kind = 'doc') as doc_shared,
         max(tokens) filter (kind = 'vocabulary') as vocabulary
  from code_spans where kind in ('forward', 'doc', 'vocabulary') group by all)
select l.seq, l.path, f.lang, f.name, f.qualified, f.tokens, f.public, f.start_byte, f.end_byte, f.blob,
       coalesce(e.forward, false) as forward, coalesce(e.documented, false) as documented,
       e.doc_subtokens, e.doc_shared, e.vocabulary
from experimental_live l join function_facts f using (blob, test_features)
left join extras e using (blob, test_features, start_byte, end_byte);

create or replace view experimental.pass_through as
select seq, avg(forward::int) as value, count(*) filter (forward) as count, count(*) as functions
from experimental_functions group by seq;

comment on view experimental.pass_through is 'STRUCTURE: Pass-through wrappers

What: the share of production functions whose body is a single call that passes on all their parameters (value), and their number (count), each week.

Read: 0.05 means one function in twenty only forwards to another. Some forward to enforce a type or an API boundary, which is fine.

Trend: rising means more layers that add a name but no behaviour. Unvalidated.';

-- Non-public functions whose name is the only definition of that name in production
-- code and is called exactly once there. Calls are unresolved names, so functions
-- called through traits or passed as values misfire.
create or replace view experimental_single_use as
with calls as (
  select l.seq, s.name, count(*) as calls
  from experimental_live l join production_spans(['call']) s
    on s.blob = l.blob and s.test_features = l.test_features
  group by all),
defs as (select seq, name, count(*) as defs from experimental_functions group by all)
select f.seq, f.path, f.qualified, f.name, f.tokens, f.public, f.blob, f.start_byte, f.end_byte,
       not f.public and d.defs = 1 and coalesce(c.calls, 0) = 1 as single_use
from experimental_functions f join defs d using (seq, name) left join calls c using (seq, name);

create or replace view experimental.single_use as
select seq, avg(single_use::int) filter (not public) as value, count(*) filter (single_use) as count,
       count(*) filter (not public) as private_functions
from experimental_single_use group by seq;

comment on view experimental.single_use is 'STRUCTURE: Single-use private functions

What: the share of non-public production functions with a unique name that is called exactly once (value), and their number (count), each week. Calls are matched by name only.

Read: 0.3 means three in ten private helpers have one caller. A helper that names a concept is fine; one that only moves code away is not, and this cannot tell them apart.

Trend: rising means more indirection per behaviour. Unvalidated.';

-- Vocabulary (distinct token texts) per function against its size: log vocabulary on
-- log tokens, fitted once over every production function version of the history.
create or replace view experimental.vocabulary as
with f as (
  select seq, ln(vocabulary) as v, ln(tokens) as t from experimental_functions
  where vocabulary > 0 and tokens > 0),
fit as (
  select regr_slope(v, t) as b, regr_intercept(v, t) as a
  from (select distinct f.blob, f.start_byte, ln(f.vocabulary) as v, ln(f.tokens) as t
        from experimental_functions f where vocabulary > 0 and tokens > 0))
select seq, avg(v - (a + b * t)) as value, any_value(b) as slope, count(*) as functions
from f, fit group by seq;

comment on view experimental.vocabulary is 'STRUCTURE: Vocabulary beyond size

What: how much richer in distinct tokens (operators plus operands) production functions are than their size predicts: the mean residual of log vocabulary regressed on log tokens over all function versions of the history (value), each week; slope is the fitted exponent.

Read: 0 means as rich as typical for their size here; 0.1 means about 10% more distinct tokens. Not a ratio to size: size is regressed out.

Trend: rising means functions use more distinct names and operators per size. Unvalidated.';

create or replace view experimental.doc_completeness as
select seq, avg(documented::int) filter (public) as value, count(*) filter (public and documented) as documented,
       count(*) filter (public) as public_functions
from experimental_functions group by seq;

comment on view experimental.doc_completeness is 'DOCUMENTATION: Documented public functions

What: the share of public production functions with a doc comment right above them (/// or /** */) or, in Python, a docstring, each week. Higher is better.

Read: 0.7 means seven in ten public functions say what they do. Visibility is syntactic (see encapsulation).

Trend: falling means the public surface grows faster than its documentation. Unvalidated.';

create or replace view experimental.comment_redundancy as
select seq, avg(doc_shared / doc_subtokens) as value, count(*) as documented
from experimental_functions where documented and doc_subtokens > 0 group by seq;

comment on view experimental.comment_redundancy is 'DOCUMENTATION: Doc comments that repeat the signature

What: of the doc comments of production functions, the mean share of their subtokens (words split at underscores and camelCase) that also occur in the signature below, each week. Lower is better.

Read: 0.5 means half a typical doc comment restates the name and parameters. Short comments of one or two words score high easily.

Trend: rising means more comments that say nothing the code does not. Unvalidated.';

create or replace view experimental.markers as
with m as (
  select l.seq, l.path, count(s.blob) as markers
  from experimental_live l left join code_spans s on s.blob = l.blob and s.test_features = l.test_features
   and s.kind = 'marker'
  group by all),
t as (select l.seq, sum(f.tokens) as tokens from experimental_live l join blob_facts f using (blob, test_features) group by all)
select seq, 1000 * sum(markers) / nullif(any_value(t.tokens), 0) as value, sum(markers) as count,
       count(*) filter (markers > 0) as files_with_markers, count(*) as files
from m join t using (seq) group by seq;

comment on view experimental.markers is 'DOCUMENTATION: TODO, FIXME and HACK comments per 1,000 tokens

What: comments in production code marked TODO, FIXME, HACK or XXX, or saying do not touch, per 1,000 tokens (value), their number (count), and the files that have any, each week. Lower is better.

Read: the markers are what the authors themselves flagged as unfinished or fragile.

Trend: rising means debt is noted faster than paid. Unvalidated.';

-- Fan-in per production file at each window end and start: the files importing it.
create or replace view experimental.demand_without_supply as
with bounds as (select win, min(seq) as first from windows group by win),
fan_in as (
  select b.win, e.target as path,
         count(distinct e.source) filter (e.since <= b.win and b.win < e.until) as now,
         count(distinct e.source) filter (e.since <= b.first and b.first < e.until) as before
  from bounds b join import_edges e on e.since <= b.win and b.first < e.until group by all),
changed as (select distinct w.win, x.path from windows w join code_changes x using (seq)),
live as (
  select b.win, v.path from bounds b join code_versions v on v.since <= b.win and b.win < v.until)
select l.win as seq, avg((f.now > f.before and c.path is null)::int) as value,
       count(*) filter (f.now > f.before and c.path is null) as count, count(*) as files
from live l left join fan_in f on f.win = l.win and f.path = l.path
left join changed c on c.win = l.win and c.path = l.path
group by l.win;

comment on view experimental.demand_without_supply is 'DEPENDENCIES: Files more depended on but left unchanged

What: the share of production files that more files import at the end of the last 100 code commits than at their start, while none of those commits changed them (value), and their number, each week.

Read: such files gain users without being adapted to them: stable foundations, or code people avoid touching.

Trend: rising means growing reliance on code that is not touched. Unvalidated.';

create or replace view experimental.routing_around as
with bounds as (select win, min(seq) as first from windows group by win),
born as (select lineage, min(seq) as seq from code_changes group by lineage),
new_files as (
  select b.win, f.path, f.tokens
  from bounds b join born n on n.seq between b.first and b.win
  join units.files f on f.lineage = n.lineage and f.since <= b.win and b.win < f.until),
stale_targets as (
  select n.win, n.path, bool_or(t.time <= c.time - to_days(stale_days())) as imports_stale
  from new_files n join import_edges e on e.source = n.path and e.since <= n.win and n.win < e.until
  join commit_times c on c.seq = n.win
  join (select path, max(seq) as seq from code_changes group by path) lc on lc.path = e.target
  join commit_times t on t.seq = lc.seq
  group by all)
select n.win as seq, avg((n.tokens < small_file_tokens() and coalesce(s.imports_stale, false))::int) as value,
       count(*) filter (n.tokens < small_file_tokens() and coalesce(s.imports_stale, false)) as count,
       count(*) as new_files
from new_files n left join stale_targets s using (win, path) group by n.win;

comment on view experimental.routing_around is 'DEPENDENCIES: Small new files around long-unchanged code

What: of the production files added in the last 100 code commits, the share that are small (under 300 tokens) and import a file unchanged for over a year (value), and their number, each week.

Read: a small new file next to old code may extend it without touching it: an adapter around code people avoid.

Trend: rising means more code routed around instead of through. Unvalidated.';

create or replace view experimental.risk_per_touch as
with touches as (
  select x.seq, x.lineage, c.time
  from code_changes x join commit_times c using (seq)
  where x.blob is not null and x.seq in (select seq from change_weights)),
fixes as (
  select x.lineage, x.seq, c.time
  from code_changes x join commit_info i using (seq) join commit_times c using (seq)
  where i.is_fix and x.blob is not null),
-- Each touch once, with the next fix to its file (times never fall back).
fixed as (
  select t.seq, t.lineage, coalesce(f.time <= t.time + to_days(fix_days()), false) as fixed
  from touches t asof left join fixes f on f.lineage = t.lineage and f.seq > t.seq),
followed as (select w.win, f.* from windows w join fixed f using (seq)),
horizon as (select max(time) as last from commit_times)
select f.win as seq, avg(f.fixed::int) as value, count(*) filter (f.fixed) as count, count(*) as touches,
       max(c.time) + to_days(fix_days()) > any_value(h.last) as incomplete
from followed f join commit_times c on c.seq = f.seq, horizon h group by f.win;

comment on view experimental.risk_per_touch is 'CHANGES: Changes followed by a fix

What: of the changes to production files in the last 100 code commits, the share followed within 30 days by a fix commit (commit_info.is_fix) to the same file (value), and their number, each week. incomplete marks windows whose 30 days are not over yet. Lower is better.

Read: 0.1 means one change in ten needed a fix soon after. is_fix finds fixes by commit subject; its recall may be as low as 0.6.

Trend: rising means changes break things more often. Unvalidated.';
