-- One row: the layout and extractor versions of the data (db.rs).
create table if not exists tally (schema integer, extractor integer);
-- The metric views (metrics.sql).
create schema if not exists metrics;

create table if not exists commits (
  seq integer, id text, time timestamp, author text, message text);
-- attr: the role .gitattributes give the path at that commit (attributes.rs).
create table if not exists changes (seq integer, path text, blob text, attr text);
create table if not exists spans (
  blob text, v integer, lang text, kind text, name text,
  line integer, end_line integer, tokens integer, comments integer,
  params integer, hash ubigint, errors integer, start_byte integer, end_byte integer,
  depth integer);

-- Derived from import_edges by graph.rs, not a fact.
create table if not exists cones (path text, since integer, until integer, cone integer, scc integer);

create or replace view files as
select path, blob, since, until from (
  select path, blob, seq as since,
         coalesce(lead(seq) over (partition by path order by seq), 2147483647) as until
  from changes
) where blob is not null;
