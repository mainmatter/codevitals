//! The walk: a repository's first-parent history into `commits` and `changes`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use duckdb::types::{TimeUnit, Value};
use duckdb::{Connection, OptionalExt, params};
use gix::ObjectId;
use gix::object::tree::diff::ChangeDetached as Change;
use rayon::prelude::*;
use tracing::{debug, info};

use codevitals::Lang;

use crate::attributes::Attributes;
use crate::db::{BATCH, oid};
use crate::parse::parseable;

/// Extend `commits` and `changes` with REV's first-parent chain past the stored tip.
pub fn walk(
    db: &mut Connection,
    path: &Path,
    repo: &gix::Repository,
    rev: &str,
) -> Result<(usize, usize)> {
    let tip = db
        .query_row(
            "select seq, id from commits order by seq desc limit 1",
            [],
            |r| Ok((r.get::<_, i32>(0)?, oid(&r.get::<_, String>(1)?))),
        )
        .optional()?;
    let find = |id: ObjectId| {
        repo.find_commit(id).with_context(|| match repo.is_shallow().unwrap_or(false) {
            true => format!(
                "commit {id} is missing: the repository is a shallow clone; fetch the full history \
                 (`git fetch --unshallow`; in GitHub Actions, actions/checkout with `fetch-depth: 0`)"
            ),
            false => format!("read commit {id}"),
        })
    };

    let head = commit(repo, rev)?.id;
    // New commits, newest first.
    let mut chain = Vec::new();
    let mut next = Some(head);
    while let Some(id) = next.filter(|id| tip.is_none_or(|(_, t)| t != *id)) {
        chain.push(id);
        next = find(id)?.parent_ids().next().map(|p| p.detach());
    }
    ensure!(
        tip.is_none() || next.is_some(),
        "{rev:?} does not extend the history stored in {path:?}: it was rewritten (force-pushed) \
         or is another repository or branch. Use one database per repository and branch, or reset \
         the walk with: duckdb {} \"delete from commits; delete from changes\" (parsed spans are kept)",
        path.display()
    );
    let started = Instant::now();
    let stored = tip.map_or(0, |(seq, _)| seq + 1);
    info!(new = chain.len(), stored, %rev, "walk");

    // The .gitattributes files of the stored tip; each commit then applies its own changes.
    let mut attributes = Attributes::default();
    let live: Vec<(String, String)> = db
        .prepare(
            "select path, blob from files where until = 2147483647
             and (path = '.gitattributes' or path like '%/.gitattributes')",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let read = |blob: &str| -> Result<Vec<u8>> {
        Ok(repo
            .find_blob(oid(blob))
            .with_context(|| format!("read .gitattributes blob {blob}"))?
            .data
            .to_vec())
    };
    for (path, blob) in &live {
        attributes.set(path, Some(read(blob)?));
    }

    let mut changes = 0;
    let mut seq = stored;
    let shared = repo.clone().into_sync();
    for batch in chain.rchunks(BATCH) {
        // Appenders join the open transaction, so each commit row and its changes land together.
        let tx = db.transaction()?;
        let mut commits = tx.appender("commits")?;
        let mut changed = tx.appender("changes")?;
        // Commits diff independently, so in parallel, each worker with its own handle;
        // rows are then appended in order, as .gitattributes carry over between commits.
        let diffed: Vec<Diffed> = batch
            .par_iter()
            .rev()
            .map_init(
                || shared.to_thread_local(),
                |repo, &id| -> Result<Diffed> {
                    let commit = repo.find_commit(id)?;
                    let rows = diff(repo, &commit).with_context(|| format!("diff commit {id}"))?;
                    let author = commit.author()?;
                    Ok((
                        id,
                        rows,
                        Value::Timestamp(TimeUnit::Second, commit.time()?.seconds),
                        author.name.to_string(),
                        author.email.to_string(),
                        commit.message_raw()?.to_string(),
                    ))
                },
            )
            .collect::<Result<_>>()?;
        for (id, rows, time, name, email, message) in diffed {
            commits.append_row(params![seq, id.to_string(), time, name, email, message])?;
            for (path, row) in rows.iter().filter(|(p, _)| Attributes::is_file(p)) {
                attributes.set(path, row.blob.as_deref().map(read).transpose()?);
            }
            for (path, row) in &rows {
                let Row {
                    blob,
                    source,
                    lines,
                    ..
                } = row;
                let (added, deleted) = lines.unzip();
                let (attr, test_features) = match blob {
                    Some(_) => (
                        attributes
                            .role(path)
                            .with_context(|| format!("commit {id}"))?,
                        attributes.test_features(path),
                    ),
                    None => (None, None),
                };
                changed.append_row(params![
                    seq,
                    path,
                    blob,
                    attr,
                    test_features,
                    source,
                    added,
                    deleted
                ])?;
            }
            debug!(seq, %id, changes = rows.len(), "commit");
            seq += 1;
            changes += rows.len();
        }
        commits.flush()?;
        changed.flush()?;
        drop((commits, changed));
        tx.commit()?;
        info!("walk: {}/{} commits", seq - stored, chain.len());
    }
    let entries = mailmap(db, repo, head)?;
    info!(commits = chain.len(), changes, mailmap = entries, elapsed = ?started.elapsed(), "walk done");
    Ok((chain.len(), changes))
}

