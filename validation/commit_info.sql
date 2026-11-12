-- commit_info, with the is_fix rule checked in README.md here, is part of codevitals'
-- SQL (sql/helpers.sql) since the defect check of `cv validate` uses it. Query it on
-- a scanned database, e.g.:
--   duckdb codevitals.duckdb "select seq, subject, is_fix from commit_info where is_fix"
select seq, id, time, author, subject, production_files, is_fix, is_fix_keyword, is_agent, is_bot
from commit_info;
