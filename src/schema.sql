create table if not exists commits (
  seq integer, id text, time timestamp, author text, message text);
create table if not exists changes (seq integer, path text, blob text);
create table if not exists spans (
  blob text, v integer, lang text, kind text, name text,
  line integer, end_line integer, tokens integer, comments integer,
  params integer, hash ubigint, errors integer);

create or replace view files as
select path, blob, since, until from (
  select path, blob, seq as since,
         coalesce(lead(seq) over (partition by path order by seq), 2147483647) as until
  from changes
) where blob is not null;
