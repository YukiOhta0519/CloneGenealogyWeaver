use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use anyhow::{Result, bail};

use crate::core::file_mapping::{EdgeLines, FileLines, FileMap, OldFile};
use crate::core::line_events::{NewLine, OldLine};
use crate::core::model::{Fragment, VersionState};
use crate::core::similarity::{WordCounts, common_words};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChildKind {
    Detected,
    Projected,
}

/// 列ごとの数にするのは，fragment_mapping の表の数の列に 1 対 1 で書くためである
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct MappingCounts {
    pub parent_lines: u32,
    pub parent_symbol_lines: u32,
    pub parent_words: u32,
    pub retained_lines: u32,
    pub retained_symbol_lines: u32,
    pub retained_words: u32,
    pub common_words: u32,
    pub modified_lines: u32,
    pub whitespace_lines: u32,
    pub taken_in_lines: u32,
    pub taken_in_modified_lines: u32,
    pub added_lines: u32,
    pub child_words: Option<u32>,
}

impl MappingCounts {
    pub fn word_counts(&self) -> Option<WordCounts> {
        self.child_words.map(|child| WordCounts {
            parent: self.parent_words,
            child,
            common: self.common_words,
        })
    }

    pub fn text_similarity(&self) -> Option<f64> {
        self.word_counts().map(|counts| counts.similarity())
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Mapping {
    pub parent: Fragment,
    pub child: Option<(Fragment, ChildKind)>,
    pub renamed: bool,
    pub similarity_cut: bool,
    pub file_deleted: bool,
    pub counts: MappingCounts,
}

pub fn map_fragments(
    parent: &VersionState,
    child: &VersionState,
    files: &FileMap,
    lines: &EdgeLines,
) -> Result<Vec<Mapping>> {
    let mut old_sums: HashMap<Rc<str>, OldSums> = HashMap::new();
    let mut new_words: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
    parent
        .fragments
        .iter()
        .map(|fragment| map_one(fragment, child, files, lines, &mut old_sums, &mut new_words))
        .collect()
}

fn map_one(
    fragment: &Fragment,
    child: &VersionState,
    files: &FileMap,
    lines: &EdgeLines,
    old_sums: &mut HashMap<Rc<str>, OldSums>,
    new_words: &mut HashMap<Vec<u8>, Vec<u32>>,
) -> Result<Mapping> {
    let path = fragment.path.as_bytes();
    let file = lines
        .by_old(path)
        .unwrap_or_else(|| panic!("the lines of {} were not prepared", fragment.path));
    let sums = old_sums
        .entry(Rc::clone(&fragment.path))
        .or_insert_with(|| OldSums::new(file));
    let range = fragment.lines();
    let counts = sums.parent_counts(&range);
    let deleted = |counts, renamed, file_deleted| Mapping {
        parent: fragment.clone(),
        child: None,
        renamed,
        similarity_cut: false,
        file_deleted,
        counts,
    };
    let (new_path, renamed) = match files.old_file(path) {
        OldFile::Same => (path, false),
        OldFile::Changed {
            new_path, renamed, ..
        } => (new_path.as_slice(), *renamed),
        OldFile::Deleted { .. } => return Ok(deleted(counts, false, true)),
    };
    if counts.retained_lines == 0 {
        return Ok(deleted(counts, renamed, false));
    }
    // 最初と最後の残った行だけを見るのは，行き先が行の順を保つからである
    let (first, last) = sums.first_and_last_retained(&range);
    let (lo, hi) = (
        destination(file.events.old[first as usize]),
        destination(file.events.old[last as usize]),
    );

    let Ok(new_path) = std::str::from_utf8(new_path) else {
        bail!(
            "cannot follow fragment {fragment}: its file was renamed to {:?}, whose path is not UTF-8",
            String::from_utf8_lossy(new_path)
        );
    };
    let words = new_words
        .entry(new_path.as_bytes().to_vec())
        .or_insert_with(|| prefix(file.new.len(), |j| file.new.words(j)));
    let child_words = |g: &Fragment| sum(words, &g.lines());
    let cut = |child_words: u32| {
        WordCounts {
            parent: counts.parent_words,
            child: child_words,
            common: counts.common_words,
        }
        .is_cut()
    };

    // 規則 1．全部の片を見ずに二分探索で絞るのは，索引が開始行の順に並んでいるからである
    if let Some(candidates) = child.index.get(new_path) {
        let upto = candidates.partition_point(|g| g.start <= lo + 1);
        let smallest = candidates[..upto]
            .iter()
            .filter(|g| g.end > hi)
            .min_by_key(|g| (g.end - g.start, g.start));
        if let Some(g) = smallest
            && !cut(child_words(g))
        {
            let counts = with_child(counts, file, &range, g, child_words(g));
            return Ok(Mapping {
                parent: fragment.clone(),
                child: Some((g.clone(), ChildKind::Detected)),
                renamed,
                similarity_cut: false,
                file_deleted: false,
                counts,
            });
        }
    }
    // 規則 2．規則 1 の片が 0.3 未満のときにほかの全部を含む片を試さないのは，規則 2 の決まりである
    let child_path = child
        .index
        .get_key_value(new_path)
        .map(|(path, _)| Rc::clone(path))
        .unwrap_or_else(|| Rc::from(new_path));
    let projected = Fragment::from_lines(child_path, lo..hi + 1);
    let projected_words = child_words(&projected);
    let counts = with_child(counts, file, &range, &projected, projected_words);
    if !cut(projected_words) {
        return Ok(Mapping {
            parent: fragment.clone(),
            child: Some((projected, ChildKind::Projected)),
            renamed,
            similarity_cut: false,
            file_deleted: false,
            counts,
        });
    }
    // 規則 3．属性を投影した範囲で数えたまま残すのは，表の CHECK がそれを前提にするからである
    Ok(Mapping {
        parent: fragment.clone(),
        child: None,
        renamed,
        similarity_cut: true,
        file_deleted: false,
        counts,
    })
}

fn destination(event: OldLine) -> u32 {
    match event {
        OldLine::Slide(j) | OldLine::WsOnly(j) | OldLine::Modified(j) => j,
        OldLine::Deleted => unreachable!("a retained line is not deleted"),
    }
}

/// 取り込んだ行を累積和にしないのは，子の片の行ごとに出どころが親の片の外かを見るからである．
fn with_child(
    mut counts: MappingCounts,
    file: &FileLines,
    parent: &Range<u32>,
    child: &Fragment,
    child_words: u32,
) -> MappingCounts {
    for j in child.lines() {
        if file.new.is_blank(j) {
            continue;
        }
        let source = match file.events.new[j as usize] {
            NewLine::Added => {
                counts.added_lines += 1;
                continue;
            }
            NewLine::Slide(i) | NewLine::WsOnly(i) | NewLine::Modified(i) => i,
        };
        if parent.contains(&source) {
            continue;
        }
        counts.taken_in_lines += 1;
        match file.events.new[j as usize] {
            NewLine::Modified(_) => counts.taken_in_modified_lines += 1,
            NewLine::WsOnly(_) => counts.whitespace_lines += 1,
            _ => {}
        }
    }
    counts.child_words = Some(child_words);
    counts
}

/// 累積和にするのは，入れ子の片が多いファイルで，同じ行を片ごとに数え直さないためである．
struct OldSums {
    nonblank: Vec<u32>,
    symbol: Vec<u32>,
    words: Vec<u32>,
    retained: Vec<u32>,
    retained_symbol: Vec<u32>,
    retained_words: Vec<u32>,
    common: Vec<u32>,
    modified: Vec<u32>,
    whitespace: Vec<u32>,
    retained_at: Vec<u32>,
}

impl OldSums {
    fn new(file: &FileLines) -> OldSums {
        let old = &file.old;
        let retained = |i: u32| !old.is_blank(i) && file.events.old[i as usize] != OldLine::Deleted;
        let common = |i: u32| match file.events.old[i as usize] {
            // スライドの行で語を取り出さないのは，バイト列で同じ行の共通の語は語の数と同じだからである
            OldLine::Slide(_) => old.words(i),
            OldLine::WsOnly(j) | OldLine::Modified(j) => {
                common_words(old.line(i), file.new.line(j))
            }
            OldLine::Deleted => 0,
        };
        let n = old.len();
        OldSums {
            nonblank: prefix(n, |i| u32::from(!old.is_blank(i))),
            symbol: prefix(n, |i| u32::from(old.is_symbol_only(i))),
            words: prefix(n, |i| old.words(i)),
            retained: prefix(n, |i| u32::from(retained(i))),
            retained_symbol: prefix(n, |i| u32::from(retained(i) && old.is_symbol_only(i))),
            retained_words: prefix(n, |i| if retained(i) { old.words(i) } else { 0 }),
            common: prefix(n, |i| if retained(i) { common(i) } else { 0 }),
            modified: prefix(n, |i| {
                u32::from(
                    retained(i) && matches!(file.events.old[i as usize], OldLine::Modified(_)),
                )
            }),
            whitespace: prefix(n, |i| {
                u32::from(retained(i) && matches!(file.events.old[i as usize], OldLine::WsOnly(_)))
            }),
            retained_at: (0..n).filter(|&i| retained(i)).collect(),
        }
    }

    fn parent_counts(&self, range: &Range<u32>) -> MappingCounts {
        MappingCounts {
            parent_lines: sum(&self.nonblank, range),
            parent_symbol_lines: sum(&self.symbol, range),
            parent_words: sum(&self.words, range),
            retained_lines: sum(&self.retained, range),
            retained_symbol_lines: sum(&self.retained_symbol, range),
            retained_words: sum(&self.retained_words, range),
            common_words: sum(&self.common, range),
            modified_lines: sum(&self.modified, range),
            whitespace_lines: sum(&self.whitespace, range),
            ..MappingCounts::default()
        }
    }

    fn first_and_last_retained(&self, range: &Range<u32>) -> (u32, u32) {
        let first = self.retained_at.partition_point(|&i| i < range.start);
        let end = self.retained_at.partition_point(|&i| i < range.end);
        (self.retained_at[first], self.retained_at[end - 1])
    }
}

/// u32 で足りるのは，行の数も語の数も blob のバイト数を超えず，blob が 4 GiB 未満だからである．
fn prefix(n: u32, value: impl Fn(u32) -> u32) -> Vec<u32> {
    let mut sums = Vec::with_capacity(n as usize + 1);
    let mut total = 0u32;
    sums.push(0);
    for i in 0..n {
        total += value(i);
        sums.push(total);
    }
    sums
}

fn sum(sums: &[u32], range: &Range<u32>) -> u32 {
    sums[range.end as usize] - sums[range.start as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{RawEntry, RawStatus, Side};
    use crate::core::testing::*;

    fn edge(path: &str, old: &str, new: &str) -> EdgeLines {
        let mut lines = EdgeLines::default();
        lines.insert(
            Some(path.as_bytes().to_vec()),
            Some(path.as_bytes().to_vec()),
            file_lines(old, new),
        );
        lines
    }

    fn one(
        parent: (&str, u32, u32),
        child_fragments: &[(&str, u32, u32)],
        old: &str,
        new: &str,
    ) -> Mapping {
        let other = ("other", 1, 1);
        let parent_state = state(&[(parent, other, &[])], &[]);
        let pairs: Vec<_> = child_fragments
            .iter()
            .map(|f| (*f, other, &[][..]))
            .collect();
        let child_state = state(&pairs, &[]);
        let mut lines = edge(parent.0, old, new);
        lines.insert(
            Some(b"other".to_vec()),
            Some(b"other".to_vec()),
            file_lines("o\n", "o\n"),
        );
        let mappings =
            map_fragments(&parent_state, &child_state, &FileMap::default(), &lines).unwrap();
        mappings
            .into_iter()
            .find(|m| m.parent == fragment(parent.0, parent.1, parent.2))
            .unwrap()
    }

    fn child_of(m: &Mapping) -> Option<(String, ChildKind)> {
        m.child.as_ref().map(|(f, kind)| (f.to_string(), *kind))
    }

    /// 1 行に 1 語ずつ，違う語を並べた中身
    fn words(n: u32) -> String {
        (1..=n).map(|i| format!("w{i}\n")).collect()
    }

    #[test]
    fn rule_1_takes_the_smallest_containing_fragment_and_the_earlier_one_on_a_tie() {
        let text = words(10);
        let m = one(
            ("a", 3, 5),
            &[("a", 1, 10), ("a", 2, 6), ("a", 3, 7), ("a", 4, 5)],
            &text,
            &text,
        );
        // 2-6 と 3-7 は同じ 5 行で，開始行が前の 2-6 を選ぶ．4-5 は 3 行目を含まない
        assert_eq!(
            child_of(&m),
            Some(("a:2-6".to_string(), ChildKind::Detected))
        );
        assert_eq!(m.counts.taken_in_lines, 2);
        assert_eq!(m.counts.child_words, Some(5));
    }

    #[test]
    fn an_inner_fragment_does_not_follow_a_much_larger_outer_one() {
        // 内側の 5 行の片は外側の 50 行の片と 2 × 5 ÷ 55 = 0.18 で，投影した範囲に続く
        let text = words(50);
        let m = one(("a", 10, 14), &[("a", 1, 50)], &text, &text);
        assert_eq!(
            child_of(&m),
            Some(("a:10-14".to_string(), ChildKind::Projected))
        );
        assert_eq!(m.counts.text_similarity(), Some(1.0));
        assert!(!m.similarity_cut);
        // 外側が 15 行なら 2 × 5 ÷ 20 = 0.5 で外側に続く
        let text = words(15);
        let m = one(("a", 10, 14), &[("a", 1, 15)], &text, &text);
        assert_eq!(
            child_of(&m),
            Some(("a:1-15".to_string(), ChildKind::Detected))
        );
        assert_eq!(m.counts.text_similarity(), Some(0.5));
    }

    #[test]
    fn rule_3_cuts_a_fragment_whose_lines_were_rewritten_in_place() {
        // 4 行のうち 3 行で，語の後に 1 文字ずつ足した．行は修正の組になるが，共通の語は 1 行目の 3 つだけになる
        let old = "alpha beta gamma\ndelta epsilon zeta\neta theta iota\nkappa lambda mu\n";
        let new =
            "alpha beta gamma\ndeltaX epsilonX zetaX\netaX thetaX iotaX\nkappaX lambdaX muX\n";
        let m = one(("a", 1, 4), &[], old, new);
        assert!(m.similarity_cut);
        assert_eq!(m.child, None);
        // 2 × 3 ÷ 24 = 0.25 で切る．属性は投影した範囲で数える
        assert_eq!(m.counts.common_words, 3);
        assert_eq!(m.counts.child_words, Some(12));
        assert_eq!(m.counts.retained_lines, 4);
        assert_eq!(m.counts.modified_lines, 3);
    }

    #[test]
    fn a_fragment_whose_lines_were_all_deleted_has_no_similarity() {
        let m = one(("a", 2, 3), &[], "x\ny\nz\nw\n", "x\nw\n");
        assert_eq!(m.child, None);
        assert!(!m.similarity_cut);
        assert!(!m.file_deleted);
        assert_eq!(m.counts.retained_lines, 0);
        assert_eq!(m.counts.parent_lines, 2);
        assert_eq!(m.counts.child_words, None);
    }

    #[test]
    fn removed_blank_lines_are_not_counted() {
        let m = one(("a", 1, 4), &[("a", 1, 2)], "a1\n\n\na2\n", "a1\na2\n");
        assert_eq!(
            child_of(&m),
            Some(("a:1-2".to_string(), ChildKind::Detected))
        );
        assert_eq!(m.counts.parent_lines, 2);
        assert_eq!(m.counts.retained_lines, 2);
        assert_eq!(m.counts.added_lines, 0);
        assert_eq!(m.counts.taken_in_lines, 0);
    }

    #[test]
    fn taken_in_lines_count_slides_modifications_and_whitespace_changes() {
        // 親の片は 3 から 4 行目．子の片は 1 から 6 行目まで広がった
        let old = "int a;\nint b = 1;\nfoo(x);\nbar(y);\nint c;\nint d;\n";
        let new = "int a;\nint b = 2;\nfoo(x);\nbar(y);\n  int c;\nint e;\nint d;\n";
        let m = one(("a", 3, 4), &[("a", 1, 7)], old, new);
        assert_eq!(
            child_of(&m),
            Some(("a:1-7".to_string(), ChildKind::Detected))
        );
        let c = m.counts;
        assert_eq!((c.retained_lines, c.modified_lines), (2, 0));
        // int a; はスライド，int b = 2; は修正，  int c; は空白だけの修正，int d; はスライド
        assert_eq!(c.taken_in_lines, 4);
        assert_eq!(c.taken_in_modified_lines, 1);
        assert_eq!(c.whitespace_lines, 1);
        assert_eq!(c.added_lines, 1);
    }

    #[test]
    fn symbols_and_words_are_counted_in_the_parent_version() {
        let m = one(
            ("a", 1, 3),
            &[("a", 1, 3)],
            "if (a) {\n  return b;\n}\n",
            "if (a) {\n  return c;\n}\n",
        );
        let c = m.counts;
        assert_eq!(
            (c.parent_lines, c.parent_symbol_lines, c.parent_words),
            (3, 1, 4)
        );
        assert_eq!(
            (c.retained_lines, c.retained_symbol_lines, c.retained_words),
            (3, 1, 4)
        );
        assert_eq!((c.modified_lines, c.common_words), (1, 3));
        assert_eq!(c.child_words, Some(4));
    }

    fn rename_entry(from: &str, to: &str) -> RawEntry {
        let side = |path: &str| Side {
            mode: 0o100644,
            oid: "1".repeat(40),
            path: path.as_bytes().to_vec(),
        };
        RawEntry {
            status: RawStatus::Renamed { score: 100 },
            old: Some(side(from)),
            new: Some(side(to)),
        }
    }

    #[test]
    fn renamed_files_carry_the_mark_and_the_new_path() {
        let parent_state = state(&[(("old/A.java", 1, 2), ("old/A.java", 3, 4), &[])], &[]);
        let child_state = state(&[(("new/A.java", 1, 2), ("x", 1, 1), &[])], &[]);
        let files = FileMap::new(&[rename_entry("old/A.java", "new/A.java")]);
        let mut lines = EdgeLines::default();
        lines.insert(
            Some(b"old/A.java".to_vec()),
            Some(b"new/A.java".to_vec()),
            file_lines("a b\nc d\ne f\ng h\n", "a b\nc d\n"),
        );
        let mappings = map_fragments(&parent_state, &child_state, &files, &lines).unwrap();
        assert!(mappings.iter().all(|m| m.renamed));
        assert_eq!(
            child_of(&mappings[0]),
            Some(("new/A.java:1-2".to_string(), ChildKind::Detected))
        );
        // 同じパスの文字列を指す
        assert!(Rc::ptr_eq(
            &mappings[0].child.as_ref().unwrap().0.path,
            &child_state
                .index
                .keys()
                .find(|k| &***k == "new/A.java")
                .unwrap()
                .clone()
        ));
        assert_eq!(mappings[1].child, None);
        assert!(!mappings[1].file_deleted);
    }

    #[test]
    fn a_rename_to_a_path_that_is_not_utf8_stops() {
        let parent_state = state(&[(("a", 1, 1), ("a", 2, 2), &[])], &[]);
        let side = |path: &[u8]| Side {
            mode: 0o100644,
            oid: "1".repeat(40),
            path: path.to_vec(),
        };
        let files = FileMap::new(&[RawEntry {
            status: RawStatus::Renamed { score: 100 },
            old: Some(side(b"a")),
            new: Some(side(b"\xff")),
        }]);
        let mut lines = EdgeLines::default();
        lines.insert(
            Some(b"a".to_vec()),
            Some(b"\xff".to_vec()),
            file_lines("x\ny\n", "x\ny\n"),
        );
        let err = map_fragments(&parent_state, &VersionState::empty(), &files, &lines).unwrap_err();
        assert!(err.to_string().contains("whose path is not UTF-8"), "{err}");
    }

    #[test]
    fn fragments_of_a_deleted_file_are_deleted_with_the_mark() {
        let parent_state = state(&[(("a", 1, 2), ("a", 2, 3), &[])], &[]);
        let side = Side {
            mode: 0o100644,
            oid: "1".repeat(40),
            path: b"a".to_vec(),
        };
        let files = FileMap::new(&[RawEntry {
            status: RawStatus::Deleted,
            old: Some(side),
            new: None,
        }]);
        let mut lines = EdgeLines::default();
        lines.insert(
            Some(b"a".to_vec()),
            None,
            FileLines::deleted(blob("x y\n}\nz\n")),
        );
        let mappings =
            map_fragments(&parent_state, &VersionState::empty(), &files, &lines).unwrap();
        for m in &mappings {
            assert_eq!(m.child, None);
            assert!(m.file_deleted && !m.renamed && !m.similarity_cut);
            assert_eq!(m.counts.retained_lines, 0);
        }
        assert_eq!(
            (
                mappings[0].counts.parent_lines,
                mappings[0].counts.parent_words
            ),
            (2, 2)
        );
        assert_eq!(mappings[0].counts.parent_symbol_lines, 1);
    }

    /// 規則 1 から 3 を，行を 1 行ずつ見る形でそのまま書いたもの
    fn naive(f: &Fragment, child: &VersionState, file: &FileLines) -> Mapping {
        let (old, new, ev) = (&file.old, &file.new, &file.events);
        let dest = |i: u32| destination(ev.old[i as usize]);
        let l: Vec<u32> = f.lines().filter(|&i| !old.is_blank(i)).collect();
        let r: Vec<u32> = l
            .iter()
            .copied()
            .filter(|&i| ev.old[i as usize] != OldLine::Deleted)
            .collect();
        let c = MappingCounts {
            parent_lines: l.len() as u32,
            parent_symbol_lines: l.iter().filter(|&&i| old.is_symbol_only(i)).count() as u32,
            parent_words: l.iter().map(|&i| old.words(i)).sum(),
            retained_lines: r.len() as u32,
            retained_symbol_lines: r.iter().filter(|&&i| old.is_symbol_only(i)).count() as u32,
            retained_words: r.iter().map(|&i| old.words(i)).sum(),
            common_words: r
                .iter()
                .map(|&i| common_words(old.line(i), new.line(dest(i))))
                .sum(),
            modified_lines: r
                .iter()
                .filter(|&&i| matches!(ev.old[i as usize], OldLine::Modified(_)))
                .count() as u32,
            whitespace_lines: r
                .iter()
                .filter(|&&i| matches!(ev.old[i as usize], OldLine::WsOnly(_)))
                .count() as u32,
            ..MappingCounts::default()
        };
        let mapping = |child, cut, counts| Mapping {
            parent: f.clone(),
            child,
            renamed: false,
            similarity_cut: cut,
            file_deleted: false,
            counts,
        };
        if r.is_empty() {
            return mapping(None, false, c);
        }
        let (lo, hi) = (dest(r[0]), dest(*r.last().unwrap()));
        let q = |g: &Fragment| g.lines().map(|j| new.words(j)).sum::<u32>();
        let similar = |g: &Fragment| {
            let (p, q) = (u64::from(c.parent_words), u64::from(q(g)));
            p + q == 0 || 20 * u64::from(c.common_words) >= 3 * (p + q)
        };
        let attrs = |mut c: MappingCounts, g: &Fragment| {
            for j in g.lines().filter(|&j| !new.is_blank(j)) {
                match ev.new[j as usize] {
                    NewLine::Added => c.added_lines += 1,
                    NewLine::Slide(i) | NewLine::WsOnly(i) | NewLine::Modified(i) => {
                        if !(f.start - 1 <= i && i < f.end) {
                            c.taken_in_lines += 1;
                            if matches!(ev.new[j as usize], NewLine::Modified(_)) {
                                c.taken_in_modified_lines += 1;
                            }
                            if matches!(ev.new[j as usize], NewLine::WsOnly(_)) {
                                c.whitespace_lines += 1;
                            }
                        }
                    }
                }
            }
            c.child_words = Some(q(g));
            c
        };
        let candidates: Vec<&Fragment> = child
            .fragments
            .iter()
            .filter(|g| g.path == f.path && g.start - 1 <= lo && hi < g.end)
            .collect();
        if let Some(g) = candidates.iter().min_by_key(|g| (g.end - g.start, g.start))
            && similar(g)
        {
            return mapping(
                Some(((*g).clone(), ChildKind::Detected)),
                false,
                attrs(c, g),
            );
        }
        let projected = Fragment::from_lines(Rc::clone(&f.path), lo..hi + 1);
        let similar_enough = similar(&projected);
        let c = attrs(c, &projected);
        if similar_enough {
            mapping(Some((projected, ChildKind::Projected)), false, c)
        } else {
            mapping(None, true, c)
        }
    }

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

    fn random_fragments(
        rng: &mut Lcg,
        lines: &crate::core::lines::BlobLines,
        count: usize,
    ) -> Vec<(u32, u32)> {
        let n = lines.len();
        let mut out = Vec::new();
        // 試す回数に上限を置くのは，空行ばかりのファイルでは作れる片が足りず，抜けられなくなるからである
        for _ in 0..100 {
            if out.len() == count || n == 0 {
                break;
            }
            let start = rng.below(u64::from(n)) as u32;
            let end = start + rng.below(u64::from(n - start)) as u32;
            if (start..=end).any(|i| !lines.is_blank(i)) && !out.contains(&(start + 1, end + 1)) {
                out.push((start + 1, end + 1));
            }
        }
        out
    }

    #[test]
    fn prefix_sums_agree_with_counting_line_by_line() {
        let vocabulary = [
            "",
            "  ",
            "}",
            "});",
            "int a = 1;",
            "int a = 2;",
            "return a;",
            "  return a;",
            "foo(b, c);",
            "foo(b, d);",
            "x = y + z;",
            "if (a) {",
            "\t}",
            "\"s t\" + 'u'",
        ];
        let mut rng = Lcg(14);
        // 比べた数と結果の種類を数えるのは，乱数の入力が規則のどの道も通ったかを確かめるためである
        let (mut compared, mut detected, mut projected, mut cut, mut deleted) = (0, 0, 0, 0, 0);
        for _ in 0..1500 {
            let n = 3 + rng.below(25) as usize;
            let old: Vec<&str> = (0..n)
                .map(|_| vocabulary[rng.below(vocabulary.len() as u64) as usize])
                .collect();
            let mut new: Vec<String> = Vec::new();
            for line in &old {
                match rng.below(6) {
                    0 => {}
                    1 => {
                        new.push(line.to_string());
                        new.push(
                            vocabulary[rng.below(vocabulary.len() as u64) as usize].to_string(),
                        );
                    }
                    2 => new.push(format!("{line} q")),
                    3 => new.push(format!("  {line}")),
                    _ => new.push(line.to_string()),
                }
            }
            let old_text: String = old.iter().map(|l| format!("{l}\n")).collect();
            let new_text: String = new.iter().map(|l| format!("{l}\n")).collect();
            let file = file_lines(&old_text, &new_text);
            let parents = random_fragments(&mut rng, &file.old, 4);
            let children = random_fragments(&mut rng, &file.new, 4);
            if parents.len() < 2 || children.len() < 2 {
                continue;
            }
            let pairs = |list: &[(u32, u32)]| -> Vec<PairSpec> {
                list.windows(2)
                    .map(|w| (("a", w[0].0, w[0].1), ("a", w[1].0, w[1].1), &[][..]))
                    .collect()
            };
            let parent_state = state(&pairs(&parents), &[]);
            let child_state = state(&pairs(&children), &[]);
            let mut lines = EdgeLines::default();
            lines.insert(
                Some(b"a".to_vec()),
                Some(b"a".to_vec()),
                file_lines(&old_text, &new_text),
            );
            let mappings =
                map_fragments(&parent_state, &child_state, &FileMap::default(), &lines).unwrap();
            for (m, f) in mappings.iter().zip(&parent_state.fragments) {
                assert_eq!(
                    m,
                    &naive(f, &child_state, &file),
                    "old {old_text:?} new {new_text:?}"
                );
                compared += 1;
                match (&m.child, m.similarity_cut) {
                    (Some((_, ChildKind::Detected)), _) => detected += 1,
                    (Some((_, ChildKind::Projected)), _) => projected += 1,
                    (None, true) => cut += 1,
                    (None, false) => deleted += 1,
                }
            }
        }
        eprintln!(
            "compared {compared}: detected {detected}, projected {projected}, cut {cut}, deleted {deleted}"
        );
        assert!(compared >= 3000);
        assert!(detected >= 100 && projected >= 100 && cut >= 20 && deleted >= 20);
    }
}
