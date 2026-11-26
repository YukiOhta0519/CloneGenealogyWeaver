// 足されたファイルと消されたファイルをここで扱わないのは，片側の行が無く，diff を取らないからである．
// 全部の行を追加か削除にするのは，ファイルの対応の側で行う．

use std::ops::Range;

use anyhow::{Result, bail};

use crate::core::lines::{BlobLines, same_without_spaces};
use crate::core::model::Hunk;
use crate::core::similarity::{LineText, pairable};

pub const MAX_HUNK_PAIRS: u64 = 1_000_000;

/// 和を == で比べないのは，足す順で最後の桁がずれ，同じ和を違うとみなしうるからである
const TIE: f64 = 1e-9;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OldLine {
    Slide(u32),
    WsOnly(u32),
    Modified(u32),
    Deleted,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NewLine {
    Slide(u32),
    WsOnly(u32),
    Modified(u32),
    Added,
}

#[derive(Clone, Copy, Debug)]
pub enum Diff<'a> {
    SameBlob,
    Hunks(&'a [Hunk]),
    Failed,
}

#[derive(PartialEq, Eq, Debug)]
pub struct LineEvents {
    pub old: Vec<OldLine>,
    pub new: Vec<NewLine>,
    pub skipped_hunks: u32,
}

pub fn line_events(old: &BlobLines, new: &BlobLines, diff: Diff) -> Result<LineEvents> {
    match diff {
        Diff::SameBlob => Ok(same_blob(old, new)),
        Diff::Failed => Ok(all_changed(old, new)),
        Diff::Hunks(hunks) => from_hunks(old, new, hunks),
    }
}

fn same_blob(old: &BlobLines, new: &BlobLines) -> LineEvents {
    assert_eq!(
        old.len(),
        new.len(),
        "the same blob has different numbers of lines"
    );
    LineEvents {
        old: (0..old.len()).map(OldLine::Slide).collect(),
        new: (0..new.len()).map(NewLine::Slide).collect(),
        skipped_hunks: 0,
    }
}

fn all_changed(old: &BlobLines, new: &BlobLines) -> LineEvents {
    LineEvents {
        old: vec![OldLine::Deleted; old.len() as usize],
        new: vec![NewLine::Added; new.len() as usize],
        skipped_hunks: 0,
    }
}

fn from_hunks(old: &BlobLines, new: &BlobLines, hunks: &[Hunk]) -> Result<LineEvents> {
    let ranges = hunk_ranges(hunks, old.len(), new.len())?;
    let mut events = all_changed(old, new);
    let (mut i, mut j) = (0, 0);
    for (hunk, (old_range, new_range)) in hunks.iter().zip(ranges) {
        // j が新行の数を超えるかを見ないのは，hunk_ranges で旧行と新行の数が同じことを確かめたからである
        while i < old_range.start {
            slide(old, new, i, j, &mut events)
                .map_err(|e| e.context(format!("before hunk {hunk}")))?;
            i += 1;
            j += 1;
        }
        (i, j) = (old_range.end, new_range.end);
        pair_in_hunk(old, new, old_range, new_range, &mut events);
    }
    while i < old.len() {
        slide(old, new, i, j, &mut events)?;
        i += 1;
        j += 1;
    }
    Ok(events)
}

fn slide(old: &BlobLines, new: &BlobLines, i: u32, j: u32, events: &mut LineEvents) -> Result<()> {
    let (x, y) = (old.line(i), new.line(j));
    if x == y {
        events.old[i as usize] = OldLine::Slide(j);
        events.new[j as usize] = NewLine::Slide(i);
    } else if same_without_spaces(x, y) {
        events.old[i as usize] = OldLine::WsOnly(j);
        events.new[j as usize] = NewLine::WsOnly(i);
    } else {
        bail!(
            "Git output is inconsistent: old line {} and new line {} are outside the hunks \
             but differ beyond whitespace",
            i + 1,
            j + 1
        );
    }
    Ok(())
}

/// Git の出力を確かめずに使わないのは，壊れた出力で黙って違う行イベントを書かないためである．
/// hunk の外の行が空白を除いて同じかをここで見ないのは，slide が行を比べるときに一緒に見るからである．
fn hunk_ranges(
    hunks: &[Hunk],
    old_len: u32,
    new_len: u32,
) -> Result<Vec<(Range<u32>, Range<u32>)>> {
    let mut ranges = Vec::with_capacity(hunks.len());
    let (mut old_end, mut new_end) = (0u64, 0u64);
    for hunk in hunks {
        let (Some(o), Some(n)) = (
            side_range(hunk.old_start, hunk.old_count),
            side_range(hunk.new_start, hunk.new_count),
        ) else {
            bail!("Git output is inconsistent: hunk {hunk} starts at line 0 with lines in it");
        };
        if o.start < old_end || n.start < new_end {
            bail!(
                "Git output is inconsistent: hunk {hunk} is out of order or overlaps the previous hunk"
            );
        }
        if o.end > u64::from(old_len) || n.end > u64::from(new_len) {
            bail!(
                "Git output is inconsistent: hunk {hunk} goes beyond the end of the files \
                 of {old_len} old and {new_len} new lines"
            );
        }
        if o.start - old_end != n.start - new_end {
            bail!(
                "Git output is inconsistent: the numbers of old and new lines before hunk {hunk} differ"
            );
        }
        (old_end, new_end) = (o.end, n.end);
        // try_from で確かめないのは，行の数を超えないことを確かめたので u32 に収まるからである
        ranges.push((o.start as u32..o.end as u32, n.start as u32..n.end as u32));
    }
    if u64::from(old_len) - old_end != u64::from(new_len) - new_end {
        bail!(
            "Git output is inconsistent: the numbers of old and new lines after the last hunk differ"
        );
    }
    Ok(ranges)
}

/// u32 で返さないのは，Git の数が u32 の最大に近くても，開始と数を足して溢れないようにするためである
fn side_range(start: u32, count: u32) -> Option<Range<u64>> {
    let (start, count) = (u64::from(start), u64::from(count));
    if count == 0 {
        Some(start..start)
    } else if start == 0 {
        None
    } else {
        Some(start - 1..start - 1 + count)
    }
}

fn pair_in_hunk(
    old: &BlobLines,
    new: &BlobLines,
    old_range: Range<u32>,
    new_range: Range<u32>,
    events: &mut LineEvents,
) {
    let olds: Vec<u32> = old_range.filter(|&i| !old.is_blank(i)).collect();
    let news: Vec<u32> = new_range.filter(|&j| !new.is_blank(j)).collect();
    if olds.is_empty() || news.is_empty() {
        return;
    }
    if olds.len() as u64 * news.len() as u64 > MAX_HUNK_PAIRS {
        // 溢れを確かめないのは，hunk の数が行の数を超えず，u32 に収まるからである
        events.skipped_hunks += 1;
        return;
    }
    for (i, j) in choose_pairs(old, new, &olds, &news) {
        let (x, y) = (old.line(i), new.line(j));
        let (o, n) = if x == y {
            (OldLine::Slide(j), NewLine::Slide(i))
        } else if same_without_spaces(x, y) {
            (OldLine::WsOnly(j), NewLine::WsOnly(i))
        } else {
            (OldLine::Modified(j), NewLine::Modified(i))
        };
        events.old[i as usize] = o;
        events.new[j as usize] = n;
    }
}

/// 組にできない類似度を Option でなく 0 で持つのは，表の升を 8 バイトにして，
/// 上限の 1,000,000 組でも表 2 つを 16 MB ほどに収めるためである．
/// 組にできる類似度は 0.5 以上なので，0 と取り違えない．
fn choose_pairs(old: &BlobLines, new: &BlobLines, olds: &[u32], news: &[u32]) -> Vec<(u32, u32)> {
    let old_texts: Vec<LineText> = olds.iter().map(|&i| LineText::new(old.line(i))).collect();
    let new_texts: Vec<LineText> = news.iter().map(|&j| LineText::new(new.line(j))).collect();
    let (p, q) = (olds.len(), news.len());

    let mut sims = vec![0.0f64; p * q];
    for (a, x) in old_texts.iter().enumerate() {
        for (b, y) in new_texts.iter().enumerate() {
            if let Some(value) = pairable(x, y) {
                sims[a * q + b] = value;
            }
        }
    }

    let width = q + 1;
    let mut best = vec![0.0f64; (p + 1) * width];
    for a in (0..p).rev() {
        for b in (0..q).rev() {
            let mut value = best[(a + 1) * width + b].max(best[a * width + b + 1]);
            let sim = sims[a * q + b];
            if sim > 0.0 {
                value = value.max(sim + best[(a + 1) * width + b + 1]);
            }
            best[a * width + b] = value;
        }
    }

    let mut pairs = Vec::new();
    let (mut a, mut b) = (0, 0);
    while a < p && b < q {
        let here = best[a * width + b];
        let sim = sims[a * q + b];
        if sim > 0.0 && same(here, sim + best[(a + 1) * width + b + 1]) {
            pairs.push((olds[a], news[b]));
            a += 1;
            b += 1;
        } else if same(here, best[(a + 1) * width + b]) {
            a += 1;
        } else {
            b += 1;
        }
    }
    pairs
}

fn same(x: f64, y: f64) -> bool {
    (x - y).abs() < TIE
}

#[cfg(test)]
mod tests {
    use super::*;
    use NewLine::Added;
    use OldLine::Deleted;

    fn blob(text: &str) -> BlobLines {
        BlobLines::new(text.as_bytes().to_vec())
    }

    fn hunk(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> Hunk {
        Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
        }
    }

    fn events(old: &str, new: &str, hunks: &[Hunk]) -> LineEvents {
        line_events(&blob(old), &blob(new), Diff::Hunks(hunks)).unwrap()
    }

    fn error(old: &str, new: &str, hunks: &[Hunk]) -> String {
        let err = line_events(&blob(old), &blob(new), Diff::Hunks(hunks)).unwrap_err();
        format!("{err:#}")
    }

    #[test]
    fn the_same_blob_slides_every_line() {
        let lines = blob("a\n\nb\n");
        let result = line_events(&lines, &lines, Diff::SameBlob).unwrap();
        assert_eq!(
            result.old,
            [OldLine::Slide(0), OldLine::Slide(1), OldLine::Slide(2)]
        );
        assert_eq!(
            result.new,
            [NewLine::Slide(0), NewLine::Slide(1), NewLine::Slide(2)]
        );
        assert_eq!(result.skipped_hunks, 0);
    }

    #[test]
    fn a_failed_diff_deletes_and_adds_every_line() {
        let result = line_events(&blob("a\nb\n"), &blob("a\n"), Diff::Failed).unwrap();
        assert_eq!(result.old, [Deleted, Deleted]);
        assert_eq!(result.new, [Added]);
        assert_eq!(result.skipped_hunks, 0);
    }

    #[test]
    fn ties_follow_the_example_of_the_spec() {
        // 旧行 aa ab と新行 bb ba では，1 つ目の旧行を飛ばし，2 つ目の旧行と 1 つ目の新行を組む
        let result = events("aa\nab\n", "bb\nba\n", &[hunk(1, 2, 1, 2)]);
        assert_eq!(result.old, [Deleted, OldLine::Modified(0)]);
        assert_eq!(result.new, [NewLine::Modified(1), Added]);
    }

    #[test]
    fn ties_pair_the_first_closing_brace() {
        // 旧行 } } と新行 } では，1 つ目の } と組む
        let result = events("}\n}\n", "}\n", &[hunk(1, 2, 1, 1)]);
        assert_eq!(result.old, [OldLine::Slide(0), Deleted]);
        assert_eq!(result.new, [NewLine::Slide(0)]);
    }

    #[test]
    fn two_pairs_of_one_half_beat_one_pair_of_nine_tenths() {
        let (a, b) = ("abcdefghij", "VWXYZfghiX");
        let (c, d) = ("abcdeVWXYZ", "abcdefghiX");
        let s =
            |x: &str, y: &str| pairable(&LineText::new(x.as_bytes()), &LineText::new(y.as_bytes()));
        assert_eq!(s(a, c), Some(0.5));
        assert_eq!(s(b, d), Some(0.5));
        assert_eq!(s(a, d), Some(1.0 - 1.0 / 10.0));
        assert_eq!(s(b, c), None);
        let result = events(
            &format!("{a}\n{b}\n"),
            &format!("{c}\n{d}\n"),
            &[hunk(1, 2, 1, 2)],
        );
        assert_eq!(result.old, [OldLine::Modified(0), OldLine::Modified(1)]);
        assert_eq!(result.new, [NewLine::Modified(0), NewLine::Modified(1)]);
    }

    #[test]
    fn pairs_in_a_hunk_are_slides_whitespace_changes_or_modifications() {
        let result = events(
            "x\nint a;\n  if (b) {\nfoo(1);\n",
            "x\nint a;\nif (b) {\nfoo(2);\n",
            &[hunk(2, 3, 2, 3)],
        );
        assert_eq!(
            result.old,
            [
                OldLine::Slide(0),
                OldLine::Slide(1),
                OldLine::WsOnly(2),
                OldLine::Modified(3)
            ]
        );
        assert_eq!(
            result.new,
            [
                NewLine::Slide(0),
                NewLine::Slide(1),
                NewLine::WsOnly(2),
                NewLine::Modified(3)
            ]
        );
    }

    #[test]
    fn blank_lines_in_a_hunk_are_not_paired() {
        let result = events("a1\n\nb1\n", "a2\n  \nb2\n", &[hunk(1, 3, 1, 3)]);
        assert_eq!(
            result.old,
            [OldLine::Modified(0), Deleted, OldLine::Modified(2)]
        );
        assert_eq!(
            result.new,
            [NewLine::Modified(0), Added, NewLine::Modified(2)]
        );
        // 片側が空行だけなら，組を作らず，大きすぎる hunk にも数えない
        let result = events("x\n", "\n", &[hunk(1, 1, 1, 1)]);
        assert_eq!(result.old, [Deleted]);
        assert_eq!(result.new, [Added]);
        assert_eq!(result.skipped_hunks, 0);
    }

    #[test]
    fn hunks_over_one_million_pairs_are_skipped() {
        // 空行を除いた行の数の積で数える．空行は 1,000 行の外に足す
        let lines = |line: &str, n: usize| format!("{}\n\n", line.repeat(n).trim_end());
        let old_1000 = lines("{\n", 1000);
        let old_1001 = lines("{\n", 1001);
        let new_1000 = lines("abc\n", 1000);
        let at_limit = events(&old_1000, &new_1000, &[hunk(1, 1001, 1, 1001)]);
        assert_eq!(at_limit.skipped_hunks, 0);
        assert!(at_limit.old[..1000].iter().all(|e| *e == Deleted));
        let over = events(&old_1001, &new_1000, &[hunk(1, 1002, 1, 1001)]);
        assert_eq!(over.skipped_hunks, 1);
        assert!(over.old.iter().all(|e| *e == Deleted));
        assert!(over.new.iter().all(|e| *e == Added));
        // 組にできる行があっても，上限を超えれば組にしない
        let over = events(
            &format!("abd\n{old_1001}"),
            &new_1000,
            &[hunk(1, 1003, 1, 1001)],
        );
        assert_eq!(over.old[0], Deleted);
        assert_eq!(over.skipped_hunks, 1);
    }

    #[test]
    fn lines_outside_hunks_slide_one_to_one() {
        let result = events(
            "a\nb\nc\nd\ne\n",
            "a\nX\nY\nc\ne\n",
            &[hunk(2, 1, 2, 2), hunk(4, 1, 4, 0)],
        );
        assert_eq!(
            result.old,
            [
                OldLine::Slide(0),
                Deleted,
                OldLine::Slide(3),
                Deleted,
                OldLine::Slide(4)
            ]
        );
        assert_eq!(
            result.new,
            [
                NewLine::Slide(0),
                Added,
                Added,
                NewLine::Slide(2),
                NewLine::Slide(4)
            ]
        );
    }

    #[test]
    fn whitespace_changes_outside_hunks_are_whitespace_only() {
        // 字下げだけ変わった行と，CRLF から LF に変わった行
        let result = events("  a\nb\r\nc\n", "a\nb\nc\n", &[]);
        assert_eq!(
            result.old,
            [OldLine::WsOnly(0), OldLine::WsOnly(1), OldLine::Slide(2)]
        );
        assert_eq!(
            result.new,
            [NewLine::WsOnly(0), NewLine::WsOnly(1), NewLine::Slide(2)]
        );
    }

    #[test]
    fn zero_counts_follow_the_rule_of_u0() {
        // 旧行の数が 0 なら，旧行の開始の行の後への挿入である
        let ranges = hunk_ranges(&[hunk(3, 0, 4, 2)], 4, 6).unwrap();
        assert_eq!(ranges, [(3..3, 3..5)]);
        let ranges = hunk_ranges(&[hunk(0, 0, 1, 2)], 1, 3).unwrap();
        assert_eq!(ranges, [(0..0, 0..2)]);
        // 新行の数が 0 なら，新行の開始の行の後での削除である
        let ranges = hunk_ranges(&[hunk(3, 2, 2, 0)], 5, 3).unwrap();
        assert_eq!(ranges, [(2..4, 2..2)]);
        let ranges = hunk_ranges(&[hunk(1, 2, 0, 0)], 2, 0).unwrap();
        assert_eq!(ranges, [(0..2, 0..0)]);
        // 挿入の hunk の範囲に直した結果で，行イベントを求める
        let result = events("a\nb\n", "a\nX\nb\n", &[hunk(1, 0, 2, 1)]);
        assert_eq!(result.old, [OldLine::Slide(0), OldLine::Slide(2)]);
        assert_eq!(result.new, [NewLine::Slide(0), Added, NewLine::Slide(1)]);
    }

    #[test]
    fn headers_printed_by_git_2_43_0() {
        // 2026-11-14 に Git 2.43.0 で，旧の a b c と新の各ファイルを，ツールと同じ引数で diff にかけた見出し．
        // 数の無い所は 1 にした
        let base = "a\nb\nc\n";
        let cases = [
            // @@ -0,0 +1,2 @@
            ("X\nY\na\nb\nc\n", hunk(0, 0, 1, 2), "SSS", "AASSS"),
            // @@ -1,0 +2 @@ a
            ("a\nX\nb\nc\n", hunk(1, 0, 2, 1), "SSS", "SASS"),
            // @@ -1,2 +0,0 @@
            ("c\n", hunk(1, 2, 0, 0), "DDS", "S"),
            // @@ -2 +1,0 @@ a
            ("a\nc\n", hunk(2, 1, 1, 0), "SDS", "SS"),
            // @@ -3 +2,0 @@ b
            ("a\nb\n", hunk(3, 1, 2, 0), "SSD", "SS"),
            // @@ -2 +2 @@ a
            ("a\nB\nc\n", hunk(2, 1, 2, 1), "SDS", "SAS"),
        ];
        for (new, h, old_kinds, new_kinds) in cases {
            let result = events(base, new, &[h]);
            let old_found: String = result
                .old
                .iter()
                .map(|e| if *e == Deleted { 'D' } else { 'S' })
                .collect();
            let new_found: String = result
                .new
                .iter()
                .map(|e| if *e == Added { 'A' } else { 'S' })
                .collect();
            assert_eq!(
                (old_found.as_str(), new_found.as_str()),
                (old_kinds, new_kinds),
                "{h}"
            );
        }
    }

    #[test]
    fn hunks_out_of_order_or_overlapping_are_errors() {
        let text = "a\nb\nc\nd\n";
        let other = "A\nb\nC\nd\n";
        let message = error(text, other, &[hunk(3, 1, 3, 1), hunk(1, 1, 1, 1)]);
        assert!(
            message.contains("hunk @@ -1,1 +1,1 @@ is out of order"),
            "{message}"
        );
        let message = error(text, other, &[hunk(1, 3, 1, 3), hunk(3, 1, 3, 1)]);
        assert!(message.contains("overlaps"), "{message}");
    }

    #[test]
    fn hunks_beyond_the_files_are_errors() {
        let message = error("a\nb\nc\n", "a\nb\nC\n", &[hunk(3, 2, 3, 1)]);
        assert!(message.contains("goes beyond the end"), "{message}");
        // u32 の最大に近い数でも溢れずに止まる
        let message = error("a\n", "b\n", &[hunk(u32::MAX, u32::MAX, 1, 1)]);
        assert!(message.contains("goes beyond the end"), "{message}");
        let message = error("a\n", "b\n", &[hunk(0, 1, 1, 1)]);
        assert!(message.contains("starts at line 0"), "{message}");
    }

    #[test]
    fn different_numbers_of_lines_outside_hunks_are_errors() {
        let message = error("a\nb\nc\n", "B\nc\n", &[hunk(2, 1, 1, 1)]);
        assert!(
            message.contains("before hunk @@ -2,1 +1,1 @@ differ"),
            "{message}"
        );
        let message = error("a\nb\nc\n", "A\nX\nb\n", &[hunk(1, 1, 1, 2)]);
        assert!(message.contains("after the last hunk differ"), "{message}");
    }

    #[test]
    fn different_lines_outside_hunks_are_errors() {
        let message = error("a\nb\n", "a\nc\n", &[]);
        assert!(message.contains("old line 2 and new line 2"), "{message}");
        let message = error("a\nb\nc\n", "A\nb\nC\n", &[hunk(3, 1, 3, 1)]);
        assert!(message.contains("before hunk @@ -3,1 +3,1 @@"), "{message}");
        assert!(message.contains("old line 1 and new line 1"), "{message}");
        // 垂直タブは空白に数えないので，それだけの違いも誤りになる
        let message = error("a\x0b\n", "a\n", &[]);
        assert!(message.contains("differ beyond whitespace"), "{message}");
    }
}
