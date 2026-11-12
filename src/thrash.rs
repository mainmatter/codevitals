//! Edit thrash within a range: lines that commits of BASE..HEAD added and later
//! commits of it deleted again. An experimental measure (`cv diff` reports it).
//!
//! It follows the first-parent chain from HEAD back to BASE and diffs each commit
//! against its first parent, carrying for every line of every source file whether a
//! commit of the range added it. A squash-merged range is one commit: what happened
//! inside the squashed branch is invisible, and only thrash across the first-parent
//! commits shows. Lines added and deleted inside one commit never show. This is not
//! a measure of leftover or unnecessary edits, which survive into HEAD: finding those
//! takes rerunning tests.

use std::collections::HashMap;

use anyhow::Result;
use codevitals::Lang;
use gix::diff::blob::{Algorithm, Diff, InternedInput};

use crate::db::oid;
use crate::parse::parseable;
use crate::walk::diff_trees;

/// Lines of source files added by the range's commits, and how many of them later
/// commits of the range deleted.
pub struct Thrash {
    pub commits: usize,
    pub added: u64,
    pub thrashed: u64,
}

/// The thrash of `base..head`, or `None` when `base` is not on `head`'s first-parent
/// chain.
pub fn thrash(
    repo: &gix::Repository,
    base: &gix::Commit,
    head: &gix::Commit,
) -> Result<Option<Thrash>> {
    let mut chain = Vec::new();
    let mut next = Some(head.id);
    while let Some(id) = next {
        if id == base.id {
            break;
        }
        let commit = repo.find_commit(id)?;
        next = commit.parent_ids().next().map(|p| p.detach());
        chain.push(commit);
    }
    if next.is_none() {
        return Ok(None);
    }
    chain.reverse();
    let read = |blob: &Option<String>| -> Result<Option<Vec<u8>>> {
        Ok(match blob {
            Some(id) => Some(repo.find_blob(oid(id))?.take_data()),
            None => None,
        })
    };
    // Per path: for each line of its current version, whether the range added it.
    let mut lines: HashMap<String, Vec<bool>> = HashMap::new();
    let (mut added, mut thrashed) = (0u64, 0u64);
    for commit in &chain {
        let prev = repo
            .find_commit(commit.parent_ids().next().expect("on the chain"))?
            .tree()?;
        for (path, row) in diff_trees(repo, &prev, &commit.tree()?)? {
            let Some(lang) = Lang::of(&path) else {
                continue;
            };
            let from = row.source.clone().unwrap_or_else(|| path.clone());
            let carried = lines.remove(&from);
            let (old, new) = (read(&row.old)?, read(&row.blob)?);
            let ok = |src: &Option<Vec<u8>>| src.as_deref().is_none_or(|s| parseable(lang, s));
            if !ok(&old) || !ok(&new) {
                continue;
            }
            let (old, new) = (old.unwrap_or_default(), new.unwrap_or_default());
            let input = InternedInput::new(old.as_slice(), new.as_slice());
            let before = carried
                .filter(|f| f.len() == input.before.len())
                .unwrap_or_else(|| vec![false; input.before.len()]);
            let diff = Diff::compute(Algorithm::Myers, &input);
            let mut after = Vec::with_capacity(input.after.len());
            let mut pos = 0usize;
            for hunk in diff.hunks() {
                let (b, a) = (
                    hunk.before.start as usize..hunk.before.end as usize,
                    hunk.after.clone(),
                );
                after.extend_from_slice(&before[pos..b.start]);
                thrashed += before[b.clone()].iter().filter(|&&f| f).count() as u64;
                added += a.len() as u64;
                after.extend(std::iter::repeat_n(true, a.len()));
                pos = b.end;
            }
            after.extend_from_slice(&before[pos..]);
            if row.blob.is_some() {
                lines.insert(path, after);
            }
        }
    }
    Ok(Some(Thrash {
        commits: chain.len(),
        added,
        thrashed,
    }))
}
