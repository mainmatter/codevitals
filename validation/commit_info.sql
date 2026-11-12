-- Commit labels for the validation study: apply to a scanned database with
--   duckdb codevitals.duckdb < validation/commit_info.sql
-- is_fix is the defect-fix rule checked in README.md here.

create or replace view commit_info as
with production as (
  select seq, count(*) as files from changes join path_roles using (path)
  where role = 'code' group by seq),
c as (
  select c.*, a.name || ' <' || a.email || '>' as author, a.is_bot,
         split_part(c.message, chr(10), 1) as subject, coalesce(p.files, 0) as production_files
  from commits c join authors a using (seq) left join production p using (seq))
select seq, id, time, author, message, subject,
  -- A defect fix in production behaviour: a fix keyword or a defect word in the
  -- subject, production code touched, and no sign of a test, CI, docs or lint fix.
  -- Checked on 150 hand-labelled RediSearch commits (validation/): precision 0.86,
  -- recall 0.89, against 0.48 and 0.89 for the keyword alone (is_fix_keyword).
  (regexp_matches(subject, '(?i)\b(fix|fixes|fixed|bug|hotfix|regression|revert)')
     or regexp_matches(subject, '(?i)(crash|leak|race\b|deadlock|hang|segfault|overflow|corrupt|wrong|incorrect|asan|use.after.free|double.free|null.pointer|panic)'))
    and production_files > 0
    and not regexp_matches(subject, '(?i)(test|flak|\bci\b|lint|typo|\bdocs?\b|readme|cmake|\bbuild\b|review|indent|format|chore|contributing|\blink)')
    as is_fix,
  regexp_matches(subject, '(?i)\b(fix|fixes|fixed|bug|hotfix|regression|revert)') as is_fix_keyword,
  regexp_matches(message, '(?i)co-authored-by:.*(claude|copilot|cursor|codex|devin|aider)') as is_agent,
  is_bot
from c;

comment on view commit_info is 'Commits with derived labels. is_fix: a defect fix in production behaviour (fix keyword or defect word, production code touched, no test/CI/docs/lint words); precision 0.86 and recall 0.89 on 150 hand-labelled commits, measured on the labels it was designed from. is_fix_keyword: the keyword alone (precision 0.48). is_agent: a co-author trailer names a coding agent; in squash merges it means an agent helped somewhere in the change. is_bot: the author is a [bot] account.';
