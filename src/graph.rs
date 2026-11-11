//! Reachability in the import graph. SQL derives the graph (`import_edges`, as
//! intervals); how many files each file reaches is a transitive closure, which a
//! recursive query would materialize pair by pair at every commit. Here each commit
//! that changes the graph gets one pass over strongly connected components with
//! bitsets, and `cones` stores each file's result as intervals for the metric views.

use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use anyhow::Result;
use duckdb::{Connection, params};
use tracing::info;

/// Where `derive` puts the cones: the stored table (by `scan`), or a temp table of the
/// same name that shadows it for this connection only (by `metrics`, which opens the
/// database read-only).
pub enum Cones {
    Stored,
    Temp,
}

/// Rebuilds `cones` from `import_edges`: from `since` to `until`, `path` reaches
/// `cone` other files and sits in a strongly connected component of `scc` files.
/// Files that reach nothing have no rows. Files import only files of their own
/// language family, so each family is its own graph: a commit recomputes only the
/// families whose edges it changes, over the files that have edges at that commit.
pub fn derive(db: &mut Connection, into: Cones) -> Result<()> {
    let start = Instant::now();
    let events: Vec<(i32, i32, String, String, String)> = db
        .prepare(
            "select since, 1, source, target, family(source) from import_edges
             union all select until, -1, source, target, family(source) from import_edges
             order by 1",
        )?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;

    let mut ids = HashMap::<String, usize>::new();
    let mut names = Vec::<String>::new();
    let mut id = |name: String| {
        *ids.entry(name).or_insert_with_key(|n| {
            names.push(n.clone());
            names.len() - 1
        })
    };
    let mut family_ids = HashMap::<String, usize>::new();
    let events: Vec<(i32, i32, usize, usize, usize)> = events
        .into_iter()
        .map(|(seq, delta, s, t, family)| {
            let next = family_ids.len();
            let family = *family_ids.entry(family).or_insert(next);
            (seq, delta, id(s), id(t), family)
        })
        .collect();

    let mut adj = vec![BTreeSet::new(); names.len()];
    let mut indegree = vec![0usize; names.len()];
    // import_edges has one row per import, so a pair of files can have several at once:
    // the pair is an edge while it has any.
    let mut imports = HashMap::<(usize, usize), u32>::new();
    let mut members = vec![BTreeSet::<usize>::new(); family_ids.len()]; // files ever in each family's graph
    let mut open = HashMap::<usize, (i32, u32, u32)>::new(); // file -> (since, cone, scc)
    let mut rows = Vec::<(usize, i32, i32, u32, u32)>::new();
    let mut passes = 0;
    for group in events.chunk_by(|a, b| a.0 == b.0) {
        let seq = group[0].0;
        let mut changed_families = BTreeSet::new();
        for &(_, delta, s, t, family) in group {
            let n = imports.entry((s, t)).or_default();
            if delta > 0 {
                *n += 1;
                if *n == 1 {
                    indegree[t] += 1;
                    adj[s].insert(t);
                }
            } else {
                *n -= 1;
                if *n == 0 {
                    indegree[t] -= 1;
                    adj[s].remove(&t);
                }
            }
            members[family].extend([s, t]);
            changed_families.insert(family);
        }
        for family in changed_families {
            passes += 1;
            // The family's files with an edge now, numbered locally.
            let live: Vec<usize> = members[family]
                .iter()
                .copied()
                .filter(|&f| !adj[f].is_empty() || indegree[f] > 0)
                .collect();
            let local: HashMap<usize, usize> =
                live.iter().enumerate().map(|(i, &f)| (f, i)).collect();
            let graph: Vec<BTreeSet<usize>> = live
                .iter()
                .map(|&f| adj[f].iter().map(|t| local[t]).collect())
                .collect();
            let now: HashMap<usize, (u32, u32)> = live.iter().copied().zip(cones(&graph)).collect();
            for &file in &members[family] {
                let (cone, scc) = now.get(&file).copied().unwrap_or((0, 1));
                let previous = open.get(&file).map(|&(_, c, s)| (c, s));
                if previous == Some((cone, scc)) || (previous.is_none() && cone == 0) {
                    continue;
                }
                if let Some((since, c, s)) = open.remove(&file) {
                    rows.push((file, since, seq, c, s));
                }
                if cone > 0 {
                    open.insert(file, (seq, cone, scc));
                }
            }
        }
    }
    assert!(
        open.is_empty(),
        "every import_edges interval ends, so every cone closes"
    );

    let tx = db.transaction()?;
    match into {
        Cones::Stored => tx.execute_batch("delete from main.cones")?,
        Cones::Temp => tx.execute_batch(
            "create or replace temp table cones as select * from main.cones limit 0",
        )?,
    }
    {
        let mut app = match into {
            Cones::Stored => tx.appender("cones")?,
            Cones::Temp => tx.appender_to_catalog_and_db("cones", "temp", "main")?,
        };
        for &(file, since, until, cone, scc) in &rows {
            app.append_row(params![names[file], since, until, cone, scc])?;
        }
    }
    tx.commit()?;
    info!(
        files = names.len(),
        passes,
        rows = rows.len(),
        elapsed = ?start.elapsed(),
        "cones derived"
    );
    Ok(())
}

