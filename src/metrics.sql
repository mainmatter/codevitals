-- ============ helpers ============
create or replace macro module(path, depth) as
  array_to_string(list_slice(string_split(path, '/'), 1,
                  least(depth, len(string_split(path, '/')) - 1)), '/');

create or replace macro live() as table
  select c.seq, f.path, f.blob from commits c
  join files f on c.seq >= f.since and c.seq < f.until;

-- The live files at every 50th commit and at the latest one, for metrics too
-- expensive to evaluate at every commit (calls outnumber all other spans).
create or replace macro live_sampled() as table
  select * from live() where seq % 50 = 0 or seq = (select max(seq) from commits);

create or replace view commit_info as
select seq, id, time, author, message,
  split_part(message, chr(10), 1) as subject,
  regexp_matches(split_part(message, chr(10), 1),
                 '(?i)\b(fix|fixes|fixed|bug|hotfix|regression|revert)') as is_fix,
  regexp_matches(message, '(?i)co-authored-by:.*(claude|copilot|cursor|codex|devin|aider)')
    or regexp_matches(author, '(?i)\[bot\]') as is_agent
from commits;

create or replace view fns as
select f.blob, f.lang, f.name, f.line, f.end_line, f.tokens, f.comments, f.params, f.hash,
  case when f.lang in ('c', 'cpp')
         then (f.blob, f.line, f.end_line) not in (select blob, line, end_line from spans where kind = 'static')
       when f.lang = 'python' then not starts_with(f.name, '_')
       else (f.blob, f.line, f.end_line) in (select blob, line, end_line from spans where kind = 'pub')
  end as public
from spans f where f.kind = 'fn';

-- Calls meet definitions on (seq, name) together. Materializing both sides keeps
-- DuckDB from joining definitions to files on seq alone first: on a 5,650-commit
-- repository that order produced a billion intermediate rows.
create or replace view call_edges as
with defs as materialized (
  select l.seq, regexp_extract(f.name, '[^:.]+$') as name, any_value(l.path) as path
  from live_sampled() l join spans f on f.blob = l.blob and f.kind = 'fn' and f.name is not null
  group by all having count(*) = 1),
calls as materialized (
  select l.seq, l.path, c.name
  from live_sampled() l join spans c on c.blob = l.blob and c.kind = 'call')
select c.seq, c.path as caller, d.path as callee
from calls c join defs d on d.seq = c.seq and d.name = c.name;

-- ============ metrics ============
create or replace view m_locality as
with c as (select seq, count(distinct module(path, 2)) as modules, count(*) as files
           from changes group by seq)
select seq,
  avg((modules = 1)::int) over w as value,
  avg(files) over w as files_per_commit
from c window w as (order by seq rows between 49 preceding and current row);

