pub mod links;
pub mod time;
pub mod tips;
pub mod writer;

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use rusqlite::{CachedStatement, Connection, OptionalExtension, params};

use crate::cli::GenealogyArgs;
use crate::genealogy::links::{Edge, Event, Kind, Node, PairId, Root, Version, VersionId};
use crate::process::Interrupt;
use crate::store;

pub fn run(args: GenealogyArgs, interrupt: &Interrupt) -> Result<()> {
    // 読む前に確かめるのは，記録を全部読んだ後で止まらないためである．名前を替えるときにも，もう一度確かめる
    if args.output.symlink_metadata().is_ok() {
        bail!("genealogy file {} already exists", args.output.display());
    }
    let events_path = std::fs::canonicalize(&args.db)
        .with_context(|| format!("cannot open database {}", args.db.display()))?;
    let events_text = events_path
        .to_str()
        .with_context(|| format!("database path {} is not UTF-8", events_path.display()))?
        .to_string();
    // 開く前に確かめるのは，record がジャーナルを DELETE に戻すのを止めないためである．
    if store::is_wal(&args.db)? {
        bail!(
            "the journal of {} is still WAL; wait until record exits, or if record failed to switch it, \
             run PRAGMA journal_mode=DELETE on it with SQLite as record said",
            args.db.display()
        );
    }
    let mut conn = store::open_read(&args.db)?;
    store::check_format(&conn)?;

    let tx = conn.transaction()?;
    let (run_id, status): (String, String) =
        tx.query_row("SELECT run_id, status FROM run", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    if status != "finished" {
        bail!("the record is {status}, not finished; genealogy reads only a finished record");
    }
    let two_parents: Option<String> = tx
        .query_row(
            "SELECT v.commit_id FROM input_parent p JOIN version v ON v.id = p.version
             GROUP BY p.version HAVING count(*) > 1 ORDER BY v.topo_order LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(commit) = two_parents {
        bail!(
            "version {commit} has two or more parents in the input graph; genealogy accepts only \
             records of graphs where every version has at most one parent, such as a first-parent line"
        );
    }

    let read = read_graph(&tx)?;
    let mut source = DbSource {
        roots: tx.prepare_cached("SELECT pair FROM root_pair WHERE version = ?1 ORDER BY pair")?,
        events: tx.prepare_cached(
            "SELECT id, event, parent_pair, child_pair FROM pair_event WHERE edge = ?1 ORDER BY id",
        )?,
    };
    let mut walk = links::walk(&read.versions, &read.edges, &mut source, interrupt)?;
    drop(source);
    let effective: HashMap<VersionId, i64> =
        read.effective_in_graph_order.iter().copied().collect();
    let mut triple = tx.prepare_cached(
        "SELECT a.path, a.start_line, a.end_line, b.path, b.start_line, b.end_line FROM pair p
         JOIN fragment_view a ON a.id = p.fragment_a JOIN fragment_view b ON b.id = p.fragment_b
         WHERE p.id = ?1",
    )?;
    let summary = tips::summarize(&mut walk, &effective, &read.topo, |_, births| {
        sort_by_triple(&mut triple, births)
    })?;
    drop(triple);
    tx.finish()?;
    conn.close().map_err(|(_, err)| err)?;

    writer::write(
        &args.output,
        &writer::Source {
            run_id: &run_id,
            path: &events_text,
        },
        &read.effective_in_graph_order,
        &walk,
        &summary,
        interrupt,
    )?;
    log::info!(
        "wrote {} genealogies to {}",
        summary.genealogies.len(),
        args.output.display()
    );
    Ok(())
}

struct Read {
    versions: Vec<Version>,
    edges: Vec<Edge>,
    effective_in_graph_order: Vec<(VersionId, i64)>,
    topo: HashMap<VersionId, i64>,
}

fn read_graph(conn: &Connection) -> Result<Read> {
    let rows: Vec<(VersionId, i64, String, String, Option<String>)> = conn
        .prepare(
            "SELECT id, topo_order, committed_at, status, root FROM version ORDER BY topo_order",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let parent: HashMap<VersionId, VersionId> = conn
        .prepare("SELECT version, parent FROM input_parent")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut committed = Vec::with_capacity(rows.len());
    for (id, _, at, _, _) in &rows {
        committed.push((*id, store::parse_time(at)?));
    }
    let effective = time::effective_times(&committed, &parent)?;
    let mut versions = Vec::with_capacity(rows.len());
    let mut topo = HashMap::with_capacity(rows.len());
    for (id, order, _, status, root) in rows {
        let ok = match status.as_str() {
            "ok" => true,
            "broken" => false,
            other => bail!("the record is broken: version {id} is {other} in a finished record"),
        };
        let root = match root.as_deref() {
            None => None,
            Some("empty_tree") => Some(Root::EmptyTree),
            Some("censored") => Some(Root::Censored),
            Some(other) => bail!("the record is broken: version {id} has root {other:?}"),
        };
        topo.insert(id, order);
        versions.push(Version {
            id,
            ok,
            root,
            effective: effective[&id],
        });
    }
    let edges = conn
        .prepare("SELECT id, parent, child, skipped_versions FROM edge ORDER BY id")?
        .query_map([], |row| {
            Ok(Edge {
                id: row.get(0)?,
                parent: row.get(1)?,
                child: row.get(2)?,
                length: 1 + row.get::<_, i64>(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let effective_in_graph_order = committed
        .iter()
        .map(|(id, _)| (*id, effective[id]))
        .collect();
    Ok(Read {
        versions,
        edges,
        effective_in_graph_order,
        topo,
    })
}

struct DbSource<'a> {
    roots: CachedStatement<'a>,
    events: CachedStatement<'a>,
}

impl links::Source for DbSource<'_> {
    fn root_pairs(&mut self, version: VersionId) -> Result<Vec<PairId>> {
        Ok(self
            .roots
            .query_map(params![version], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn events(&mut self, edge: i64) -> Result<Vec<Event>> {
        let rows: Vec<(i64, String, Option<PairId>, Option<PairId>)> = self
            .events
            .query_map(params![edge], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(id, kind, parent, child)| {
                let kind = match kind.as_str() {
                    "Appear" => Kind::Appear,
                    "Persist" => Kind::Persist,
                    "Merge" => Kind::Merge,
                    "Disappear" => Kind::Disappear,
                    other => bail!("the record is broken: event {id} is {other:?}"),
                };
                Ok(Event {
                    id,
                    kind,
                    parent,
                    child,
                })
            })
            .collect()
    }
}

type Triple = (String, u32, u32, String, u32, u32);

/// 3 つ組の順は，辞書の番号を振る順と同じく，2 つの片のパス，開始行，終了行の順に比べる．
fn sort_by_triple(
    statement: &mut CachedStatement,
    births: Vec<(PairId, Node)>,
) -> Result<Vec<(PairId, Node)>> {
    let mut keyed: Vec<(Triple, PairId, Node)> = Vec::with_capacity(births.len());
    for (pair, node) in births {
        let triple: Triple = statement.query_row(params![pair], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?;
        keyed.push((triple, pair, node));
    }
    keyed.sort_unstable();
    Ok(keyed
        .into_iter()
        .map(|(_, pair, node)| (pair, node))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::links::memory::Memory;
    use super::links::{BirthKind, Kind, Role, walk};
    use super::tips::{Genealogy, summarize};
    use super::*;

    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    type Occurrence = (VersionId, PairId);

    /// 無作為な記録と，出現を全部作る素朴な計算に要るもの
    #[derive(Default)]
    struct Case {
        versions: Vec<Version>,
        edges: Vec<Edge>,
        memory: Memory,
        states: BTreeMap<VersionId, BTreeSet<PairId>>,
        links: BTreeMap<Occurrence, Vec<(Occurrence, i64)>>,
        births: BTreeMap<Occurrence, BirthKind>,
        disappears: BTreeMap<Occurrence, Vec<i64>>,
        events: Vec<(i64, Occurrence, Kind)>,
        pairs: PairId,
    }

    impl Case {
        fn event(
            &mut self,
            edge: i64,
            kind: Kind,
            at: Occurrence,
            parent: Option<PairId>,
            child: Option<PairId>,
        ) -> i64 {
            self.memory.edge(edge, &[(kind, parent, child)]);
            let id = self.events.len() as i64 + 1;
            self.events.push((id, at, kind));
            id
        }

        fn new_pair(&mut self) -> PairId {
            self.pairs += 1;
            self.pairs
        }
    }

    /// 版の番号を作る順にするので，番号の順がグラフの順になる．親は 1 つ以下で，子は複数になりうる
    fn random_case(seed: u64) -> Case {
        let mut rng = Lcg(seed);
        let mut case = Case::default();
        let n = 1 + rng.below(12) as i64;
        let mut parent: BTreeMap<VersionId, VersionId> = BTreeMap::new();
        let mut ok: BTreeMap<VersionId, bool> = BTreeMap::new();
        let mut effective: BTreeMap<VersionId, i64> = BTreeMap::new();
        for v in 1..=n {
            if v > 1 && rng.below(100) < 85 {
                parent.insert(v, 1 + rng.below(v as u64 - 1) as i64);
            }
            let mut time = rng.below(20) as i64 * 43_200;
            if let Some(p) = parent.get(&v) {
                time = time.max(effective[p]);
            }
            effective.insert(v, time);
            ok.insert(v, rng.below(100) >= 20);
            let (mut ancestor, mut skipped) = (parent.get(&v).copied(), 0);
            while let Some(a) = ancestor.filter(|a| !ok[a]) {
                skipped += 1;
                ancestor = parent.get(&a).copied();
            }
            let mut root = None;
            if ok[&v] {
                let mut state = BTreeSet::new();
                let edge = case.edges.len() as i64 + 1;
                match ancestor {
                    None if rng.below(2) == 0 => {
                        root = Some(Root::Censored);
                        for _ in 0..rng.below(4) {
                            let pair = case.new_pair();
                            state.insert(pair);
                            case.births.insert((v, pair), BirthKind::LeftCensored);
                        }
                        case.memory.roots.insert(v, state.iter().copied().collect());
                    }
                    None => {
                        root = Some(Root::EmptyTree);
                        case.edges.push(Edge {
                            id: edge,
                            parent: None,
                            child: v,
                            length: 1,
                        });
                    }
                    Some(a) => {
                        let length = 1 + skipped;
                        case.edges.push(Edge {
                            id: edge,
                            parent: Some(a),
                            child: v,
                            length,
                        });
                        let before: Vec<PairId> = case.states[&a].iter().copied().collect();
                        let mut k = 0;
                        while k < before.len() {
                            let pair = before[k];
                            let choice = rng.below(100);
                            let group = (2 + rng.below(2) as usize).min(before.len() - k);
                            if choice < 45 {
                                state.insert(pair);
                                case.links
                                    .entry((v, pair))
                                    .or_default()
                                    .push(((a, pair), length));
                            } else if choice < 70 || (choice >= 85 && group < 2) {
                                let child = if choice < 60 { pair } else { case.new_pair() };
                                state.insert(child);
                                case.event(edge, Kind::Persist, (a, pair), Some(pair), Some(child));
                                case.links
                                    .entry((v, child))
                                    .or_default()
                                    .push(((a, pair), length));
                            } else if choice < 85 {
                                let id =
                                    case.event(edge, Kind::Disappear, (a, pair), Some(pair), None);
                                case.disappears.entry((a, pair)).or_default().push(id);
                            } else {
                                let child = case.new_pair();
                                state.insert(child);
                                for &from in &before[k..k + group] {
                                    case.event(
                                        edge,
                                        Kind::Merge,
                                        (a, from),
                                        Some(from),
                                        Some(child),
                                    );
                                    case.links
                                        .entry((v, child))
                                        .or_default()
                                        .push(((a, from), length));
                                }
                                k += group - 1;
                            }
                            k += 1;
                        }
                    }
                }
                if root != Some(Root::Censored) {
                    for _ in 0..rng.below(3) {
                        let pair = case.new_pair();
                        state.insert(pair);
                        case.event(edge, Kind::Appear, (v, pair), None, Some(pair));
                        case.births.insert((v, pair), BirthKind::Appear);
                    }
                }
                case.states.insert(v, state);
            }
            case.versions.push(Version {
                id: v,
                ok: ok[&v],
                root,
                effective: time,
            });
        }
        case
    }

    /// 出現を全部作って，系譜の作り方を字のとおりに当てて求める．系譜ごとに，値，出現の集まり，解消のイベントを返す
    #[allow(clippy::type_complexity)]
    fn naive(
        case: &Case,
    ) -> (
        Vec<(Genealogy, BTreeSet<Occurrence>)>,
        BTreeSet<i64>,
        BTreeSet<Occurrence>,
    ) {
        let effective: BTreeMap<VersionId, i64> =
            case.versions.iter().map(|v| (v.id, v.effective)).collect();
        let mut out: BTreeMap<VersionId, Vec<(VersionId, i64)>> = BTreeMap::new();
        for edge in &case.edges {
            if let Some(parent) = edge.parent {
                out.entry(parent)
                    .or_default()
                    .push((edge.child, edge.length));
            }
        }
        let occurrences: Vec<Occurrence> = case
            .states
            .iter()
            .flat_map(|(v, pairs)| pairs.iter().map(|p| (*v, *p)))
            .collect();
        // つながりの向きを無視して連結な出現の集まりを，行き渡るまで小さい印に寄せて求める
        let mut mark: BTreeMap<Occurrence, Occurrence> =
            occurrences.iter().map(|o| (*o, *o)).collect();
        loop {
            let mut changed = false;
            for (child, parents) in &case.links {
                for (from, _) in parents {
                    let low = mark[child].min(mark[from]);
                    for o in [child, from] {
                        if mark[o] != low {
                            mark.insert(*o, low);
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        let mut depth: BTreeMap<Occurrence, (i64, i64)> = BTreeMap::new();
        for o in &occurrences {
            let d = match case.links.get(o) {
                None => (0, 0),
                Some(parents) => (
                    parents.iter().map(|(p, _)| depth[p].0 + 1).max().unwrap(),
                    parents.iter().map(|(p, l)| depth[p].1 + l).max().unwrap(),
                ),
            };
            depth.insert(*o, d);
        }
        let continued: BTreeSet<Occurrence> =
            case.links.values().flatten().map(|(p, _)| *p).collect();
        let mut ends = BTreeSet::new();
        let mut censored_tips = BTreeSet::new();
        let mut groups: BTreeMap<Occurrence, Vec<Occurrence>> = BTreeMap::new();
        for o in &occurrences {
            groups.entry(mark[o]).or_default().push(*o);
        }
        let mut result = Vec::new();
        for members in groups.values() {
            let births: Vec<Occurrence> = members
                .iter()
                .filter(|o| case.births.contains_key(o))
                .copied()
                .collect();
            let tips: Vec<Occurrence> = members
                .iter()
                .filter(|o| !continued.contains(o))
                .copied()
                .collect();
            let (censored, resolved): (Vec<Occurrence>, Vec<Occurrence>) =
                tips.iter().partition(|o| !out.contains_key(&o.0));
            for tip in &resolved {
                assert_eq!(case.disappears[tip].len(), out[&tip.0].len());
                ends.extend(case.disappears[tip].iter().copied());
            }
            censored_tips.extend(censored.iter().copied());
            let mut genealogy = Genealogy {
                root_count: births.len() as u32,
                birth: None,
                censored: !censored.is_empty(),
                effective_days: None,
                dag_transitions: None,
                dag_transitions_with_skipped: None,
            };
            if let [birth] = births[..] {
                let kind = case.births[&birth];
                genealogy.birth = Some((birth.0, kind));
                if kind == BirthKind::Appear {
                    let (end, transitions, with_skipped) = if censored.is_empty() {
                        (
                            resolved
                                .iter()
                                .flat_map(|t| out[&t.0].iter().map(|(c, _)| effective[c]))
                                .max()
                                .unwrap(),
                            resolved.iter().map(|t| depth[t].0 + 1).max().unwrap(),
                            resolved
                                .iter()
                                .map(|t| {
                                    depth[t].1 + out[&t.0].iter().map(|(_, l)| *l).max().unwrap()
                                })
                                .max()
                                .unwrap(),
                        )
                    } else {
                        (
                            censored.iter().map(|t| effective[&t.0]).max().unwrap(),
                            censored.iter().map(|t| depth[t].0).max().unwrap(),
                            censored.iter().map(|t| depth[t].1).max().unwrap(),
                        )
                    };
                    genealogy.effective_days = Some((end - effective[&birth.0]) as f64 / 86_400.0);
                    genealogy.dag_transitions = Some(transitions);
                    genealogy.dag_transitions_with_skipped = Some(with_skipped);
                }
            }
            // 番号の順は，出生の版の実効の時刻，版の番号，ペアの番号の一番小さい組で決める
            let key = births
                .iter()
                .map(|b| (effective[&b.0], b.0, b.1))
                .min()
                .unwrap();
            result.push((
                key,
                genealogy,
                members.iter().copied().collect::<BTreeSet<_>>(),
            ));
        }
        result.sort_by_key(|(key, _, _)| *key);
        (
            result
                .into_iter()
                .map(|(_, g, members)| (g, members))
                .collect(),
            ends,
            censored_tips,
        )
    }

    #[test]
    fn random_records_agree_with_the_naive_genealogies() {
        let (mut genealogies, mut merged, mut resolved, mut partial) = (0, 0, 0, 0);
        for seed in 0..3000 {
            let mut case = random_case(seed);
            let (expected, ends, censored_tips) = naive(&case);
            let memory = std::mem::take(&mut case.memory);
            let mut memory = memory;
            let mut walked = walk(
                &case.versions,
                &case.edges,
                &mut memory,
                &Interrupt::default(),
            )
            .unwrap_or_else(|err| panic!("seed {seed}: {err}"));
            let effective = case.versions.iter().map(|v| (v.id, v.effective)).collect();
            let topo = case.versions.iter().map(|v| (v.id, v.id)).collect();
            let summary = summarize(&mut walked, &effective, &topo, |_, mut births| {
                births.sort();
                Ok(births)
            })
            .unwrap();

            let got: Vec<&Genealogy> = summary.genealogies.iter().collect();
            let want: Vec<&Genealogy> = expected.iter().map(|(g, _)| g).collect();
            assert_eq!(got, want, "seed {seed}");
            let of: BTreeMap<Occurrence, u32> = expected
                .iter()
                .enumerate()
                .flat_map(|(i, (_, members))| members.iter().map(move |o| (*o, i as u32)))
                .collect();
            for (node, birth) in walked.births.iter().enumerate() {
                assert_eq!(
                    summary.of_node[node],
                    of[&(birth.version, birth.pair)],
                    "seed {seed}"
                );
            }
            assert_eq!(walked.events.len(), case.events.len(), "seed {seed}");
            for (event, (id, at, kind)) in walked.events.iter().zip(&case.events) {
                let role = match kind {
                    Kind::Appear => Some(Role::Birth),
                    Kind::Disappear if ends.contains(id) => Some(Role::End),
                    _ => None,
                };
                assert_eq!((event.event, event.role), (*id, role), "seed {seed}");
                assert_eq!(
                    summary.of_node[event.node as usize], of[at],
                    "seed {seed} event {id}"
                );
            }
            let tips: BTreeSet<Occurrence> = walked
                .censored
                .iter()
                .map(|t| (t.version, t.pair))
                .collect();
            assert_eq!(tips, censored_tips, "seed {seed}");
            for tip in &walked.censored {
                assert_eq!(
                    summary.of_node[tip.node as usize],
                    of[&(tip.version, tip.pair)],
                    "seed {seed}"
                );
            }

            genealogies += expected.len();
            merged += expected.iter().filter(|(g, _)| g.root_count > 1).count();
            resolved += expected
                .iter()
                .filter(|(g, _)| g.effective_days.is_some() && !g.censored)
                .count();
            partial += case
                .events
                .iter()
                .filter(|(id, _, kind)| *kind == Kind::Disappear && !ends.contains(id))
                .count();
        }
        // 無作為な記録が，調べたい場合を踏んでいることを確かめる
        assert!(genealogies > 10_000, "{genealogies}");
        assert!(
            merged > 300 && resolved > 300 && partial > 300,
            "{merged} {resolved} {partial}"
        );
    }
}