/// For every node: how many other nodes it reaches, and the size of its strongly
/// connected component. Tarjan's algorithm (iterative, so deep chains cannot
/// overflow the stack) emits components sinks first, so each component's reach is
/// its members plus the reach of components it points to, all already known.
fn cones(adj: &[BTreeSet<usize>]) -> Vec<(u32, u32)> {
    const NONE: usize = usize::MAX;
    let n = adj.len();
    let words = n.div_ceil(64);
    let (mut index, mut low, mut comp) = (vec![NONE; n], vec![0; n], vec![NONE; n]);
    let (mut stack, mut on_stack) = (Vec::new(), vec![false; n]);
    let mut reach: Vec<Vec<u64>> = Vec::new(); // per component, in emission order
    let mut sizes = Vec::new();
    let mut next = 0;
    for root in 0..n {
        if index[root] != NONE {
            continue;
        }
        // Each frame: a node and the successors still to visit.
        let mut frames = vec![(root, adj[root].iter())];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some((v, succ)) = frames.last_mut() {
            let v = *v;
            if let Some(&w) = succ.next() {
                if index[w] == NONE {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push((w, adj[w].iter()));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let c = reach.len();
                let mut bits = vec![0u64; words];
                let mut members = Vec::new();
                loop {
                    let w = stack.pop().expect("v is on the stack");
                    on_stack[w] = false;
                    comp[w] = c;
                    bits[w / 64] |= 1 << (w % 64);
                    members.push(w);
                    if w == v {
                        break;
                    }
                }
                for &m in &members {
                    for &w in &adj[m] {
                        if comp[w] != c {
                            assert!(comp[w] < c, "successor components are emitted first");
                            for (b, r) in bits.iter_mut().zip(&reach[comp[w]]) {
                                *b |= r;
                            }
                        }
                    }
                }
                reach.push(bits);
                sizes.push(members.len());
            }
        }
    }
    (0..n)
        .map(|v| {
            let c = comp[v];
            let reached: u32 = reach[c].iter().map(|w| w.count_ones()).sum();
            (reached - 1, u32::try_from(sizes[c]).expect("fits"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(n: usize, edges: &[(usize, usize)]) -> Vec<BTreeSet<usize>> {
        let mut adj = vec![BTreeSet::new(); n];
        for &(s, t) in edges {
            adj[s].insert(t);
        }
        adj
    }

    /// Cones and component sizes by hand on a chain with a cycle and an isolated node;
    /// then bitset cones equal a breadth-first search from every node, on graphs dense
    /// enough for cycles and wider than one 64-bit word.
    #[test]
    fn cones_match_breadth_first_search() {
        // 0 -> 1 -> 2 <-> 3 -> 4, and 5 alone.
        let adj = graph(6, &[(0, 1), (1, 2), (2, 3), (3, 2), (3, 4)]);
        assert_eq!(
            cones(&adj),
            [(4, 1), (3, 1), (2, 2), (2, 2), (0, 1), (0, 1)]
        );
        let mut state = 7u64;
        let mut random = |m: usize| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize % m
        };
        for n in [1, 5, 70, 200] {
            let edges: Vec<_> = (0..n * 2).map(|_| (random(n), random(n))).collect();
            let adj = graph(n, &edges);
            let expected: Vec<u32> = (0..n)
                .map(|v| {
                    let mut seen = vec![false; n];
                    let mut todo = vec![v];
                    seen[v] = true;
                    while let Some(u) = todo.pop() {
                        for &w in &adj[u] {
                            if !seen[w] {
                                seen[w] = true;
                                todo.push(w);
                            }
                        }
                    }
                    u32::try_from(seen.iter().filter(|&&s| s).count() - 1).unwrap()
                })
                .collect();
            let actual: Vec<u32> = cones(&adj).into_iter().map(|(c, _)| c).collect();
            assert_eq!(actual, expected, "n = {n}");
        }
    }
}
