-- Per-unit tables: the values of production files and functions at every version, so
-- a repository-level value can be traced to the files and functions behind it. Each
-- row holds from commit `since` until before `until`.

-- Production files: blob_facts per version, with the file's lineage (renames keep it).
create or replace view units.files as
select v.path, l.lineage, v.blob, v.since, v.until, f.* exclude (blob, test_features)
from versions v join path_roles r on r.path = v.path and r.role = 'code'
join blob_facts f using (blob, test_features)
join (select distinct seq, path, lineage from lineages) l on l.path = v.path and l.seq = v.since;

comment on view units.files is 'Production files at each version [since, until): path, lineage (a stable id across renames), blob, and the facts the metrics sum (tokens, decisions, throws, gotos, global_state, fn_tokens, nested_tokens, deep_tokens, private_tokens, large_tokens).';

-- Production functions per file version. `function` identifies a function across
-- versions: its file''s lineage, its qualified name, and its ordinal when the file has
-- several of that name (`#2`, ...). A renamed function, or one moved to another
-- file, gets a new identity.
create or replace view units.functions as
select v.path, l.lineage,
       l.lineage || ' ' || f.qualified || if(f.ordinal > 1, '#' || f.ordinal, '') as function,
       v.blob, v.since, v.until, f.* exclude (blob, test_features)
from versions v join path_roles r on r.path = v.path and r.role = 'code'
join function_facts f using (blob, test_features)
join (select distinct seq, path, lineage from lineages) l on l.path = v.path and l.seq = v.since;

comment on view units.functions is 'Production functions at each version [since, until) of their file: function (lineage, qualified name and ordinal: an identity across versions), name, qualified, line, end_line, tokens, params, public, nesting (deepest block below the body), decisions, throws and gotos (in this function, not in functions inside it).';