/// The commit `rev` (a branch, tag, commit id, `HEAD~3`, ...) names.
pub fn commit<'r>(repo: &'r gix::Repository, rev: &str) -> Result<gix::Commit<'r>> {
    repo.rev_parse_single(rev)
        .map_err(anyhow::Error::from)
        .and_then(|r| Ok(r.object()?.peel_to_commit()?))
        .with_context(|| {
            format!(
                "resolve {rev:?} to a commit; pass a branch, tag or commit id \
                 (a repository whose HEAD has no commits, such as a jj workspace, needs one)"
            )
        })
}

/// Replaces `mailmap` with the entries of `.mailmap` at the scanned revision (none
/// when it has none). Lines that do not parse are skipped, as git skips them.
fn mailmap(db: &mut Connection, repo: &gix::Repository, head: ObjectId) -> Result<usize> {
    let tree = repo.find_commit(head)?.tree()?;
    let content = match tree.lookup_entry_by_path(".mailmap")? {
        Some(entry) if entry.mode().is_blob() => entry.object()?.detach().data,
        _ => Vec::new(),
    };
    let text = |s: Option<&gix::bstr::BStr>| s.map(|s| s.to_string());
    let tx = db.transaction()?;
    tx.execute("delete from mailmap", [])?;
    let mut rows = tx.appender("mailmap")?;
    let mut count = 0;
    for (line, entry) in gix::mailmap::parse_ignore_errors(&content).enumerate() {
        rows.append_row(params![
            line,
            text(entry.new_name()),
            text(entry.new_email()),
            text(entry.old_name()),
            entry.old_email().to_string()
        ])?;
        count += 1;
    }
    rows.flush()?;
    drop(rows);
    tx.commit()?;
    Ok(count)
}

/// A diffed commit: its id, changes, time, author name and email, and message.
type Diffed = (
    ObjectId,
    BTreeMap<String, Row>,
    Value,
    String,
    String,
    String,
);

/// A commit's change to one path.
#[derive(Default)]
pub struct Row {
    /// The new blob, `None` when the file is gone.
    pub blob: Option<String>,
    /// The path the file was renamed from.
    pub source: Option<String>,
    /// The blob the change replaces, to count lines against.
    pub old: Option<String>,
    /// Lines added and deleted; `None` for a file codevitals does not parse (see
    /// `parse::parseable`), and for the deletion a rename implies.
    pub lines: Option<(i32, i32)>,
}

/// A commit's changes against its first parent (the root's against the empty tree):
/// each changed path with its new blob and, for a rename, its source path, which
/// gets a row of its own as deleted. Renames are detected as git does by default:
/// a deleted and an added file at least 50% similar.
fn diff(repo: &gix::Repository, commit: &gix::Commit) -> Result<BTreeMap<String, Row>> {
    // The chain walk already found every parent, so a shallow boundary cannot hide one here.
    let prev = match commit.parent_ids().next() {
        Some(p) => repo.find_commit(p)?.tree()?,
        None => repo.empty_tree(),
    };
    diff_trees(repo, &prev, &commit.tree()?)
}

