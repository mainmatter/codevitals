-- Import resolution and the import graph.

create or replace macro subpath(dir, rest) as case when dir = '' then rest else dir || '/' || rest end;

-- `.` and `..` resolved; a `..` above the start stays, so it names no file.
create or replace macro normalize(p) as
  array_to_string(list_reduce(string_split(p, '/'),
    lambda acc, s: case when s in ('', '.') then acc
                        when s = '..' and len(acc) > 0 and acc[-1] <> '..' then acc[1:-2]
                        else list_append(acc, s) end,
    []::varchar[]), '/');

-- Whether an import's syntax names this repository (a miss is then unresolved).
create or replace macro internal_import(path, name) as
  case family(path) when 'c' then starts_with(name, '"')
                    when 'rust' then split_part(name, '::', 1) in ('crate', 'self', 'super')
                    when 'python' then starts_with(name, '.')
                    when 'ecma' then regexp_matches(name, '^\.\.?/') end;

create or replace view path_life as
with v as (
  select path, since, until,
         since <> coalesce(lag(until) over (partition by path order by since), -1) as starts
  from versions)
select path, min(since) as since, max(until) as until
from (select *, sum(starts::int) over (partition by path order by since) as run from v)
group by path, run;

-- Each blob's imports, with the Rust inline modules around them (`a/b`).
create or replace view blob_imports as
select distinct blob, name, scope from (
  select i.blob, i.name, coalesce(string_agg(m.name, '/' order by m.start_byte), '') as scope
  from spans i
  left join spans m on m.blob = i.blob and m.kind = 'module'
    and m.start_byte < i.start_byte and i.end_byte <= m.end_byte
  where i.kind = 'import'
  group by i.blob, i.name, i.start_byte, i.end_byte);

-- What an ECMAScript import of `base` may name: extensions, index files, .js as .ts.
create or replace macro ecma_files(base) as
  [base, regexp_replace(base, '\.js$', '.ts'), regexp_replace(base, '\.js$', '.tsx')]
  || list_transform(['.ts', '.tsx', '.d.ts', '.js', '.jsx', '.mjs', '.cjs', '.json', '/index.ts',
                     '/index.tsx', '/index.js', '/index.jsx'], lambda e: base || e);

-- tsconfig path aliases: under `dir`, `pattern` (trailing `*`) names `target`.
create or replace view es_aliases as
select distinct directory(c.path) as dir, a.name as pattern, t.name as target,
       coalesce(b.name, '.') as base_url
from changes c
join spans a on a.blob = c.blob and a.kind = 'alias'
join spans t on t.blob = c.blob and t.kind = 'alias_target'
  and t.start_byte >= a.start_byte and t.end_byte <= a.end_byte
left join spans b on b.blob = c.blob and b.kind = 'base_url';

create or replace view import_keys as
select distinct v.path, i.name, i.scope
from code_versions v join blob_imports i using (blob);

-- Imports an alias matches, and the path it makes of them.
create or replace view es_alias_imports as
select k.path, k.name, k.scope,
       normalize(subpath(subpath(a.dir, a.base_url),
                 replace(a.target, '*', substr(k.name, len(rtrim(a.pattern, '*')) + 1)))) as base
from import_keys k join es_aliases a
  on (a.dir = '' or starts_with(k.path, a.dir || '/'))
 and if(ends_with(a.pattern, '*'), starts_with(k.name, rtrim(a.pattern, '*')), k.name = a.pattern)
where family(k.path) = 'ecma' and not regexp_matches(k.name, '^\.\.?/')
  and not regexp_matches(a.target, '(^|/)node_modules/'); -- an alias that pins a dependency

-- Package names from manifests: Cargo [package]/[lib], package.json.
create or replace view packages as
select distinct directory(c.path) as dir, trim(s.name, '"''') as name,
       if(c.path like '%Cargo.toml', 'rust', 'ecma') as family
from changes c join spans s on s.blob = c.blob and s.kind = 'package';

-- Imports of a workspace package (`@scope/pkg/sub`): its directory and subpath.
create or replace view es_packages as
select k.path, k.name, k.scope, p.dir, substr(k.name, len(p.name) + 2) as sub
from import_keys k join packages p on p.family = 'ecma'
  and (k.name = p.name or starts_with(k.name, p.name || '/'))
where family(k.path) = 'ecma';

-- Candidate files per import, ranked (then nearest, then shallowest):
--   C/C++: next to the includer, else any path ending in the include.
--   Python: the longest dotted prefix naming a module, relative for leading dots.
--   ECMAScript: relative paths, workspace packages, tsconfig aliases.
--   Rust: crate::/self::/super::, child modules and workspace crates, to the longest
--   prefix naming a module file, else the module's own file.
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
-- Rust crates: directories with src/lib.rs or src/main.rs, named by directory and
-- Cargo.toml.
crate_dirs as (
  select distinct regexp_extract(path, '^(.*?)/?src/(lib|main)\.rs$', 1) as dir from changes
  where regexp_matches(path, '(^|/)src/(lib|main)\.rs$')),
crates as (
  select dir, replace(regexp_extract(dir, '[^/]*$'), '-', '_') as crate from crate_dirs where dir <> ''
  union
  select d.dir, replace(p.name, '-', '_') from crate_dirs d join packages p on p.dir = d.dir and p.family = 'rust'),
