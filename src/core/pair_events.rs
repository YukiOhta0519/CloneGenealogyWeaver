use std::collections::{BTreeMap, HashMap};

use crate::core::file_mapping::FileMap;
use crate::core::fragment_mapping::{ChildKind, Mapping};
use crate::core::model::{Fragment, Pair, VersionState};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisappearReason {
    FragmentDeleted,
    FragmentsMerged,
    OutOfScope,
    UndetectedFragment,
    NotPaired,
}

/// ラベルと類似度を持たせないのは，書き出しのときに子の版の PairInfo から引くからである．
#[derive(Clone, PartialEq, Debug)]
pub enum PairEvent {
    Appear {
        child: Pair,
        entered_scope: bool,
    },
    Persist {
        parent: Pair,
        child: Pair,
        labels_changed: Vec<bool>,
    },
    Merge {
        parents: Vec<Pair>,
        child: Pair,
    },
    Disappear {
        parent: Pair,
        reason: DisappearReason,
    },
}

/// Merge を一番前の親のペアの位置に 1 つだけ置くのは，書き出しで親のペアごとの行に分けるからである．
pub fn pair_events(
    parent: &VersionState,
    child: &VersionState,
    mappings: &[Mapping],
    files: &FileMap,
) -> Vec<PairEvent> {
    let child_of: HashMap<&Fragment, &Mapping> = mappings.iter().map(|m| (&m.parent, m)).collect();
    let mapped = |fragment: &Fragment| {
        *child_of
            .get(fragment)
            .unwrap_or_else(|| panic!("fragment {fragment} of a parent pair was not mapped"))
    };

    // 判定の順を気にしないのは，5 つの理由が重ならず，順で結果が変わらないからである
    let mut outcomes: Vec<(&Pair, Result<Pair, DisappearReason>)> =
        Vec::with_capacity(parent.pairs.len());
    for pair in parent.pairs.keys() {
        let (u, v) = (mapped(&pair.a), mapped(&pair.b));
        let outcome = match (&u.child, &v.child) {
            (None, _) | (_, None) => Err(DisappearReason::FragmentDeleted),
            (Some((cu, _)), Some((cv, _))) if cu == cv => Err(DisappearReason::FragmentsMerged),
            (Some((cu, _)), Some((cv, _)))
                if !child.scope.contains(cu.path.as_bytes())
                    || !child.scope.contains(cv.path.as_bytes()) =>
            {
                Err(DisappearReason::OutOfScope)
            }
            (Some((_, ku)), Some((_, kv)))
                if *ku == ChildKind::Projected || *kv == ChildKind::Projected =>
            {
                Err(DisappearReason::UndetectedFragment)
            }
            (Some((cu, _)), Some((cv, _))) => {
                let continued =
                    Pair::new(cu.clone(), cv.clone()).expect("two different fragments make a pair");
                if child.pairs.contains_key(&continued) {
                    Ok(continued)
                } else {
                    Err(DisappearReason::NotPaired)
                }
            }
        };
        outcomes.push((pair, outcome));
    }

    let mut parents_of: BTreeMap<&Pair, Vec<Pair>> = BTreeMap::new();
    for (pair, outcome) in &outcomes {
        if let Ok(continued) = outcome {
            parents_of
                .entry(continued)
                .or_default()
                .push((*pair).clone());
        }
    }

    let mut events = Vec::new();
    for (pair, outcome) in &outcomes {
        match outcome {
            Err(reason) => events.push(PairEvent::Disappear {
                parent: (*pair).clone(),
                reason: *reason,
            }),
            Ok(continued) => {
                let parents = &parents_of[continued];
                if parents[0] != **pair {
                    continue;
                }
                if parents.len() == 1 {
                    let before = &parent.pairs[*pair].labels;
                    let after = &child.pairs[continued].labels;
                    events.push(PairEvent::Persist {
                        parent: (*pair).clone(),
                        child: continued.clone(),
                        labels_changed: before.iter().zip(after).map(|(b, a)| b != a).collect(),
                    });
                } else {
                    events.push(PairEvent::Merge {
                        parents: parents.clone(),
                        child: continued.clone(),
                    });
                }
            }
        }
    }
    for pair in child.pairs.keys() {
        if !parents_of.contains_key(pair) {
            events.push(PairEvent::Appear {
                child: pair.clone(),
                entered_scope: entered_scope(pair, parent, files),
            });
        }
    }
    events
}

