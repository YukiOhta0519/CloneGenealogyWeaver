// core のテストの道具．
//
// Git の hunk の代わりに最長共通部分列で hunk を作るのは，core のテストで Git を使わないためである．
// 結果は Git の histogram と違いうるので，テストは diff の決まり方に頼らない中身で書く．

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::core::file_mapping::FileLines;
use crate::core::line_events::{Diff, line_events};
use crate::core::lines::{BlobLines, strip_spaces};
use crate::core::model::{Fragment, Hunk, Pair, PairInfo, Scope, VersionState};

pub fn blob(text: &str) -> Rc<BlobLines> {
    Rc::new(BlobLines::new(text.as_bytes().to_vec()))
}

pub fn fragment(path: &str, start: u32, end: u32) -> Fragment {
    Fragment {
        path: Rc::from(path),
        start,
        end,
    }
}

/// 空白を除いて同じ行を最長共通部分列でそろえ，そろわなかった所を -U0 の hunk にする
pub fn hunks(old: &BlobLines, new: &BlobLines) -> Vec<Hunk> {
    let a: Vec<Vec<u8>> = (0..old.len()).map(|i| strip_spaces(old.line(i))).collect();
    let b: Vec<Vec<u8>> = (0..new.len()).map(|j| strip_spaces(new.line(j))).collect();
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut hunks = Vec::new();
    let (mut i, mut j) = (0, 0);
    let (mut start_i, mut start_j) = (0, 0);
    let flush = |hunks: &mut Vec<Hunk>, si: usize, sj: usize, i: usize, j: usize| {
        let (b, d) = ((i - si) as u32, (j - sj) as u32);
        if b > 0 || d > 0 {
            hunks.push(Hunk {
                old_start: if b == 0 { si as u32 } else { si as u32 + 1 },
                old_count: b,
                new_start: if d == 0 { sj as u32 } else { sj as u32 + 1 },
                new_count: d,
            });
        }
    };
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            flush(&mut hunks, start_i, start_j, i, j);
            i += 1;
            j += 1;
            start_i = i;
            start_j = j;
        } else if j < m && (i == n || lcs[i][j + 1] >= lcs[i + 1][j]) {
            j += 1;
        } else {
            i += 1;
        }
    }
    flush(&mut hunks, start_i, start_j, i, j);
    hunks
}

/// 古い中身と新しい中身から，ファイルの組の行の情報を作る．同じ中身なら同じ blob として扱う
pub fn file_lines(old_text: &str, new_text: &str) -> FileLines {
    let (old, new) = (blob(old_text), blob(new_text));
    let events = if old_text == new_text {
        line_events(&old, &new, Diff::SameBlob)
    } else {
        line_events(&old, &new, Diff::Hunks(&hunks(&old, &new)))
    }
    .unwrap();
    FileLines { old, new, events }
}

/// 片を (パス, 開始行, 終了行) で書く
pub type At<'a> = (&'a str, u32, u32);

/// ペアの 2 つの片と，プラグインの順のラベル
pub type PairSpec<'a> = (At<'a>, At<'a>, &'a [&'static str]);

/// ペアの片を (パス, 開始行, 終了行) で並べ，ラベルを振って版の状態を作る
pub fn state(pairs: &[PairSpec], scope: &[&str]) -> VersionState {
    let pairs: BTreeMap<Pair, PairInfo> = pairs
        .iter()
        .map(|(a, b, labels)| {
            let pair = Pair::new(fragment(a.0, a.1, a.2), fragment(b.0, b.1, b.2)).unwrap();
            (
                pair,
                PairInfo {
                    similarity: None,
                    labels: labels.to_vec(),
                },
            )
        })
        .collect();
    let scope = Scope(scope.iter().map(|p| p.as_bytes().to_vec()).collect());
    VersionState::new(pairs, scope)
}

#[test]
fn hunks_from_the_longest_common_subsequence_are_valid_for_line_events() {
    let lines = file_lines("a\nb\nc\nd\n", "a\nX\nc\n  d\ne\n");
    use crate::core::line_events::{NewLine, OldLine};
    assert_eq!(
        lines.events.old,
        [
            OldLine::Slide(0),
            OldLine::Deleted,
            OldLine::Slide(2),
            OldLine::WsOnly(3)
        ]
    );
    assert_eq!(lines.events.new[4], NewLine::Added);
}