rs_bases as (
  select path, name, scope, src as base, segs[2:] as rest, 3 as tier from rs where segs[1] = 'crate' and src <> ''
  union all
  select path, name, scope, normalize(module || repeat('/..', len(list_filter(segs, lambda s: s = 'super')))),
         list_filter(segs, lambda s: s not in ('self', 'super')), 3
  from rs where segs[1] in ('self', 'super')
  union all
  select path, name, scope, module, segs, 2 from rs where segs[1] not in ('crate', 'self', 'super')
  union all
  select rs.path, rs.name, rs.scope, subpath(k.dir, 'src'), segs[2:], 1 from rs join crates k on k.crate = rs.segs[1]),
exact as (
  select path, name, scope, normalize(subpath(directory(path), inc)) as target, 2 as rank, false as fallback
  from c where starts_with(name, '"')
  union all
  select path, name, scope, unnest(case when j = 0 then [subpath(base, '__init__.py')]
         else [subpath(base, stem) || '.py', subpath(base, stem) || '/__init__.py'] end), j * 10 + 2,
         j = 0 and segs <> ['']
  from py_j where dots > 0

  union all
  select path, name, scope, unnest(ecma_files(base)), 1, false from es
  union all
  select path, name, scope,
         unnest(flatten(list_transform(
           case when sub = '' then [subpath(dir, 'src/index'), subpath(dir, 'index')]
                else [subpath(dir, sub), subpath(dir, 'src/' || sub)] end,
           lambda b: ecma_files(b)))),
         1, false
  from es_packages
  union all
  select path, name, scope, unnest(ecma_files(base)), 1, false from es_alias_imports
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
paths as (select path, regexp_extract(path, '[^/]*$') as file from path_roles),
patterns as (select *, true as anchored from exact union all select *, false from suffix)
select c.path, c.name, c.scope, p.path as target, max(c.rank) as rank, arg_max(c.fallback, c.rank) as fallback
from patterns c join paths p on p.file = regexp_extract(c.target, '[^/]*$')
  and (p.path = c.target or (not c.anchored and ends_with(p.path, '/' || c.target)))
group by all;

-- Each production import over time and what it resolves to. class: resolved,
-- ambiguous, external (by syntax, vendored, or a submodule's include) or unresolved.
-- fallback: only a module's own file matched (a crate root, an __init__.py).
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
       -- An asset (`./logo.svg`) names no code.
       when family(v.path) = 'ecma' and regexp_matches(i.name, '\.[A-Za-z0-9]+(\?.*)?$')
            and family(regexp_replace(i.name, '\?.*$', '')) is null then 'external'
       when internal_import(v.path, i.name)
            or (v.path, i.name) in (select path, name from es_packages union all select path, name from es_alias_imports)
         then 'unresolved' else 'external' end as class,
  r.n = 1 and not r.vendored and r.fallback as fallback
from versions v
join path_roles pr on pr.path = v.path and pr.role = 'code'
join blob_imports i on i.blob = v.blob
join resolution r on r.path = v.path and r.name = i.name and r.scope = i.scope
  and r.since < v.until and v.since < r.until;

create or replace view graph_files as
select l.path, family(l.path) as family, l.since, l.until
from path_life l join path_roles r on r.path = l.path and r.role = 'code';

-- The import graph, one row per import (graph.rs counts a pair once).
create or replace view import_edges as
select i.path as source, i.target, i.since, i.until
from import_status i join path_roles r on r.path = i.target and r.role = 'code'
where i.class = 'resolved' and i.target <> i.path;

-- Where dependency metrics are read: each week's last commit, window ends, the tip.
create or replace view reach_points as
select seq from commit_times
qualify row_number() over (partition by date_trunc('week', time) order by seq desc) = 1
union select win from windows
union select max(seq) from commits;

-- Reach per family at each point as a histogram: n files reach `cone` others,
-- `below` reach fewer, the largest cycle among them has `scc` files.
create or replace view reach as
with deltas as (
  select family, seq, sum(n) as n from (
    select family, since as seq, 1 as n from graph_files
    union all select family, until, -1 from graph_files)
  group by all),
running as (select family, seq, sum(n) over (partition by family order by seq) as files from deltas),
alive as (
  select p.seq, a.family, a.files from reach_points p
  join (select distinct family from graph_files) k on true
  asof join running a on a.family = k.family and p.seq >= a.seq
  where a.files > 0),
counts as (
  select p.seq, family(c.path) as family, c.cone, count(*) as n, max(c.scc) as scc
  from reach_points p join cones c on c.since <= p.seq and p.seq < c.until group by all),
histogram as (
  select a.seq, a.family, a.files, 0 as cone, a.files - coalesce(sum(c.n), 0) as n, 1 as scc
  from alive a left join counts c using (seq, family) group by a.seq, a.family, a.files
  union all
  select c.seq, c.family, a.files, c.cone, c.n, c.scc from counts c join alive a using (seq, family))
select seq, family, files, cone, n, scc, sum(n) over (partition by seq, family order by cone) - n as below
from histogram where n > 0;