create or replace view m_scatter as
with per_file as (select seq // 50 as win, path, count(*) as n from changes group by all),
tot as (select win, sum(n) as total, count(*) as files from per_file group by win)
select p.win, min(p.win) * 50 as seq,
  -sum(p.n / t.total * log2(p.n / t.total)) / nullif(log2(any_value(t.files)), 0) as value,
  any_value(t.files) as files, any_value(t.total) as changes
from per_file p join tot t using (win) group by p.win;

create or replace view m_hidden_coupling as
with c as (select seq, path, seq // 100 as win from changes
           where seq in (select seq from changes group by seq having count(*) between 2 and 30)),
pairs as (select a.win, module(a.path, 2) <> module(b.path, 2) as cross_mod
          from c a join c b on a.seq = b.seq and a.path < b.path)
select win, win * 100 as seq, avg(cross_mod::int) as value, count(*) as pairs
from pairs group by win;

create or replace view m_call_coupling as
with e as (select seq, module(caller, 2) as a, module(callee, 2) as b, caller <> callee as cross_file
           from call_edges),
m as (select distinct seq, a, b from e where a <> b),
mutual as (select x.seq, count(*) / 2 as pairs from m x join m y on x.seq = y.seq and x.a = y.b and x.b = y.a group by x.seq),
tot as (select l.seq, count(*) as calls from live_sampled() l join spans c on c.blob = l.blob and c.kind = 'call' group by l.seq)
select e.seq,
  avg((a <> b)::int) filter (cross_file) as value,
  coalesce(any_value(mutual.pairs), 0) as mutual_module_pairs,
  count(*) / any_value(tot.calls) as resolution_rate
from e left join mutual using (seq) join tot using (seq) group by e.seq;

create or replace view m_encapsulation as
select l.seq,
  sum(f.tokens) filter (not f.public) / sum(f.tokens) as value,
  avg(f.public::int) as share_public_fns
from live() l join fns f on f.blob = l.blob group by l.seq;

create or replace view m_duplication as
with f as (select l.seq, s.hash, s.tokens from live() l
           join spans s on s.blob = l.blob and s.kind = 'fn' and s.tokens >= 50),
g as (select seq, hash, sum(tokens) as t, count(*) as n from f group by seq, hash)
select seq, coalesce(sum(t) filter (n > 1), 0) / sum(t) as value,
       count(*) filter (n > 1) as duplicated_shapes
from g group by seq;

create or replace view m_interface_bloat as
select l.seq,
  sum(s.params) / sum(s.tokens) * 100 as value,
  avg((s.params >= 5)::int) as share_wide,
  avg((s.tokens < 20)::int) as share_tiny,
  median(s.tokens) as fn_tokens_median
from live() l join spans s on s.blob = l.blob and s.kind = 'fn' group by l.seq;

create or replace view m_concentration as
select l.seq,
  coalesce(sum(s.tokens) filter (s.tokens > 200), 0) / sum(s.tokens) as value,
  max(s.tokens) as largest_fn
from live() l join spans s on s.blob = l.blob and s.kind = 'fn' group by l.seq;

create or replace view m_sprawl as
with d as (select seq, module(path, 99) as dir, count(*) as files from live() group by all)
select seq, max(files) / sum(files) as value, count(*) as dirs,
       quantile_cont(files, 0.9) as files_per_dir_p90
from d group by seq;

create or replace view m_rework as
with v as (select path, seq, blob, seq - lag(seq) over (partition by path order by seq) as age from changes)
select seq // 50 as win, min(seq // 50) * 50 as seq,
  avg((age <= 5)::int) as value, avg((age <= 20)::int) as within_20, count(*) as modifications
from v where age is not null and blob is not null group by win;

create or replace view m_fix_follow as
with ch as (select c.seq, c.path from changes c join commit_info i using (seq)
            where not i.is_fix and c.blob is not null
              and c.seq <= (select max(seq) from commits) - 20),
fx as (select c.seq, c.path from changes c join commit_info i using (seq) where i.is_fix),
j as (select ch.seq, fx.seq as fix_seq from ch asof left join fx on ch.path = fx.path and ch.seq < fx.seq)
select seq // 50 as win, min(seq // 50) * 50 as seq,
  avg((fix_seq is not null and fix_seq <= seq + 20)::int) as value, count(*) as changes
from j group by win;

create or replace view m_naming as
with f as (select l.seq, l.path, s.name from live() l
           join spans s on s.blob = l.blob and s.kind = 'fn' and s.name is not null),
words as (select seq, path, name,
          unnest(regexp_extract_all(name, '[A-Z]?[a-z0-9]+|[A-Z0-9]+')) as w from f),
wc as (select seq, path, lower(w) as w, count(distinct name) as k from words group by all),
top as (select seq, path, max(k) as top_k from wc group by seq, path),
n as (select seq, path, count(distinct name) as fns from f group by seq, path having count(distinct name) >= 4),
coh as (select n.seq, avg(top.top_k / n.fns) as cohesion from n join top using (seq, path) group by n.seq)
select f.seq, avg((len(f.name) <= 3)::int) as value, avg(len(f.name)) as name_len_mean,
       any_value(coh.cohesion) as lexical_cohesion
from f left join coh using (seq) group by f.seq;

-- ============ explanations (stored in the database) ============
comment on view commit_info is 'Commits with derived labels. is_fix: subject matches fix/bug/regression/revert keywords (noisy). is_agent: co-author trailer or bot author matches a pattern; tune the pattern to your agents.';
comment on view fns is 'One row per function span with a syntactic visibility flag. public: Rust pub, JS/TS export, C/C++ not static, Python no leading underscore. A proxy for API surface, not proof of external use.';
comment on view call_edges is 'Heuristic call graph at every 50th commit and the latest. A call is resolved only when its name matches exactly one function defined in the repository at that commit. Expect a minority of calls to resolve; check m_call_coupling.resolution_rate.';

comment on view m_locality is 'COHERENCE. Share of the last 50 commits whose changes stay inside one module (directory depth 2). Higher is better. Low or falling values mean a typical change cuts across module boundaries: the boundaries no longer match how the code changes. Evidence: the variable encapsulation metrics were validated against; follow-on patch size predicted agent failure. Gaming: splitting one change into several single-module commits; measure per merged PR.';
comment on view m_scatter is 'COHERENCE. Normalized Shannon entropy of changes across files per 50-commit window; 0 = all changes hit one file, 1 = spread evenly over every touched file. Lower is better. High values mean work is diffuse and hard to keep in your head. Evidence: change entropy predicted faults better than prior changes or prior faults in six open-source projects.';
comment on view m_hidden_coupling is 'ENTANGLEMENT. Of all file pairs that changed in the same commit (commits with 2-30 files, per 100-commit window), the share whose files live in different modules. Lower is better. High values reveal dependencies that no import shows: files in separate modules that cannot change independently. Evidence: change coupling correlates with defects.';
comment on view m_call_coupling is 'ENTANGLEMENT. At every 50th commit and the latest: of resolved calls between different files, the share that crosses a module boundary. Lower is better. mutual_module_pairs counts module pairs that call each other in both directions: a cheap proxy for dependency cycles; should be zero. Heuristic: only calls with a repository-unique name resolve (see resolution_rate). Unvalidated.';
comment on view m_encapsulation is 'BOUNDARIES. Share of function tokens that sit in non-public functions. Higher is better: more of the code is hidden behind a smaller public surface. Falling values mean the API surface is growing faster than the implementation. A syntactic approximation of the internal-code share that correlated with local change. Unvalidated in this form.';
comment on view m_duplication is 'COMPLICATEDNESS. Share of function tokens (functions of 50+ tokens) whose structure occurs more than once at that commit, ignoring names and literal values. Lower is better. Rising values mean copy-paste instead of reuse. Whole-function clones only. Gaming: trivial structural edits to a copy.';
comment on view m_interface_bloat is 'COMPLICATEDNESS. Parameters per 100 function tokens. Lower is better. Rising values, together with rising share_tiny (functions under 20 tokens) and a falling fn_tokens_median, mean complexity is moving out of function bodies into parameter lists: splitting without simplifying. share_wide is the share of functions with 5+ parameters. Evidence: field observation only.';
comment on view m_concentration is 'COMPLEXITY. Share of all function tokens that sits in functions larger than 200 tokens. Lower is better. High values mean logic piles up in a few large functions. Read together with m_interface_bloat: driving this down by mechanical splitting shows up there. Token counts ignore whitespace, comments and identifier length.';
comment on view m_sprawl is 'ORGANIZATION. Share of all files that sits in the single largest directory. Lower is better. High values mean a flat pile of files instead of modules. files_per_dir_p90 shows how full directories get. Gaming: creating directories that do not reflect dependencies; cross-check with m_locality and m_hidden_coupling.';
comment on view m_rework is 'STABILITY. Share of file modifications that hit a file changed within the previous 5 commits (within_20: previous 20). Lower is better once a feature has landed. High values mean code does not settle: it is rewritten soon after it is written. Unvalidated; expect high values during active feature work.';
comment on view m_fix_follow is 'OUTCOME. Share of non-fix file changes that are followed by a fix commit touching the same file within 20 commits. Lower is better. This is the closest thing to a defect signal in the data, and the label to validate every other metric against. Noisy: depends on commit-message keywords.';
comment on view m_naming is 'NAMING. Share of function names of 3 characters or fewer. Lower is better. A rising value or a falling name_len_mean flags identifier shortening. lexical_cohesion is the average share of a file''s functions that contain the file''s most common name word (files with 4+ functions); low values suggest grab-bag files. Weakest evidence of all metrics.';
