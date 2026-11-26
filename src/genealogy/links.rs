// 出現を全部は作らない．書かなかった Persist の出現は，親の版の出現と同じ値のまま子の版へ持ち越す．
// 版が数万でペアが 2 万近い記録では，出現が数億になるからである．

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::process::Interrupt;

pub type VersionId = i64;
pub type PairId = i64;
pub type Node = u32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Root {
    EmptyTree,
    Censored,
}

pub struct Version {
    pub id: VersionId,
    pub ok: bool,
    pub root: Option<Root>,
    pub effective: i64,
}

pub struct Edge {
    pub id: i64,
    pub parent: Option<VersionId>,
    pub child: VersionId,
    /// 1 と飛ばした版の数を足した長さ
    pub length: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Appear,
    Persist,
    Merge,
    Disappear,
}

pub struct Event {
    pub id: i64,
    pub kind: Kind,
    pub parent: Option<PairId>,
    pub child: Option<PairId>,
}

/// 版ごとと edge ごとに読ませるのは，記録の全部のイベントをメモリに持たないためである
pub trait Source {
    fn root_pairs(&mut self, version: VersionId) -> Result<Vec<PairId>>;
    fn events(&mut self, edge: i64) -> Result<Vec<Event>>;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BirthKind {
    Appear,
    LeftCensored,
}

/// 出生．番号が，その出生から始まる union-find の節の番号になる
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Birth {
    pub version: VersionId,
    pub pair: PairId,
    pub kind: BirthKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Birth,
    End,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EventNode {
    pub event: i64,
    pub node: Node,
    pub role: Option<Role>,
}

/// 出ていく edge の全部で Disappear した末端．深さには，解消の Disappear の edge も数える
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ResolvedTip {
    pub node: Node,
    pub transitions: i64,
    pub transitions_with_skipped: i64,
    pub end_effective: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CensoredTip {
    pub node: Node,
    pub version: VersionId,
    pub pair: PairId,
    pub depth: i64,
    pub depth_with_skipped: i64,
    pub effective: i64,
}

pub struct Walk {
    pub births: Vec<Birth>,
    parents: Vec<Node>,
    pub events: Vec<EventNode>,
    pub resolved: Vec<ResolvedTip>,
    pub censored: Vec<CensoredTip>,
}

impl Walk {
    fn new_birth(&mut self, birth: Birth) -> Node {
        let node = Node::try_from(self.births.len()).expect("fewer than 2^32 births");
        self.births.push(birth);
        self.parents.push(node);
        node
    }

    pub fn find(&mut self, node: Node) -> Node {
        let mut root = node;
        while self.parents[root as usize] != root {
            root = self.parents[root as usize];
        }
        let mut at = node;
        while self.parents[at as usize] != root {
            let next = self.parents[at as usize];
            self.parents[at as usize] = root;
            at = next;
        }
        root
    }

    /// 小さい番号を根にするのは，同じ入力からいつも同じ根になるようにするためである
    fn union(&mut self, a: Node, b: Node) -> Node {
        let (ra, rb) = (self.find(a), self.find(b));
        let (root, other) = if ra <= rb { (ra, rb) } else { (rb, ra) };
        self.parents[other as usize] = root;
        root
    }
}

/// 深さは，版の深さとの差で持つ．書かなかった Persist の出現は差が変わらないので，
/// edge ごとに全部のペアを書き直さずに済む．書いた Persist も差が変わらず，変わるのは出生と Merge だけである
#[derive(Clone, Copy, Debug)]
struct Occurrence {
    node: Node,
    offset: i64,
    offset_with_skipped: i64,
}

/// 版の状態．出ていく edge を全部処理するまで持つ
struct Held {
    pairs: HashMap<PairId, Occurrence>,
    depth: i64,
    depth_with_skipped: i64,
    remaining: usize,
    /// まだ全部の edge で Disappear したとは分からない出現の，Disappear の events の位置
    disappeared: HashMap<PairId, Vec<usize>>,
}

#[derive(Default)]
struct Out {
    degree: usize,
    longest: i64,
    latest_child: i64,
}

fn broken(what: String) -> anyhow::Error {
    anyhow::anyhow!("the record is broken: {what}")
}

/// versions はグラフの順に並べる
pub fn walk(
    versions: &[Version],
    edges: &[Edge],
    source: &mut impl Source,
    interrupt: &Interrupt,
) -> Result<Walk> {
    let effective: HashMap<VersionId, i64> = versions.iter().map(|v| (v.id, v.effective)).collect();
    let mut incoming: HashMap<VersionId, &Edge> = HashMap::new();
    let mut out: HashMap<VersionId, Out> = HashMap::new();
    for edge in edges {
        if incoming.insert(edge.child, edge).is_some() {
            bail!(broken(format!(
                "version {} has two incoming edges",
                edge.child
            )));
        }
        if let Some(parent) = edge.parent {
            let child_time = *effective
                .get(&edge.child)
                .ok_or_else(|| broken(format!("edge {} has an unknown child", edge.id)))?;
            let o = out.entry(parent).or_default();
            o.degree += 1;
            o.longest = o.longest.max(edge.length);
            o.latest_child = o.latest_child.max(child_time);
        }
    }

    let mut walk = Walk {
        births: Vec::new(),
        parents: Vec::new(),
        events: Vec::new(),
        resolved: Vec::new(),
        censored: Vec::new(),
    };
    let mut held: HashMap<VersionId, Held> = HashMap::new();
    for version in versions.iter().filter(|v| v.ok) {
        interrupt.check()?;
        let v = version.id;
        let (pairs, depth, depth_with_skipped) = match (incoming.get(&v), version.root) {
            (None, Some(Root::Censored)) => {
                let mut pairs = HashMap::new();
                for pair in source.root_pairs(v)? {
                    let node = walk.new_birth(Birth {
                        version: v,
                        pair,
                        kind: BirthKind::LeftCensored,
                    });
                    pairs.insert(
                        pair,
                        Occurrence {
                            node,
                            offset: 0,
                            offset_with_skipped: 0,
                        },
                    );
                }
                (pairs, 0, 0)
            }
            (Some(edge), Some(Root::EmptyTree)) if edge.parent.is_none() => {
                let mut pairs = HashMap::new();
                apply(&mut walk, source, edge, v, 0, 0, &mut pairs, None)?;
                (pairs, 0, 0)
            }
            (Some(edge), None) if edge.parent.is_some() => {
                let parent = edge.parent.expect("checked above");
                let Some(state) = held.get_mut(&parent) else {
                    bail!(broken(format!(
                        "version {v} comes before its parent {parent}, or the parent is broken"
                    )));
                };
                let depth = state.depth + 1;
                let depth_with_skipped = state.depth_with_skipped + edge.length;
                // 最後の子には写さずに渡すのは，一列のグラフで版ごとに状態を写さないためである
                let mut pairs = if state.remaining == 1 {
                    std::mem::take(&mut state.pairs)
                } else {
                    state.pairs.clone()
                };
                let o = &out[&parent];
                let tip = TipContext {
                    degree: o.degree,
                    longest: o.longest,
                    latest_child: o.latest_child,
                };
                apply(
                    &mut walk,
                    source,
                    edge,
                    v,
                    depth,
                    depth_with_skipped,
                    &mut pairs,
                    Some((state, tip)),
                )?;
                state.remaining -= 1;
                if state.remaining == 0 {
                    held.remove(&parent);
                }
                (pairs, depth, depth_with_skipped)
            }
            (edge, root) => bail!(broken(format!(
                "version {v} has root {root:?} and {}",
                match edge {
                    None => "no incoming edge".to_string(),
                    Some(e) if e.parent.is_none() => "an edge from the empty tree".to_string(),
                    Some(_) => "an edge from a version".to_string(),
                }
            ))),
        };

        let degree = out.get(&v).map_or(0, |o| o.degree);
        if degree == 0 {
            let mut tips: Vec<(&PairId, &Occurrence)> = pairs.iter().collect();
            tips.sort_by_key(|(pair, _)| **pair);
            for (&pair, occurrence) in tips {
                walk.censored.push(CensoredTip {
                    node: occurrence.node,
                    version: v,
                    pair,
                    depth: depth + occurrence.offset,
                    depth_with_skipped: depth_with_skipped + occurrence.offset_with_skipped,
                    effective: version.effective,
                });
            }
        } else {
            held.insert(
                v,
                Held {
                    pairs,
                    depth,
                    depth_with_skipped,
                    remaining: degree,
                    disappeared: HashMap::new(),
                },
            );
        }
    }
    if let Some(v) = held.keys().min() {
        bail!(broken(format!(
            "version {v} has edges to children that are not ok versions"
        )));
    }
    Ok(walk)
}

#[derive(Clone, Copy)]
struct TipContext {
    degree: usize,
    longest: i64,
    latest_child: i64,
}

/// 親のペアを先に全部引いて外すのは，同じ edge の中で外したペアと足したペアが同じ番号のことがあるからである．store/reconstruct.rs と同じ
#[allow(clippy::too_many_arguments)]
fn apply(
    walk: &mut Walk,
    source: &mut impl Source,
    edge: &Edge,
    child: VersionId,
    depth: i64,
    depth_with_skipped: i64,
    pairs: &mut HashMap<PairId, Occurrence>,
    mut parent: Option<(&mut Held, TipContext)>,
) -> Result<()> {
    let events = source.events(edge.id)?;
    let mut found = Vec::with_capacity(events.len());
    for event in &events {
        found.push(match event.parent {
            Some(pair) => Some(*pairs.get(&pair).ok_or_else(|| {
                broken(format!(
                    "event {} has parent pair {pair}, which the parent version does not have",
                    event.id
                ))
            })?),
            None => None,
        });
    }
    for event in &events {
        if let Some(pair) = event.parent {
            pairs.remove(&pair);
        }
    }
    // 届いた数も持つのは，record が親のペアが 1 つなら Persist，2 つ以上なら Merge を書く決まりを確かめるためである
    let mut added: HashMap<PairId, (Occurrence, usize)> = HashMap::new();
    let (mut persisted, mut merged) = (Vec::new(), Vec::new());
    for (event, occurrence) in events.iter().zip(found) {
        match (event.kind, event.child, occurrence) {
            (Kind::Appear, Some(pair), None) => {
                let node = walk.new_birth(Birth {
                    version: child,
                    pair,
                    kind: BirthKind::Appear,
                });
                walk.events.push(EventNode {
                    event: event.id,
                    node,
                    role: Some(Role::Birth),
                });
                let occurrence = Occurrence {
                    node,
                    offset: -depth,
                    offset_with_skipped: -depth_with_skipped,
                };
                if added.insert(pair, (occurrence, 0)).is_some() {
                    bail!(broken(format!(
                        "pair {pair} appears twice on edge {}",
                        edge.id
                    )));
                }
            }
            (Kind::Persist | Kind::Merge, Some(pair), Some(from)) => {
                walk.events.push(EventNode {
                    event: event.id,
                    node: from.node,
                    role: None,
                });
                let next = match (event.kind, added.get(&pair)) {
                    (_, None) => (from, 1),
                    (Kind::Merge, Some(&(before, reached))) if reached > 0 => (
                        Occurrence {
                            node: walk.union(before.node, from.node),
                            offset: before.offset.max(from.offset),
                            offset_with_skipped: before
                                .offset_with_skipped
                                .max(from.offset_with_skipped),
                        },
                        reached + 1,
                    ),
                    _ => bail!(broken(format!(
                        "pair {pair} is reached twice on edge {} by events that are not all Merge",
                        edge.id
                    ))),
                };
                added.insert(pair, (next.0, next.1));
                if event.kind == Kind::Persist {
                    persisted.push(pair);
                } else {
                    merged.push(pair);
                }
            }
            (Kind::Disappear, None, Some(from)) => {
                let index = walk.events.len();
                walk.events.push(EventNode {
                    event: event.id,
                    node: from.node,
                    role: None,
                });
                let Some((state, tip)) = parent.as_mut() else {
                    bail!(broken(format!(
                        "edge {} from the empty tree has a Disappear",
                        edge.id
                    )));
                };
                let pair = event.parent.expect("a Disappear has a parent pair");
                let seen = state.disappeared.entry(pair).or_default();
                seen.push(index);
                if seen.len() == tip.degree {
                    for &i in seen.iter() {
                        walk.events[i].role = Some(Role::End);
                    }
                    state.disappeared.remove(&pair);
                    walk.resolved.push(ResolvedTip {
                        node: from.node,
                        transitions: state.depth + from.offset + 1,
                        transitions_with_skipped: state.depth_with_skipped
                            + from.offset_with_skipped
                            + tip.longest,
                        end_effective: tip.latest_child,
                    });
                }
            }
            _ => bail!(broken(format!(
                "event {} does not fit its kind {:?}",
                event.id, event.kind
            ))),
        }
    }
    if let Some(pair) = persisted
        .iter()
        .find(|pair| added[*pair].1 != 1)
        .or_else(|| merged.iter().find(|pair| added[*pair].1 < 2))
    {
        bail!(broken(format!(
            "pair {pair} on edge {} is reached by a Persist with other events, or by one Merge",
            edge.id
        )));
    }
    for (pair, (occurrence, _)) in added {
        if pairs.insert(pair, occurrence).is_some() {
            bail!(broken(format!(
                "pair {pair} on edge {} is both written and carried over unchanged",
                edge.id
            )));
        }
    }
    Ok(())
}

/// links，tips，writer の単体のテストが，記録の代わりに使う
#[cfg(test)]
pub mod memory {
    use super::*;

    #[derive(Default)]
    pub struct Memory {
        pub roots: HashMap<VersionId, Vec<PairId>>,
        events: HashMap<i64, Vec<Event>>,
        next: i64,
    }

    impl Memory {
        /// イベントの番号は，足した順に 1 から振る
        pub fn edge(&mut self, edge: i64, events: &[(Kind, Option<PairId>, Option<PairId>)]) {
            for &(kind, parent, child) in events {
                self.next += 1;
                self.events.entry(edge).or_default().push(Event {
                    id: self.next,
                    kind,
                    parent,
                    child,
                });
            }
        }
    }

    impl Source for Memory {
        fn root_pairs(&mut self, version: VersionId) -> Result<Vec<PairId>> {
            Ok(self.roots.get(&version).cloned().unwrap_or_default())
        }
        fn events(&mut self, edge: i64) -> Result<Vec<Event>> {
            Ok(self.events.remove(&edge).unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::memory::Memory;
    use super::*;

    fn version(id: VersionId, root: Option<Root>, effective: i64) -> Version {
        Version {
            id,
            ok: true,
            root,
            effective,
        }
    }

    fn edge(id: i64, parent: Option<VersionId>, child: VersionId, length: i64) -> Edge {
        Edge {
            id,
            parent,
            child,
            length,
        }
    }

    use Kind::*;

    fn run(versions: &[Version], edges: &[Edge], memory: &mut Memory) -> Walk {
        walk(versions, edges, memory, &Interrupt::default()).unwrap()
    }

    #[test]
    fn implicit_persists_carry_the_depth_to_the_censored_tip() {
        // 1 は空のツリーから 10 が現れ，2 と 3 は書かないイベントだけで続く
        let versions = [
            version(1, Some(Root::EmptyTree), 0),
            version(2, None, 10),
            version(3, None, 20),
        ];
        let edges = [
            edge(1, None, 1, 1),
            edge(2, Some(1), 2, 1),
            edge(3, Some(2), 3, 3),
        ];
        let mut memory = Memory::default();
        memory.edge(1, &[(Appear, None, Some(10))]);
        let mut walk = run(&versions, &edges, &mut memory);
        assert_eq!(
            walk.births,
            [Birth {
                version: 1,
                pair: 10,
                kind: BirthKind::Appear
            }]
        );
        assert_eq!(
            walk.censored,
            [CensoredTip {
                node: 0,
                version: 3,
                pair: 10,
                depth: 2,
                depth_with_skipped: 4,
                effective: 20
            }]
        );
        assert!(walk.resolved.is_empty());
        assert_eq!(walk.find(0), 0);
    }

    #[test]
    fn a_pair_that_disappears_on_every_edge_is_a_resolved_tip() {
        // 1 の 10 は，2 への edge で Disappear し，3 への edge で続く．11 は両方で Disappear する
        let versions = [
            version(1, Some(Root::Censored), 0),
            version(2, None, 5),
            version(3, None, 7),
        ];
        let edges = [edge(1, Some(1), 2, 2), edge(2, Some(1), 3, 1)];
        let mut memory = Memory::default();
        memory.roots.insert(1, vec![10, 11]);
        memory.edge(
            1,
            &[(Disappear, Some(10), None), (Disappear, Some(11), None)],
        );
        memory.edge(2, &[(Disappear, Some(11), None)]);
        let walk = run(&versions, &edges, &mut memory);
        assert_eq!(walk.births.len(), 2);
        assert_eq!(
            walk.resolved,
            [ResolvedTip {
                node: 1,
                transitions: 1,
                transitions_with_skipped: 2,
                end_effective: 7
            }]
        );
        let roles: Vec<_> = walk.events.iter().map(|e| (e.event, e.role)).collect();
        assert_eq!(
            roles,
            [(1, None), (2, Some(Role::End)), (3, Some(Role::End))]
        );
        // 10 は 3 の版まで続いて打ち切りになる
        assert_eq!(walk.censored.len(), 1);
        assert_eq!(walk.censored[0].pair, 10);
        assert_eq!(walk.censored[0].version, 3);
    }

    #[test]
    fn a_merge_joins_the_parents_and_keeps_the_deeper_one() {
        // 10 は 1 の起点から，20 は 2 で現れる．3 で 2 つが 30 に Merge する
        let versions = [
            version(1, Some(Root::Censored), 0),
            version(2, None, 0),
            version(3, None, 0),
        ];
        let edges = [edge(1, Some(1), 2, 1), edge(2, Some(2), 3, 1)];
        let mut memory = Memory::default();
        memory.roots.insert(1, vec![10]);
        memory.edge(1, &[(Appear, None, Some(20))]);
        memory.edge(
            2,
            &[(Merge, Some(10), Some(30)), (Merge, Some(20), Some(30))],
        );
        let mut walk = run(&versions, &edges, &mut memory);
        assert_eq!(walk.find(1), walk.find(0));
        assert_eq!(walk.censored.len(), 1);
        // 起点の 10 から数えた深さ 2 が，2 で現れた 20 から数えた 1 より深い
        assert_eq!(walk.censored[0].depth, 2);
        let nodes: Vec<_> = walk.events.iter().map(|e| e.node).collect();
        assert_eq!(nodes, [1, 0, 1]);
    }

    #[test]
    fn a_written_persist_to_the_same_pair_number_is_replaced() {
        let versions = [version(1, Some(Root::Censored), 0), version(2, None, 0)];
        let edges = [edge(1, Some(1), 2, 1)];
        let mut memory = Memory::default();
        memory.roots.insert(1, vec![10]);
        memory.edge(1, &[(Persist, Some(10), Some(10))]);
        let walk = run(&versions, &edges, &mut memory);
        assert_eq!(walk.censored.len(), 1);
        assert_eq!(walk.censored[0].depth, 1);
    }

    #[test]
    fn broken_records_are_refused() {
        let versions = [version(1, Some(Root::Censored), 0), version(2, None, 0)];
        let edges = [edge(1, Some(1), 2, 1)];
        for events in [
            vec![(Persist, Some(99), Some(10))],
            vec![
                (Disappear, Some(10), None),
                (Appear, None, Some(11)),
                (Appear, None, Some(11)),
            ],
            vec![(Persist, Some(10), Some(12)), (Appear, None, Some(10))],
            // record は，親のペアが 1 つなら Persist，2 つ以上なら Merge だけを書く
            vec![(Merge, Some(10), Some(13))],
            vec![(Persist, Some(10), Some(13)), (Merge, Some(12), Some(13))],
            vec![(Merge, Some(10), Some(13)), (Persist, Some(12), Some(13))],
            vec![(Persist, Some(10), Some(13)), (Persist, Some(12), Some(13))],
        ] {
            let mut memory = Memory::default();
            memory.roots.insert(1, vec![10, 12]);
            memory.edge(1, &events);
            let err = walk(&versions, &edges, &mut memory, &Interrupt::default())
                .err()
                .unwrap();
            assert!(err.to_string().starts_with("the record is broken"), "{err}");
        }
        // 起点でない版に root がある
        let versions = [
            version(1, Some(Root::Censored), 0),
            version(2, Some(Root::Censored), 0),
        ];
        let mut memory = Memory::default();
        assert!(walk(&versions, &edges, &mut memory, &Interrupt::default()).is_err());
    }

    #[test]
    fn broken_versions_are_passed_over() {
        // 2 は壊れた版で，1 から 3 への edge が飛ばした版を 1 つ数える
        let mut versions = [
            version(1, Some(Root::Censored), 0),
            version(2, None, 0),
            version(3, None, 9),
        ];
        versions[1].ok = false;
        let edges = [edge(1, Some(1), 3, 2)];
        let mut memory = Memory::default();
        memory.roots.insert(1, vec![10]);
        let walk = run(&versions, &edges, &mut memory);
        assert_eq!(walk.censored[0].depth, 1);
        assert_eq!(walk.censored[0].depth_with_skipped, 2);
    }
}
