use std::collections::BTreeMap;

use crate::graph::{Graph, VersionIdx};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Edge {
    pub parent: VersionIdx,
    pub skipped: u32,
}

/// 祖先ごとの，道の上の壊れた版の集合．集合を番号の順に並べた Vec で持つのは，
/// 足したときの順が決まり，同じ入力から同じ結果になるからである．
type Reach = BTreeMap<VersionIdx, Vec<VersionIdx>>;

/// edge を前もって全部作らないのは，版が壊れたかがプラグインを動かすまで分からないからである．
pub struct EdgeBuilder<'g> {
    graph: &'g Graph,
    broken: Vec<Option<bool>>,
    reach: Vec<Option<Reach>>,
    unprocessed_children: Vec<usize>,
    holders: Vec<usize>,
}

impl<'g> EdgeBuilder<'g> {
    pub fn new(graph: &'g Graph) -> EdgeBuilder<'g> {
        let n = graph.versions.len();
        EdgeBuilder {
            graph,
            broken: vec![None; n],
            reach: vec![None; n],
            unprocessed_children: graph.versions.iter().map(|v| v.children.len()).collect(),
            holders: vec![0; n],
        }
    }

    /// 祖先を鍵にするのは，同じ祖先からの直接の edge と壊れた版を通る道を 1 本にまとめるためである．
    pub fn incoming(&self, v: VersionIdx) -> Vec<Edge> {
        let mut edges: Vec<Edge> = self
            .reach_of(v)
            .into_iter()
            .map(|(parent, skipped)| Edge {
                parent,
                skipped: skipped.len() as u32,
            })
            .collect();
        edges.sort_by_key(|edge| self.graph.position[edge.parent.index()]);
        edges
    }

    /// 増やす分を先に足してから減らす分を引くのは，途中で 0 になって早く捨てないためである．
    pub fn finish(&mut self, v: VersionIdx, broken: bool) -> Vec<VersionIdx> {
        assert!(
            self.broken[v.index()].is_none(),
            "version {v:?} is finished twice"
        );
        let has_children = self.unprocessed_children[v.index()] > 0;
        let mut released = Vec::new();
        if broken {
            if has_children {
                let reach = self.reach_of(v);
                for ancestor in reach.keys() {
                    self.holders[ancestor.index()] += 1;
                }
                self.reach[v.index()] = Some(reach);
            }
        } else {
            self.holders[v.index()] = self.unprocessed_children[v.index()];
            if !has_children {
                released.push(v);
            }
        }
        self.broken[v.index()] = Some(broken);

        for parent in &self.graph.versions[v.index()].parents {
            let p = parent.index();
            self.unprocessed_children[p] -= 1;
            match self.broken[p] {
                Some(false) => self.release(*parent, &mut released),
                Some(true) => {
                    if self.unprocessed_children[p] == 0 {
                        let reach = self.reach[p]
                            .take()
                            .expect("a broken parent with children keeps its reach");
                        for ancestor in reach.keys() {
                            self.release(*ancestor, &mut released);
                        }
                    }
                }
                None => panic!("version {v:?} is finished before its parent {parent:?}"),
            }
        }
        released
    }

    pub fn held(&self) -> Vec<VersionIdx> {
        (0..self.graph.versions.len())
            .filter(|&i| self.broken[i] == Some(false) && self.holders[i] > 0)
            .map(|i| VersionIdx(i as u32))
            .collect()
    }

    fn release(&mut self, v: VersionIdx, released: &mut Vec<VersionIdx>) {
        self.holders[v.index()] -= 1;
        if self.holders[v.index()] == 0 {
            released.push(v);
        }
    }

    fn reach_of(&self, v: VersionIdx) -> Reach {
        let mut reach = Reach::new();
        for parent in &self.graph.versions[v.index()].parents {
            match self.broken[parent.index()] {
                Some(false) => {
                    reach.entry(*parent).or_default();
                }
                Some(true) => {
                    let through = self.reach[parent.index()]
                        .as_ref()
                        .expect("a broken parent with an unprocessed child keeps its reach");
                    for (ancestor, skipped) in through {
                        let set = reach.entry(*ancestor).or_default();
                        *set = union(set, skipped, *parent);
                    }
                }
                None => {
                    panic!("the edges of {v:?} are asked before its parent {parent:?} is finished")
                }
            }
        }
        reach
    }
}

fn union(a: &[VersionIdx], b: &[VersionIdx], extra: VersionIdx) -> Vec<VersionIdx> {
    let mut out = Vec::with_capacity(a.len() + b.len() + 1);
    let (mut i, mut j) = (0, 0);
    let mut extra = Some(extra);
    loop {
        let next = [a.get(i), b.get(j), extra.as_ref()]
            .into_iter()
            .flatten()
            .min()
            .copied();
        let Some(next) = next else {
            break;
        };
        if a.get(i) == Some(&next) {
            i += 1;
        }
        if b.get(j) == Some(&next) {
            j += 1;
        }
        if extra == Some(next) {
            extra = None;
        }
        out.push(next);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版の名前と親の名前の並びから作る．名前は 1 文字にする
    fn graph(lines: &[(char, &str)]) -> Graph {
        let index =
            |name: char| VersionIdx(lines.iter().position(|(v, _)| *v == name).unwrap() as u32);
        Graph::new(
            lines
                .iter()
                .map(|(v, parents)| (v.to_string(), parents.chars().map(index).collect()))
                .collect(),
        )
        .unwrap()
    }

    fn name(g: &Graph, v: VersionIdx) -> char {
        g.versions[v.index()].commit.chars().next().unwrap()
    }

    /// 版ごとに，入る edge と，書き終えて捨てる版と，その時点で持つ版を並べる．
    /// 捨てる版は，書き終えた版の後に，捨てられるようになった祖先の順に並ぶ
    fn walk(g: &Graph, broken: &str) -> Vec<String> {
        let mut builder = EdgeBuilder::new(g);
        let mut log = Vec::new();
        for &v in &g.order {
            let is_broken = broken.contains(name(g, v));
            let edges = if is_broken {
                "broken".to_string()
            } else {
                let edges: Vec<String> = builder
                    .incoming(v)
                    .iter()
                    .map(|e| format!("{}{}", name(g, e.parent), e.skipped))
                    .collect();
                if edges.is_empty() {
                    "origin".to_string()
                } else {
                    edges.join(",")
                }
            };
            let released: String = builder
                .finish(v, is_broken)
                .iter()
                .map(|r| name(g, *r))
                .collect();
            let held: String = builder.held().iter().map(|h| name(g, *h)).collect();
            log.push(format!(
                "{}:{edges} drop[{released}] hold[{held}]",
                name(g, v)
            ));
        }
        log
    }

    #[test]
    fn a_straight_line() {
        let g = graph(&[('a', ""), ('b', "a"), ('c', "b")]);
        assert_eq!(
            walk(&g, ""),
            [
                "a:origin drop[] hold[a]",
                "b:a0 drop[a] hold[b]",
                "c:b0 drop[cb] hold[]"
            ]
        );
    }

    #[test]
    fn broken_versions_in_a_row_are_skipped_and_the_ancestor_is_held() {
        let g = graph(&[('a', ""), ('x', "a"), ('y', "x"), ('c', "y")]);
        assert_eq!(
            walk(&g, "xy"),
            [
                "a:origin drop[] hold[a]",
                "x:broken drop[] hold[a]",
                "y:broken drop[] hold[a]",
                "c:a2 drop[ca] hold[]",
            ]
        );
    }

    #[test]
    fn broken_versions_in_a_diamond_count_each_once() {
        // a → x → y，a → x → z，y と z → c．x，y，z が壊れれば a → c で 3 つ飛ばす
        let g = graph(&[('a', ""), ('x', "a"), ('y', "x"), ('z', "x"), ('c', "yz")]);
        let log = walk(&g, "xyz");
        assert_eq!(log.last().unwrap(), "c:a3 drop[ca] hold[]");
    }

    #[test]
    fn a_direct_edge_and_a_path_through_a_broken_version_become_one() {
        // a → c と a → b → c で b が壊れれば，a → c の 1 本で 1 つ飛ばす
        let g = graph(&[('a', ""), ('b', "a"), ('c', "ab")]);
        assert_eq!(
            walk(&g, "b"),
            [
                "a:origin drop[] hold[a]",
                "b:broken drop[] hold[a]",
                "c:a1 drop[ca] hold[]"
            ]
        );
    }

    #[test]
    fn a_broken_root_without_ancestors_adds_no_edge() {
        // r → c と a → c で r が壊れ，r の上に祖先が無ければ，c には a からの edge だけが入る
        let g = graph(&[('r', ""), ('a', ""), ('c', "ra")]);
        assert_eq!(
            walk(&g, "r"),
            [
                "r:broken drop[] hold[]",
                "a:origin drop[] hold[a]",
                "c:a0 drop[ca] hold[]"
            ]
        );
    }

    #[test]
    fn a_version_whose_ancestors_are_all_broken_is_an_origin() {
        let g = graph(&[('r', ""), ('b', "r"), ('c', "b")]);
        assert_eq!(
            walk(&g, "rb"),
            [
                "r:broken drop[] hold[]",
                "b:broken drop[] hold[]",
                "c:origin drop[c] hold[]"
            ]
        );
    }

    #[test]
    fn edges_come_in_the_graph_order_of_their_parents() {
        // m の行では b を先に書いたが，グラフの順は a が先である
        let g = graph(&[('r', ""), ('a', "r"), ('b', "r"), ('m', "ba")]);
        let log = walk(&g, "");
        assert_eq!(log.last().unwrap(), "m:a0,b0 drop[mba] hold[]");
    }

    #[test]
    fn a_branch_ending_in_a_broken_version_releases_its_ancestor() {
        // a → x で x に子が無ければ，x を処理したときに a を捨てる
        let g = graph(&[('a', ""), ('x', "a"), ('b', "a"), ('c', "b")]);
        let log = walk(&g, "x");
        assert_eq!(
            log,
            [
                "a:origin drop[] hold[a]",
                "x:broken drop[] hold[a]",
                "b:a0 drop[a] hold[b]",
                "c:b0 drop[cb] hold[]",
            ]
        );
    }

    #[test]
    fn the_ancestor_is_held_until_every_child_of_the_broken_version_is_done() {
        // a → x → c と x → d で x が壊れれば，a は c と d の両方を処理するまで持つ
        let g = graph(&[('a', ""), ('x', "a"), ('c', "x"), ('d', "x")]);
        assert_eq!(
            walk(&g, "x"),
            [
                "a:origin drop[] hold[a]",
                "x:broken drop[] hold[a]",
                "c:a1 drop[c] hold[a]",
                "d:a1 drop[da] hold[]",
            ]
        );
    }

    #[test]
    fn finishing_again_from_the_start_gives_the_same_held_versions() {
        // 再開では，止まった所までの finish を呼び直して held を求める
        let g = graph(&[
            ('a', ""),
            ('x', "a"),
            ('b', "a"),
            ('y', "b"),
            ('c', "xy"),
            ('d', "c"),
        ]);
        let broken = "xy";
        let mut full = EdgeBuilder::new(&g);
        let mut held_at = Vec::new();
        for &v in &g.order {
            full.finish(v, broken.contains(name(&g, v)));
            held_at.push(full.held());
        }
        for (stop, expected) in held_at.iter().enumerate() {
            let mut again = EdgeBuilder::new(&g);
            for &v in &g.order[..=stop] {
                again.finish(v, broken.contains(name(&g, v)));
            }
            assert_eq!(&again.held(), expected, "stopped after {stop}");
        }
    }

    #[test]
    fn union_keeps_the_order_and_drops_duplicates() {
        let v = |n: u32| VersionIdx(n);
        assert_eq!(
            union(&[v(1), v(4)], &[v(2), v(4), v(7)], v(3)),
            [v(1), v(2), v(3), v(4), v(7)]
        );
        assert_eq!(union(&[], &[], v(5)), [v(5)]);
        assert_eq!(union(&[v(5)], &[], v(5)), [v(5)]);
    }
}
