use std::collections::HashMap;

use anyhow::Result;

use crate::genealogy::links::{BirthKind, Node, PairId, VersionId, Walk};
use crate::genealogy::time::days;

#[derive(Clone, PartialEq, Debug)]
pub struct Genealogy {
    pub root_count: u32,
    pub birth: Option<(VersionId, BirthKind)>,
    pub censored: bool,
    pub effective_days: Option<f64>,
    pub dag_transitions: Option<i64>,
    pub dag_transitions_with_skipped: Option<i64>,
}

pub struct Summary {
    /// 番号の順．番号は位置に 1 を足した値
    pub genealogies: Vec<Genealogy>,
    /// 節ごとの系譜の位置
    pub of_node: Vec<u32>,
}

#[derive(Default, Clone, Copy)]
struct Ends {
    censored: bool,
    resolved: Option<(i64, i64, i64)>,
    censored_max: Option<(i64, i64, i64)>,
}

fn max3(a: Option<(i64, i64, i64)>, b: (i64, i64, i64)) -> Option<(i64, i64, i64)> {
    Some(match a {
        None => b,
        Some(a) => (a.0.max(b.0), a.1.max(b.1), a.2.max(b.2)),
    })
}

/// triples は，1 つの版の出生のペアを 3 つ組の順に並べ替えて返す．
/// 版ごとに引くのは，3 つ組を比べるのが同じ版の出生どうしだけで，全部の出生の 3 つ組を一度に持たないためである
pub fn summarize(
    walk: &mut Walk,
    effective: &HashMap<VersionId, i64>,
    topo: &HashMap<VersionId, i64>,
    mut triples: impl FnMut(VersionId, Vec<(PairId, Node)>) -> Result<Vec<(PairId, Node)>>,
) -> Result<Summary> {
    let nodes = walk.births.len();
    let component: Vec<Node> = (0..nodes as Node).map(|n| walk.find(n)).collect();

    let mut by_version: Vec<(i64, i64, VersionId)> = Vec::new();
    let mut births_of: HashMap<VersionId, Vec<(PairId, Node)>> = HashMap::new();
    for (node, birth) in walk.births.iter().enumerate() {
        let list = births_of.entry(birth.version).or_default();
        if list.is_empty() {
            by_version.push((
                effective[&birth.version],
                topo[&birth.version],
                birth.version,
            ));
        }
        list.push((birth.pair, node as Node));
    }
    by_version.sort_unstable();
    let mut index = vec![u32::MAX; nodes];
    let mut count = 0u32;
    for (_, _, version) in by_version {
        let births = births_of.remove(&version).expect("listed above");
        for (_, node) in triples(version, births)? {
            let root = component[node as usize] as usize;
            if index[root] == u32::MAX {
                index[root] = count;
                count += 1;
            }
        }
    }
    let of_node: Vec<u32> = component.iter().map(|&root| index[root as usize]).collect();

    let mut genealogies = vec![
        Genealogy {
            root_count: 0,
            birth: None,
            censored: false,
            effective_days: None,
            dag_transitions: None,
            dag_transitions_with_skipped: None,
        };
        count as usize
    ];
    for (node, birth) in walk.births.iter().enumerate() {
        let g = of_node[node] as usize;
        genealogies[g].root_count += 1;
        genealogies[g].birth = Some((birth.version, birth.kind));
    }
    let mut ends = vec![Ends::default(); count as usize];
    for tip in &walk.resolved {
        let e = &mut ends[of_node[tip.node as usize] as usize];
        e.resolved = max3(
            e.resolved,
            (
                tip.transitions,
                tip.transitions_with_skipped,
                tip.end_effective,
            ),
        );
    }
    for tip in &walk.censored {
        let e = &mut ends[of_node[tip.node as usize] as usize];
        e.censored = true;
        e.censored_max = max3(
            e.censored_max,
            (tip.depth, tip.depth_with_skipped, tip.effective),
        );
    }

    for (g, genealogy) in genealogies.iter_mut().enumerate() {
        let e = ends[g];
        genealogy.censored = e.censored;
        if genealogy.root_count > 1 {
            genealogy.birth = None;
            continue;
        }
        let Some((version, BirthKind::Appear)) = genealogy.birth else {
            continue;
        };
        // 解消した末端を混ぜないのは，打ち切りの系譜の時間を，打ち切りの末端だけで数えるからである
        let (transitions, with_skipped, end) = if e.censored {
            e.censored_max
        } else {
            e.resolved
        }
        .expect("a genealogy has a tip");
        genealogy.effective_days = Some(days(effective[&version], end));
        genealogy.dag_transitions = Some(transitions);
        genealogy.dag_transitions_with_skipped = Some(with_skipped);
    }
    Ok(Summary {
        genealogies,
        of_node,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genealogy::links::memory::Memory;
    use crate::genealogy::links::{Edge, Kind, Root, Version, walk as walk_versions};
    use crate::process::Interrupt;

    fn summary(
        versions: &[(VersionId, Option<Root>, i64)],
        edges: &[(i64, Option<VersionId>, VersionId, i64)],
        memory: &mut Memory,
    ) -> (Walk, Summary) {
        let list: Vec<Version> = versions
            .iter()
            .map(|&(id, root, effective)| Version {
                id,
                ok: true,
                root,
                effective,
            })
            .collect();
        let edges: Vec<Edge> = edges
            .iter()
            .map(|&(id, parent, child, length)| Edge {
                id,
                parent,
                child,
                length,
            })
            .collect();
        let mut walk = walk_versions(&list, &edges, memory, &Interrupt::default()).unwrap();
        let effective = versions.iter().map(|v| (v.0, v.2)).collect();
        let topo = versions
            .iter()
            .enumerate()
            .map(|(i, v)| (v.0, i as i64))
            .collect();
        // ペアの番号を 3 つ組の順の代わりにする
        let summary = summarize(&mut walk, &effective, &topo, |_, mut births| {
            births.sort();
            Ok(births)
        })
        .unwrap();
        (walk, summary)
    }

    use Kind::*;

    #[test]
    fn a_resolved_genealogy_counts_days_and_transitions_to_the_disappear() {
        // 1 で 10 が現れ，2 で書かない Persist，3 で Disappear する．2 から 3 は飛ばした版が 1 つある
        let mut memory = Memory::default();
        memory.edge(1, &[(Appear, None, Some(10))]);
        memory.edge(3, &[(Disappear, Some(10), None)]);
        let (_, s) = summary(
            &[
                (1, Some(Root::EmptyTree), 0),
                (2, None, 86_400),
                (3, None, 3 * 86_400),
            ],
            &[(1, None, 1, 1), (2, Some(1), 2, 1), (3, Some(2), 3, 2)],
            &mut memory,
        );
        assert_eq!(
            s.genealogies,
            [Genealogy {
                root_count: 1,
                birth: Some((1, BirthKind::Appear)),
                censored: false,
                effective_days: Some(3.0),
                dag_transitions: Some(2),
                dag_transitions_with_skipped: Some(3),
            }]
        );
    }

    #[test]
    fn a_censored_genealogy_counts_only_the_censored_tips() {
        // 1 の 10 は，2 では Disappear し，3 では続いて終わる
        let mut memory = Memory::default();
        memory.edge(1, &[(Appear, None, Some(10))]);
        memory.edge(2, &[(Disappear, Some(10), None)]);
        let (_, s) = summary(
            &[
                (1, Some(Root::EmptyTree), 0),
                (2, None, 5 * 86_400),
                (3, None, 86_400),
            ],
            &[(1, None, 1, 1), (2, Some(1), 2, 1), (3, Some(1), 3, 4)],
            &mut memory,
        );
        let g = &s.genealogies[0];
        assert!(g.censored);
        assert_eq!(g.effective_days, Some(1.0));
        assert_eq!(g.dag_transitions, Some(1));
        assert_eq!(g.dag_transitions_with_skipped, Some(4));
    }

    #[test]
    fn merged_births_make_one_genealogy_without_times() {
        let mut memory = Memory::default();
        memory.edge(1, &[(Appear, None, Some(10)), (Appear, None, Some(11))]);
        memory.edge(
            2,
            &[(Merge, Some(10), Some(12)), (Merge, Some(11), Some(12))],
        );
        let (_, s) = summary(
            &[(1, Some(Root::EmptyTree), 0), (2, None, 0)],
            &[(1, None, 1, 1), (2, Some(1), 2, 1)],
            &mut memory,
        );
        assert_eq!(s.genealogies.len(), 1);
        let g = &s.genealogies[0];
        assert_eq!(g.root_count, 2);
        assert_eq!(g.birth, None);
        assert!(g.censored);
        assert_eq!(g.effective_days, None);
    }

    #[test]
    fn left_censored_genealogies_have_no_times() {
        let mut memory = Memory::default();
        memory.roots.insert(1, vec![10]);
        let (_, s) = summary(&[(1, Some(Root::Censored), 0)], &[], &mut memory);
        assert_eq!(s.genealogies[0].birth, Some((1, BirthKind::LeftCensored)));
        assert_eq!(s.genealogies[0].effective_days, None);
        assert_eq!(s.genealogies[0].dag_transitions, None);
    }

    #[test]
    fn genealogies_are_numbered_by_the_earliest_birth() {
        // 2 の版は実効の時刻が 1 より早い．同じ版の出生はペアの 3 つ組の順になる
        let mut memory = Memory::default();
        memory.roots.insert(1, vec![30]);
        memory.roots.insert(2, vec![21, 20]);
        let (_, s) = summary(
            &[(1, Some(Root::Censored), 50), (2, Some(Root::Censored), 10)],
            &[],
            &mut memory,
        );
        // 節は出生の順で，1 の 30 が 0，2 の 21 が 1，20 が 2 である
        assert_eq!(s.of_node, [2, 1, 0]);
    }
}
