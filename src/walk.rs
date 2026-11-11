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
use tracing::{debug, info};

use crate::attributes::Attributes;
use crate::db::{BATCH, oid};

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

    // New commits, newest first.
    let mut chain = Vec::new();
    let head = repo
        .rev_parse_single(rev)
        .and_then(|r| Ok(r.object()?.peel_to_commit()?.id))
        .map_err(anyhow::Error::from)
        .with_context(|| {
            format!(
                "resolve {rev:?} to a commit; pass a branch, tag or commit id \
                 (a repository whose HEAD has no commits, such as a jj workspace, needs one)"
            )
        })?;
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
    info!(new = chain.len(), stored = tip.map_or(0, |(seq, _)| seq + 1), %rev, "walk");

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
    let mut seq = tip.map_or(0, |(seq, _)| seq + 1);
    for (b, batch) in chain.rchunks(BATCH).enumerate() {
        // Appenders join the open transaction, so each commit row and its changes land together.
        let tx = db.transaction()?;
        let mut commits = tx.appender("commits")?;
        let mut changed = tx.appender("changes")?;
        for &id in batch.iter().rev() {
            let diffing = Instant::now();
            let commit = find(id)?;
            let (rows, time, author, message) = (|| -> Result<_> {
                // The previous commit on the chain is the first parent; the root diffs against the empty tree.
                let prev = match commit.parent_ids().next() {
                    Some(p) => find(p.detach())?.tree()?,
                    None => repo.empty_tree(),
                };
                let mut rows = BTreeMap::new();
                for c in
                    repo.diff_tree_to_tree(&prev, &commit.tree()?, gix::diff::Options::default())?
                {
                    let (old, new) = match c {
                        Change::Addition { .. } => (None, file(c.entry_mode_and_id())),
                        Change::Deletion { .. } => (file(c.entry_mode_and_id()), None),
                        Change::Modification { .. } => (
                            file(c.source_entry_mode_and_id()),
                            file(c.entry_mode_and_id()),
                        ),
                        Change::Rewrite { .. } => unreachable!("rename tracking is off"),
                    };
                    if old != new {
                        let path = path_text(c.location());
                        ensure!(
                            !rows.contains_key(&path),
                            "two changes for the path {path:?}"
                        );
                        rows.insert(path, new);
                    }
                }
                let time = Value::Timestamp(TimeUnit::Second, commit.time()?.seconds);
                let author = commit.author()?;
                let author = format!("{} <{}>", author.name, author.email);
                Ok((rows, time, author, commit.message_raw()?.to_string()))
            })()
            .with_context(|| format!("read commit {id}"))?;
            commits.append_row(params![seq, id.to_string(), time, author, message])?;
            for (path, blob) in rows.iter().filter(|(p, _)| Attributes::is_file(p)) {
                attributes.set(path, blob.as_deref().map(read).transpose()?);
            }
            for (path, blob) in &rows {
                let attr = match blob {
                    Some(_) => attributes
                        .role(path)
                        .with_context(|| format!("commit {id}"))?,
                    None => None,
                };
                changed.append_row(params![seq, path, blob, attr])?;
            }
            debug!(seq, %id, changes = rows.len(), elapsed = ?diffing.elapsed(), "commit");
            seq += 1;
            changes += rows.len();
        }
        commits.flush()?;
        changed.flush()?;
        drop((commits, changed));
        tx.commit()?;
        info!(
            "walk: {}/{} commits",
            (b * BATCH + batch.len()),
            chain.len()
        );
    }
    info!(commits = chain.len(), changes, elapsed = ?started.elapsed(), "walk done");
    Ok((chain.len(), changes))
}

/// A path as text: as is when it is UTF-8, else with each byte of an invalid
/// sequence written `\xNN`, so distinct paths stay distinct.
fn path_text(path: &[u8]) -> String {
    match std::str::from_utf8(path) {
        Ok(text) => text.to_owned(),
        Err(_) => path.utf8_chunks().fold(String::new(), |mut text, chunk| {
            text.push_str(chunk.valid());
            for b in chunk.invalid() {
                write!(text, "\\x{b:02x}").expect("writing to a String succeeds");
            }
            text
        }),
    }
}

/// `B(tree, path)`: the blob id when the entry is a regular or executable file.
fn file((mode, id): (gix::object::tree::EntryMode, &gix::oid)) -> Option<String> {
    mode.is_blob().then(|| id.to_string())
}
