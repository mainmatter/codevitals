//! Reachability in the import graph. SQL derives the graph (`import_edges`, as
//! intervals); how many files each file reaches is a transitive closure, which a
//! recursive query would materialize pair by pair at every commit. Here each commit
//! that changes the graph gets one pass over strongly connected components with
//! bitsets, and `cones` stores each file's result as intervals for the metric views.

use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use anyhow::Result;
use duckdb::{Connection, params};
use petgraph::algo::kosaraju_scc;
use petgraph::graph::{DiGraph, NodeIndex};
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
    // Files and families by dense id, numbered as they first appear.
    let (mut files, mut families) = (HashMap::new(), HashMap::new());
    let intern = |ids: &mut HashMap<String, usize>, name: String| {
        let next = ids.len();
        *ids.entry(name).or_insert(next)
    };
    // Planning the import views costs most of a second: skip them without imports.
    let imports: bool = db.query_row(
        "select exists (select 1 from spans where kind = 'import')",
        [],
        |r| r.get(0),
    )?;
    // Each edge interval once (computing import_edges is most of the cost), as two
    // events: the pair gains an import at `since` and loses it at `until`. Sorted
    // here: DuckDB returns a large result out of order when it need not preserve
    // insertion order (codevitals' setting), even under ORDER BY.
    let mut events = Vec::<(i32, i32, usize, usize, usize)>::new();
    if imports {
        let mut edges =
            db.prepare("select since, until, source, target, family(source) from import_edges")?;
        let mut rows = edges.query([])?;
        while let Some(r) = rows.next()? {
            let (s, t) = (intern(&mut files, r.get(2)?), intern(&mut files, r.get(3)?));
            let family = intern(&mut families, r.get(4)?);
            events.push((r.get(0)?, 1, s, t, family));
            events.push((r.get(1)?, -1, s, t, family));
        }
    }
    events.sort_unstable_by_key(|e| e.0);
    let mut names = vec![String::new(); files.len()];
    for (name, id) in files {
        names[id] = name;
    }

    let mut adj = vec![BTreeSet::new(); names.len()];
    let mut indegree = vec![0usize; names.len()];
    // import_edges has one row per import, so a pair of files can have several at once:
    // the pair is an edge while it has any.
    let mut imports = HashMap::<(usize, usize), u32>::new();
    let mut members = vec![BTreeSet::<usize>::new(); families.len()]; // files ever in each family's graph
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
                *n = n.checked_sub(1).expect("an import ends after it starts");
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
    let mut app = match into {
        Cones::Stored => {
            tx.execute_batch("delete from main.cones")?;
            tx.appender("cones")?
        }
        Cones::Temp => {
            tx.execute_batch(
                "create or replace temp table cones as select * from main.cones limit 0",
            )?;
            tx.appender_to_catalog_and_db("cones", "temp", "main")?
        }
    };
    for &(file, since, until, cone, scc) in &rows {
        app.append_row(params![names[file], since, until, cone, scc])?;
    }
    app.flush()?;
    drop(app);
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
/// connected component. Kosaraju's algorithm (iterative, so deep chains cannot
/// overflow the stack) yields components sinks first, so each component's reach is
/// its members plus the reach of components it points to, all already known.
fn cones(adj: &[BTreeSet<usize>]) -> Vec<(u32, u32)> {
    let mut graph = DiGraph::<(), ()>::with_capacity(adj.len(), 0);
    adj.iter().for_each(|_| _ = graph.add_node(()));
    for (s, targets) in adj.iter().enumerate() {
        for &t in targets {
            graph.add_edge(NodeIndex::new(s), NodeIndex::new(t), ());
        }
    }
    let (mut comp, mut reach) = (vec![usize::MAX; adj.len()], Vec::<Vec<u64>>::new());
    let sccs = kosaraju_scc(&graph);
    for (c, members) in sccs.iter().enumerate() {
        let mut bits = vec![0u64; adj.len().div_ceil(64)];
        for m in members.iter().map(|m| m.index()) {
            comp[m] = c;
            bits[m / 64] |= 1 << (m % 64);
        }
        for &w in members.iter().flat_map(|m| &adj[m.index()]) {
            if comp[w] != c {
                assert!(comp[w] < c, "successor components come first");
                for (b, r) in bits.iter_mut().zip(&reach[comp[w]]) {
                    *b |= r;
                }
            }
        }
        reach.push(bits);
    }
    comp.iter()
        .map(|&c| {
            let reached: u32 = reach[c].iter().map(|w| w.count_ones()).sum();
            (reached - 1, u32::try_from(sccs[c].len()).expect("fits"))
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
        // A chain far deeper than a recursive search could follow on a test thread's 2 MiB stack.
        let n = 30_000;
        let chain = graph(n, &(1..n).map(|i| (i - 1, i)).collect::<Vec<_>>());
        assert_eq!(cones(&chain)[0], (u32::try_from(n - 1).unwrap(), 1));
    }
}
