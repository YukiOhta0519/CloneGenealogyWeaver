use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::graph::{Version, VersionIdx};

/// 最後に親が終わった版を先に選ぶのは，1 つの枝を続けて処理して，持っておく版の状態を減らすためである．
pub fn topological(versions: &[Version]) -> Result<Vec<VersionIdx>, VersionIdx> {
    let mut waiting: Vec<usize> = versions.iter().map(|v| v.parents.len()).collect();
    let mut ready: BinaryHeap<(usize, Reverse<VersionIdx>)> = BinaryHeap::new();
    for (i, count) in waiting.iter().enumerate() {
        if *count == 0 {
            ready.push((0, Reverse(VersionIdx(i as u32))));
        }
    }
    let mut order = Vec::with_capacity(versions.len());
    while let Some((_, Reverse(v))) = ready.pop() {
        order.push(v);
        for child in &versions[v.index()].children {
            waiting[child.index()] -= 1;
            if waiting[child.index()] == 0 {
                ready.push((order.len(), Reverse(*child)));
            }
        }
    }
    if order.len() < versions.len() {
        return Err(on_cycle(versions, &waiting));
    }
    Ok(order)
}

/// 残った版をそのまま出さないのは，閉路の先にあるだけの版も残るからである．
/// 残った版には残った親が必ずあるので，親をたどれば 2 回目に来る版がある．
fn on_cycle(versions: &[Version], waiting: &[usize]) -> VersionIdx {
    let remains = |v: &VersionIdx| waiting[v.index()] > 0;
    let mut v = VersionIdx(
        waiting
            .iter()
            .position(|&count| count > 0)
            .expect("a version remains") as u32,
    );
    let mut seen = vec![false; versions.len()];
    while !seen[v.index()] {
        seen[v.index()] = true;
        v = *versions[v.index()]
            .parents
            .iter()
            .find(|p| remains(p))
            .expect("a remaining version has a remaining parent");
    }
    v
}

#[cfg(test)]
mod tests {
    use crate::graph::{Graph, VersionIdx};

    /// 版の名前と，親の名前の並びから作る
    fn graph(lines: &[(&str, &[&str])]) -> Result<Graph, VersionIdx> {
        let index =
            |name: &str| VersionIdx(lines.iter().position(|(v, _)| *v == name).unwrap() as u32);
        Graph::new(
            lines
                .iter()
                .map(|(v, parents)| (v.to_string(), parents.iter().map(|p| index(p)).collect()))
                .collect(),
        )
    }

    fn names(lines: &[(&str, &[&str])]) -> Vec<String> {
        let g = graph(lines).unwrap();
        g.order
            .iter()
            .map(|v| g.versions[v.index()].commit.clone())
            .collect()
    }

    #[test]
    fn the_branch_that_became_ready_last_goes_first() {
        // r を終えると a と b が選べ，行の順で a を選ぶ．a を終えると c が選べ，b より後に選べたので先に選ぶ
        assert_eq!(
            names(&[("r", &[]), ("a", &["r"]), ("b", &["r"]), ("c", &["a"])]),
            ["r", "a", "c", "b"]
        );
    }

    #[test]
    fn versions_without_parents_follow_the_line_order() {
        assert_eq!(
            names(&[("x", &[]), ("y", &[]), ("z", &["y"])]),
            ["x", "y", "z"]
        );
        assert_eq!(
            names(&[("y", &[]), ("x", &[]), ("z", &["y"])]),
            ["y", "z", "x"]
        );
    }

    #[test]
    fn swapping_lines_changes_the_order_among_ties() {
        assert_eq!(
            names(&[("r", &[]), ("b", &["r"]), ("a", &["r"]), ("c", &["a"])]),
            ["r", "b", "a", "c"]
        );
    }

    #[test]
    fn a_merge_waits_for_all_parents_and_positions_follow_the_order() {
        let g = graph(&[("m", &["a", "b"]), ("a", &["r"]), ("b", &["r"]), ("r", &[])]).unwrap();
        let order: Vec<&str> = g
            .order
            .iter()
            .map(|v| g.versions[v.index()].commit.as_str())
            .collect();
        assert_eq!(order, ["r", "a", "b", "m"]);
        assert_eq!(g.position, [3, 1, 2, 0]);
        assert_eq!(g.versions[3].children, [VersionIdx(1), VersionIdx(2)]);
    }

    #[test]
    fn a_cycle_names_a_version_on_it_and_not_one_after_it() {
        // d は閉路 b → c → b の先にあるだけで，閉路の上には無い
        let err = graph(&[("d", &["c"]), ("a", &[]), ("b", &["a", "c"]), ("c", &["b"])])
            .err()
            .unwrap();
        assert!(err == VersionIdx(2) || err == VersionIdx(3), "{err:?}");
        let err = graph(&[("s", &["s"])]).err().unwrap();
        assert_eq!(err, VersionIdx(0));
    }
}
