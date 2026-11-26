use std::collections::HashSet;

use crate::core::file_mapping::{EdgeLines, FileMap, NewFile};
use crate::core::fragment_mapping::Mapping;
use crate::core::line_events::NewLine;
use crate::core::model::{Fragment, VersionState};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContentEvent {
    Stable,
    Modified,
    Deleted,
}

/// 行を見直さずに片の対応の数から決めるのは，数が行ごとの判定をそのまま数えたもので，結果が同じだからである
pub fn content_event(mapping: &Mapping) -> ContentEvent {
    if mapping.child.is_none() {
        return ContentEvent::Deleted;
    }
    let c = &mapping.counts;
    let parent_changed = c.retained_lines < c.parent_lines || c.modified_lines > 0;
    let child_changed = c.added_lines > 0 || c.taken_in_modified_lines > 0;
    if parent_changed || child_changed || c.whitespace_lines > 0 {
        ContentEvent::Modified
    } else {
        ContentEvent::Stable
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub fragment: Fragment,
    pub added: bool,
    pub file_added: bool,
}

/// Added でない片も返すのは，どの親の片の続き先でもない子の検出された片を，Added でなくても edge ごとに書くからである．
pub fn entries(
    child: &VersionState,
    mappings: &[Mapping],
    files: &FileMap,
    lines: &EdgeLines,
) -> Vec<Entry> {
    let targets: HashSet<&Fragment> = mappings
        .iter()
        .filter_map(|m| m.child.as_ref().map(|(fragment, _)| fragment))
        .collect();
    child
        .fragments
        .iter()
        .filter(|fragment| !targets.contains(fragment))
        .map(|fragment| {
            let path = fragment.path.as_bytes();
            let file_added = matches!(files.new_file(path), NewFile::Added { .. });
            let added = file_added || {
                let file = lines
                    .by_new(path)
                    .unwrap_or_else(|| panic!("the lines of {} were not prepared", fragment.path));
                let mut nonblank = fragment
                    .lines()
                    .filter(|&j| !file.new.is_blank(j))
                    .peekable();
                nonblank.peek().is_some()
                    && nonblank.all(|j| file.events.new[j as usize] == NewLine::Added)
            };
            Entry {
                fragment: fragment.clone(),
                added,
                file_added,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::file_mapping::FileLines;
    use crate::core::fragment_mapping::map_fragments;
    use crate::core::model::{RawEntry, RawStatus, Side};
    use crate::core::testing::*;

    fn events(
        old: &str,
        new: &str,
        parent: (u32, u32),
        child: &[(u32, u32)],
    ) -> (Vec<ContentEvent>, Vec<Entry>) {
        let parent_state = state(&[(("a", parent.0, parent.1), ("z", 1, 1), &[])], &[]);
        let pairs: Vec<_> = child
            .iter()
            .map(|(s, e)| (("a", *s, *e), ("z", 1, 1), &[][..]))
            .collect();
        let child_state = state(&pairs, &[]);
        let mut lines = EdgeLines::default();
        lines.insert(
            Some(b"a".to_vec()),
            Some(b"a".to_vec()),
            file_lines(old, new),
        );
        lines.insert(
            Some(b"z".to_vec()),
            Some(b"z".to_vec()),
            file_lines("z\n", "z\n"),
        );
        let files = FileMap::default();
        let mappings = map_fragments(&parent_state, &child_state, &files, &lines).unwrap();
        let contents = mappings
            .iter()
            .filter(|m| &*m.parent.path == "a")
            .map(content_event)
            .collect();
        (contents, entries(&child_state, &mappings, &files, &lines))
    }

    #[test]
    fn stable_when_every_line_slides_even_if_the_range_grows_over_slid_lines() {
        let text = "a b\nc d\ne f\n";
        assert_eq!(
            events(text, text, (1, 2), &[(1, 2)]).0,
            [ContentEvent::Stable]
        );
        assert_eq!(
            events(text, text, (2, 2), &[(1, 3)]).0,
            [ContentEvent::Stable]
        );
    }

    #[test]
    fn modified_by_whitespace_additions_and_taken_in_modifications() {
        let old = "a b\nc d\ne f\ng h\n";
        // 空白だけの修正
        assert_eq!(
            events(old, "a b\n  c d\ne f\ng h\n", (1, 3), &[(1, 3)]).0,
            [ContentEvent::Modified]
        );
        // 子の片に追加の行
        assert_eq!(
            events(old, "a b\nnew line\nc d\ne f\ng h\n", (1, 3), &[(1, 4)]).0,
            [ContentEvent::Modified]
        );
        // 取り込んだ行の修正．親の片は 1 から 2 行目，子の片は 1 から 3 行目で，3 行目は e f から e g になった
        assert_eq!(
            events(old, "a b\nc d\ne g\ng h\n", (1, 2), &[(1, 3)]).0,
            [ContentEvent::Modified]
        );
        // 取り込んだ行がスライドだけなら Stable
        assert_eq!(
            events(old, old, (1, 2), &[(1, 3)]).0,
            [ContentEvent::Stable]
        );
        // 親の片の行の削除
        assert_eq!(
            events(old, "a b\ne f\ng h\n", (1, 3), &[(1, 2)]).0,
            [ContentEvent::Modified]
        );
    }

    #[test]
    fn deleted_when_no_line_remains() {
        assert_eq!(
            events("a b\nc d\n", "x y\n", (1, 2), &[(1, 1)]).0,
            [ContentEvent::Deleted]
        );
    }

    #[test]
    fn fragments_that_no_parent_fragment_follows_are_entries() {
        let old = "a b\nc d\n";
        let new = "a b\nc d\nnew one\nnew two\n";
        let (_, entries) = events(old, new, (1, 2), &[(1, 2), (2, 4), (3, 4)]);
        let got: Vec<(String, bool, bool)> = entries
            .iter()
            .map(|e| (e.fragment.to_string(), e.added, e.file_added))
            .collect();
        // 1-2 と z:1-1 は親の片の続き先．2-4 は c d を含むので Added にならない．3-4 は全部追加の行
        assert_eq!(
            got,
            [
                ("a:2-4".to_string(), false, false),
                ("a:3-4".to_string(), true, false),
            ]
        );
    }

    #[test]
    fn every_fragment_is_added_from_the_empty_tree() {
        let child_state = state(&[(("a", 1, 2), ("b", 1, 1), &[])], &[]);
        let side = |path: &str| Side {
            mode: 0o100644,
            oid: "1".repeat(40),
            path: path.as_bytes().to_vec(),
        };
        let files = FileMap::new(&[
            RawEntry {
                status: RawStatus::Added,
                old: None,
                new: Some(side("a")),
            },
            RawEntry {
                status: RawStatus::Added,
                old: None,
                new: Some(side("b")),
            },
        ]);
        let mut lines = EdgeLines::default();
        lines.insert(None, Some(b"a".to_vec()), FileLines::added(blob("x\ny\n")));
        let entries = entries(&child_state, &[], &files, &lines);
        assert!(entries.iter().all(|e| e.added && e.file_added));
        assert_eq!(entries.len(), 2);
    }
}