/// 新しく足したファイルに付けないのは，親の版に無く，範囲に入る前のファイルが無いからである．
/// 改名で出どころを引くのは，範囲の外のファイルを範囲の中へ移した場合も追うためである
fn entered_scope(pair: &Pair, parent: &VersionState, files: &FileMap) -> bool {
    [&pair.a, &pair.b].iter().any(|fragment| {
        files
            .source(fragment.path.as_bytes())
            .is_some_and(|source| !parent.scope.contains(source))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::file_mapping::EdgeLines;
    use crate::core::fragment_mapping::map_fragments;
    use crate::core::model::{RawEntry, RawStatus, Side};
    use crate::core::testing::*;

    /// どのファイルも中身が同じで，1 行に 1 語ずつ違う語が 20 行並ぶとする．files は改名などの項目
    fn run(
        parent: &[PairSpec],
        parent_scope: &[&str],
        child: &[PairSpec],
        child_scope: &[&str],
        files: FileMap,
    ) -> Vec<String> {
        let parent_state = state(parent, parent_scope);
        let child_state = state(child, child_scope);
        let text: String = (1..=20).map(|i| format!("w{i}\n")).collect();
        let mut lines = EdgeLines::default();
        let paths: std::collections::BTreeSet<&str> = parent
            .iter()
            .chain(child)
            .flat_map(|(a, b, _)| [a.0, b.0])
            .collect();
        for path in paths {
            let new_path = match files.old_file(path.as_bytes()) {
                crate::core::file_mapping::OldFile::Changed { new_path, .. } => new_path.clone(),
                _ => path.as_bytes().to_vec(),
            };
            lines.insert(
                Some(path.as_bytes().to_vec()),
                Some(new_path),
                file_lines(&text, &text),
            );
        }
        let mappings = map_fragments(&parent_state, &child_state, &files, &lines).unwrap();
        pair_events(&parent_state, &child_state, &mappings, &files)
            .iter()
            .map(show)
            .collect()
    }

    fn show(event: &PairEvent) -> String {
        let pair = |p: &Pair| format!("{}~{}", p.a, p.b);
        match event {
            PairEvent::Appear {
                child,
                entered_scope,
            } => format!(
                "Appear {}{}",
                pair(child),
                if *entered_scope { " entered" } else { "" }
            ),
            PairEvent::Persist {
                parent,
                child,
                labels_changed,
            } => format!(
                "Persist {} -> {} {labels_changed:?}",
                pair(parent),
                pair(child)
            ),
            PairEvent::Merge { parents, child } => format!(
                "Merge {} -> {}",
                parents.iter().map(pair).collect::<Vec<_>>().join(" + "),
                pair(child)
            ),
            PairEvent::Disappear { parent, reason } => {
                format!("Disappear {} {reason:?}", pair(parent))
            }
        }
    }

    const ALL: &[&str] = &["a", "b", "c"];

    #[test]
    fn a_pair_that_stays_persists_and_marks_changed_labels_per_plugin() {
        let events = run(
            &[(("a", 1, 3), ("b", 1, 3), &["intra", "product"])],
            ALL,
            &[(("a", 1, 3), ("b", 1, 3), &["inter", "product"])],
            ALL,
            FileMap::default(),
        );
        assert_eq!(events, ["Persist a:1-3~b:1-3 -> a:1-3~b:1-3 [true, false]"]);
    }

    #[test]
    fn fragment_deleted_and_fragments_merged() {
        // a:5-6 の行が全部消えたことにはできないので，b を消されたファイルにする
        let side = |path: &str| Side {
            mode: 0o100644,
            oid: "1".repeat(40),
            path: path.as_bytes().to_vec(),
        };
        let deleted = FileMap::new(&[RawEntry {
            status: RawStatus::Deleted,
            old: Some(side("b")),
            new: None,
        }]);
        let parent_state = state(&[(("a", 1, 3), ("b", 1, 3), &[])], ALL);
        let mut lines = EdgeLines::default();
        let text: String = (1..=20).map(|i| format!("w{i}\n")).collect();
        lines.insert(
            Some(b"a".to_vec()),
            Some(b"a".to_vec()),
            file_lines(&text, &text),
        );
        lines.insert(
            Some(b"b".to_vec()),
            None,
            crate::core::file_mapping::FileLines::deleted(blob(&text)),
        );
        let mappings =
            map_fragments(&parent_state, &VersionState::empty(), &deleted, &lines).unwrap();
        let events: Vec<String> =
            pair_events(&parent_state, &VersionState::empty(), &mappings, &deleted)
                .iter()
                .map(show)
                .collect();
        assert_eq!(events, ["Disappear a:1-3~b:1-3 FragmentDeleted"]);

        // a:2-3 と a:3-4 の両端が，子の版の a:1-5 に続く
        let events = run(
            &[(("a", 2, 3), ("a", 3, 4), &[])],
            ALL,
            &[(("a", 1, 5), ("b", 1, 1), &[])],
            ALL,
            FileMap::default(),
        );
        assert_eq!(events[0], "Disappear a:2-3~a:3-4 FragmentsMerged");
        assert_eq!(events[1], "Appear a:1-5~b:1-1");
    }

    #[test]
    fn out_of_scope_undetected_and_not_paired() {
        let events = run(
            &[(("a", 1, 3), ("b", 1, 3), &[])],
            ALL,
            &[(("a", 1, 3), ("c", 1, 3), &[])],
            &["a", "c"],
            FileMap::default(),
        );
        assert_eq!(events[0], "Disappear a:1-3~b:1-3 OutOfScope");
        let events = run(
            &[(("a", 1, 3), ("b", 1, 3), &[])],
            ALL,
            &[(("a", 1, 3), ("c", 1, 3), &[])],
            ALL,
            FileMap::default(),
        );
        assert_eq!(events[0], "Disappear a:1-3~b:1-3 UndetectedFragment");
        let events = run(
            &[(("a", 1, 3), ("b", 1, 3), &[])],
            ALL,
            &[
                (("a", 1, 3), ("c", 1, 3), &[]),
                (("b", 1, 3), ("c", 4, 6), &[]),
            ],
            ALL,
            FileMap::default(),
        );
        assert_eq!(events[0], "Disappear a:1-3~b:1-3 NotPaired");
    }

    #[test]
    fn two_parent_pairs_reaching_one_child_pair_merge_at_the_first_parent() {
        // a:2-3~b:2-3 と a:2-4~b:2-4 がどちらも a:1-5~b:1-5 に続く．間に c のペアの Persist が入る
        let events = run(
            &[
                (("a", 2, 3), ("b", 2, 3), &[]),
                (("a", 2, 3), ("c", 1, 3), &[]),
                (("a", 2, 4), ("b", 2, 4), &[]),
            ],
            ALL,
            &[
                (("a", 1, 5), ("b", 1, 5), &[]),
                (("a", 1, 5), ("c", 1, 3), &[]),
            ],
            ALL,
            FileMap::default(),
        );
        assert_eq!(
            events,
            [
                "Merge a:2-3~b:2-3 + a:2-4~b:2-4 -> a:1-5~b:1-5",
                "Persist a:2-3~c:1-3 -> a:1-5~c:1-3 []",
            ]
        );
    }

    #[test]
    fn an_inner_pair_persists_into_the_outer_pair() {
        // 内側の片 5 行は外側の 6 行の片と 2 × 5 ÷ 11 = 0.91 で，外側のペアに続く
        let events = run(
            &[(("a", 2, 6), ("b", 2, 6), &[])],
            ALL,
            &[(("a", 1, 6), ("b", 1, 6), &[])],
            ALL,
            FileMap::default(),
        );
        assert_eq!(events, ["Persist a:2-6~b:2-6 -> a:1-6~b:1-6 []"]);
    }

    #[test]
    fn appear_marks_files_that_were_outside_the_parent_scope() {
        let side = |path: &str| Side {
            mode: 0o100644,
            oid: "1".repeat(40),
            path: path.as_bytes().to_vec(),
        };
        let files = FileMap::new(&[
            RawEntry {
                status: RawStatus::Renamed { score: 100 },
                old: Some(side("vendor/x")),
                new: Some(side("c")),
            },
            RawEntry {
                status: RawStatus::Added,
                old: None,
                new: Some(side("d")),
            },
        ]);
        let events = run(
            &[],
            &["a"],
            &[
                (("a", 1, 2), ("b", 1, 2), &[]),
                (("a", 1, 2), ("c", 1, 2), &[]),
                (("a", 1, 2), ("d", 1, 2), &[]),
            ],
            &["a", "b", "c", "d"],
            files,
        );
        // b は親の版にあったが範囲の外，c は範囲の外から改名で移った，d は新しく足した
        assert_eq!(
            events,
            [
                "Appear a:1-2~b:1-2 entered",
                "Appear a:1-2~c:1-2 entered",
                "Appear a:1-2~d:1-2"
            ]
        );
    }

    #[test]
    fn disappear_and_persist_follow_the_parent_order_and_appear_follows() {
        let events = run(
            &[
                (("a", 1, 2), ("b", 1, 2), &[]),
                (("a", 1, 2), ("c", 1, 2), &[]),
            ],
            ALL,
            &[
                (("a", 1, 2), ("c", 1, 2), &[]),
                (("a", 4, 5), ("b", 4, 5), &[]),
            ],
            &["a", "c"],
            FileMap::default(),
        );
        assert_eq!(
            events,
            [
                "Disappear a:1-2~b:1-2 OutOfScope",
                "Persist a:1-2~c:1-2 -> a:1-2~c:1-2 []",
                "Appear a:4-5~b:4-5",
            ]
        );
    }
}