/// The changes from tree `prev` to `tree`, as `diff` describes them.
pub fn diff_trees(
    repo: &gix::Repository,
    prev: &gix::Tree,
    tree: &gix::Tree,
) -> Result<BTreeMap<String, Row>> {
    let mut rows = BTreeMap::new();
    let mut insert = |path: String, row: Row| {
        ensure!(
            !rows.contains_key(&path),
            "two changes for the path {path:?}"
        );
        rows.insert(path, row);
        Ok(())
    };
    let mut options = gix::diff::Options::default();
    options.track_rewrites(Some(gix::diff::Rewrites::default()));
    for c in repo.diff_tree_to_tree(prev, tree, options)? {
        let (old, new) = match c {
            Change::Addition { .. } => (None, file(c.entry_mode_and_id())),
            Change::Deletion { .. } => (file(c.entry_mode_and_id()), None),
            Change::Modification { .. } => (
                file(c.source_entry_mode_and_id()),
                file(c.entry_mode_and_id()),
            ),
            Change::Rewrite {
                ref source_location,
                copy,
                ..
            } => {
                let (old, new) = (
                    file(c.source_entry_mode_and_id()),
                    file(c.entry_mode_and_id()),
                );
                match (old.is_some(), new.is_some()) {
                    (true, true) => {
                        let source = path_text(source_location);
                        if !copy {
                            insert(source.clone(), Row::default())?;
                        }
                        let row = Row {
                            blob: new,
                            source: Some(source),
                            old,
                            lines: None,
                        };
                        insert(path_text(c.location()), row)?;
                        continue;
                    }
                    // A symlink or submodule on either side: no file was renamed.
                    _ => {
                        if old.is_some() && !copy {
                            let row = Row {
                                old,
                                ..Row::default()
                            };
                            insert(path_text(source_location), row)?;
                        }
                        (None, new)
                    }
                }
            }
        };
        if old != new {
            let row = Row {
                blob: new,
                old,
                ..Row::default()
            };
            insert(path_text(c.location()), row)?;
        }
    }
    for (path, row) in rows.iter_mut() {
        let renamed_away = row.blob.is_none() && row.old.is_none();
        if renamed_away || Lang::of(path).is_none() {
            continue;
        }
        let read = |blob: &Option<String>| -> Result<Option<Vec<u8>>> {
            Ok(match blob {
                Some(id) => Some(repo.find_blob(oid(id))?.take_data()),
                None => None,
            })
        };
        let (old, new) = (read(&row.old)?, read(&row.blob)?);
        let lang = Lang::of(path).expect("checked above");
        let parseable =
            |src: &Option<Vec<u8>>| src.as_deref().is_none_or(|src| parseable(lang, src));
        if parseable(&old) && parseable(&new) {
            row.lines = Some(line_changes(
                old.as_deref().unwrap_or_default(),
                new.as_deref().unwrap_or_default(),
            ));
        }
    }
    Ok(rows)
}

/// Lines added and deleted from `old` to `new`, as `git diff --numstat` counts them
/// (Myers' algorithm, so counts may differ from git's in rare ties).
fn line_changes(old: &[u8], new: &[u8]) -> (i32, i32) {
    use gix::diff::blob::{Algorithm, Diff, InternedInput};
    let input = InternedInput::new(old, new);
    let diff = Diff::compute(Algorithm::Myers, &input);
    let int = |n: u32| i32::try_from(n).expect("line counts fit in i32");
    (int(diff.count_additions()), int(diff.count_removals()))
}

/// A path as text: as is when it is UTF-8, else with each byte of an invalid
/// sequence written `\xNN`, so distinct paths stay distinct.
pub fn path_text(path: &[u8]) -> String {
    path.utf8_chunks().fold(String::new(), |mut text, chunk| {
        text.push_str(chunk.valid());
        for b in chunk.invalid() {
            write!(text, "\\x{b:02x}").expect("writing to a String succeeds");
        }
        text
    })
}

/// `B(tree, path)`: the blob id when the entry is a regular or executable file.
fn file((mode, id): (gix::object::tree::EntryMode, &gix::oid)) -> Option<String> {
    mode.is_blob().then(|| id.to_string())
}
