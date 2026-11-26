use std::cmp::Ordering;

use crate::core::lines::{strip_spaces, words_in};

pub const MAX_CHARS: usize = 1000;

/// 組ごとに文字の並びを作り直さないのは，1,000,000 組の hunk で，
/// 同じ行を相手の行の数だけ読み直さないためである．
pub struct LineText {
    stripped: Vec<u8>,
    chars: Option<Vec<char>>,
}

impl LineText {
    /// UTF-8 として読んでから空白を除かないのは，バイト列から空白を除いてから読む決まりに合わせるためである．
    /// 読めないバイトの並びの間に空白がある行では，置換文字の数が違いうる．
    pub fn new(line: &[u8]) -> LineText {
        let stripped = strip_spaces(line);
        // 1,001 文字より先を読まないのは，0 にすると決まった長い行を全部写さないためである
        let chars: Vec<char> = String::from_utf8_lossy(&stripped)
            .chars()
            .take(MAX_CHARS + 1)
            .collect();
        let chars = (chars.len() <= MAX_CHARS).then_some(chars);
        LineText { stripped, chars }
    }
}

/// 0.5 以上かを浮動小数点で比べないのは，読む人が丸めを確かめずに済むようにするためである．
/// 1 − d ÷ m ≥ 0.5 は 2d ≤ m と同じである．
/// 編集距離を最後まで求めないのは，組にできるかには d が m ÷ 2 以下かだけが要るからである．
pub fn pairable(x: &LineText, y: &LineText) -> Option<f64> {
    if x.stripped == y.stripped {
        return Some(1.0);
    }
    let (Some(a), Some(b)) = (&x.chars, &y.chars) else {
        return None;
    };
    // m が 0 かを確かめないのは，空白を除いたバイト列が違えば，どちらかに 1 文字以上あるからである
    let m = a.len().max(b.len());
    let d = bounded_distance(a, b, m / 2)?;
    Some(1.0 - d as f64 / m as f64)
}

fn bounded_distance(a: &[char], b: &[char], limit: usize) -> Option<usize> {
    let (a, b) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let (n, m) = (a.len(), b.len());
    if m - n > limit {
        return None;
    }
    if n == 0 {
        return Some(m);
    }
    let over = limit + 1;
    let mut prev: Vec<usize> = (0..=m).map(|j| if j <= limit { j } else { over }).collect();
    let mut cur = vec![over; m + 1];
    for i in 1..=n {
        let low = i.saturating_sub(limit);
        let high = (i + limit).min(m);
        // 入れ直さないと，前の前の行の値が幅の左に残る
        if low == 0 {
            cur[0] = i;
        } else {
            cur[low - 1] = over;
        }
        let mut row_min = if low == 0 { i } else { over };
        for j in low.max(1)..=high {
            // prev[high] を入れ直さないのは，どの行も一度も書いていない升で，over のままだからである
            let replace = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            let value = replace.min(prev[j] + 1).min(cur[j - 1] + 1).min(over);
            cur[j] = value;
            row_min = row_min.min(value);
        }
        if row_min > limit {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    // prev[m] が幅の外かを確かめないのは，長さの差が limit 以下だからである
    let d = prev[m];
    (d <= limit).then_some(d)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WordCounts {
    pub parent: u32,
    pub child: u32,
    pub common: u32,
}

impl WordCounts {
    /// 式の順を変えないのは，fragment_mapping の表の CHECK が，SQLite の中で
    /// `2.0 * common_words / (parent_words + child_words)` を同じ順に求めるからである．
    /// 順を変えると最後の桁がずれ，CHECK で拒まれうる．
    /// 和を u32 にしないのは，溢れるからである．2 の 53 乗より小さいので，f64 に直しても変わらない．
    pub fn similarity(&self) -> f64 {
        let sum = u64::from(self.parent) + u64::from(self.child);
        if sum == 0 {
            1.0
        } else {
            (2.0 * f64::from(self.common)) / (sum as f64)
        }
    }

    /// similarity と 0.3 を比べないのは，浮動小数点の丸めで判定が揺れないようにするためである．
    /// u32 のままにしないのは，語の数が億の桁の片で溢れるからである．
    pub fn is_cut(&self) -> bool {
        20 * u64::from(self.common) < 3 * (u64::from(self.parent) + u64::from(self.child))
    }
}

pub fn common_words(old_line: &[u8], new_line: &[u8]) -> u32 {
    let mut old: Vec<&[u8]> = words_in(old_line).collect();
    let mut new: Vec<&[u8]> = words_in(new_line).collect();
    old.sort_unstable();
    new.sort_unstable();
    let (mut i, mut j, mut common) = (0, 0, 0u32);
    while i < old.len() && j < new.len() {
        match old[i].cmp(new[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    common
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &str) -> LineText {
        LineText::new(line.as_bytes())
    }

    fn sim(x: &str, y: &str) -> Option<f64> {
        pairable(&text(x), &text(y))
    }

    /// 全部の升を埋める素直な編集距離
    fn naive_distance(a: &[char], b: &[char]) -> usize {
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for i in 1..=a.len() {
            let mut cur = vec![i; b.len() + 1];
            for j in 1..=b.len() {
                let replace = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
                cur[j] = replace.min(prev[j] + 1).min(cur[j - 1] + 1);
            }
            prev = cur;
        }
        prev[b.len()]
    }

    /// 乱数の crate を使わないのは，依存の crate をテストのためにも増やさないからである
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> usize {
            (self.next() % n) as usize
        }
    }

    #[test]
    fn short_lines_are_not_special() {
        // `}` と `});` は 0.33 で組にならず，`}` と `},` は 0.5 で組になる
        assert_eq!(sim("}", "});"), None);
        assert_eq!(sim("}", "},"), Some(0.5));
        assert_eq!(sim("  }", "},  "), Some(0.5));
    }

    #[test]
    fn lines_equal_without_spaces_are_1_at_any_length() {
        let x = "a".repeat(1001);
        let y = "a ".repeat(1001);
        assert_eq!(sim(&x, &y), Some(1.0));
        assert_eq!(sim("if (x) {", "if(x){\r"), Some(1.0));
    }

    #[test]
    fn different_lines_over_1000_characters_are_0() {
        let x = "a".repeat(1001);
        let y = format!("{}b", "a".repeat(1000));
        assert_eq!(sim(&x, &y), None);
        // 空白を除いて 1,000 文字ちょうどなら求める
        let x = "a".repeat(1000);
        let y = format!("{}b", "a".repeat(999));
        assert_eq!(sim(&x, &y), Some(1.0 - 1.0 / 1000.0));
    }

    #[test]
    fn characters_are_unicode_scalar_values() {
        // 結合文字も 1 文字に数える．e と結合アクセントの 2 文字と e は，距離 1，長さ 2
        assert_eq!(sim("e\u{301}", "e"), Some(0.5));
        assert_eq!(sim("日本", "日米"), Some(0.5));
    }

    #[test]
    fn unreadable_bytes_become_one_replacement_character_per_chunk() {
        let line = LineText::new(b"a\xf0\x9f\x98b\xffc");
        assert_eq!(line.chars.as_deref(), Some(&['a', '�', 'b', '�', 'c'][..]));
        // バイト列が違えば，文字の並びが同じでも 1 にする
        let x = LineText::new(b"ab\xff");
        let y = LineText::new(b"ab\xfe");
        assert_eq!(pairable(&x, &y), Some(1.0));
    }

    #[test]
    fn an_empty_line_is_not_paired_with_a_line_of_characters() {
        assert_eq!(sim("", "a"), None);
        assert_eq!(sim("", ""), Some(1.0));
    }

    #[test]
    fn bounded_distance_agrees_with_the_naive_one() {
        let mut rng = Lcg(20261114);
        let alphabet = ['a', 'b', 'c', '日'];
        let word = |rng: &mut Lcg| -> Vec<char> {
            let len = rng.below(13);
            (0..len).map(|_| alphabet[rng.below(4)]).collect()
        };
        for _ in 0..20_000 {
            let a = word(&mut rng);
            let b = word(&mut rng);
            let d = naive_distance(&a, &b);
            for limit in 0..=13 {
                let expected = (d <= limit).then_some(d);
                assert_eq!(
                    bounded_distance(&a, &b, limit),
                    expected,
                    "a = {a:?}, b = {b:?}, limit = {limit}"
                );
            }
            // 組にできるかと類似度も，素直な計算と同じになる
            let x: String = a.iter().collect();
            let y: String = b.iter().collect();
            let m = a.len().max(b.len());
            let expected = if a == b {
                Some(1.0)
            } else if 2 * d <= m {
                Some(1.0 - d as f64 / m as f64)
            } else {
                None
            };
            assert_eq!(sim(&x, &y), expected, "x = {x:?}, y = {y:?}");
        }
    }

    #[test]
    fn similarity_of_fragments() {
        let w = |parent, child, common| WordCounts {
            parent,
            child,
            common,
        };
        assert_eq!(w(10, 12, 8).similarity(), 2.0 * 8.0 / 22.0);
        // 両方 0 語なら 1 で，切らない
        assert_eq!(w(0, 0, 0).similarity(), 1.0);
        assert!(!w(0, 0, 0).is_cut());
        // 0.3 ちょうどは切らず，下回れば切る
        assert!(!w(10, 10, 3).is_cut());
        assert!(w(10, 11, 3).is_cut());
        assert!(w(5, 0, 0).is_cut());
    }

    #[test]
    fn word_counts_do_not_overflow_near_the_u32_maximum() {
        let max = u32::MAX;
        let all = WordCounts {
            parent: max,
            child: max,
            common: max,
        };
        assert_eq!(all.similarity(), 1.0);
        assert!(!all.is_cut());
        let none = WordCounts {
            parent: max,
            child: max,
            common: 0,
        };
        assert_eq!(none.similarity(), 0.0);
        assert!(none.is_cut());
        // u32 では溢れる大きさでの，0.3 の境目．
        // 和は 8,589,934,589 で，3 × 和 は 25,769,803,767 である．
        // 共通が 1,288,490,189 なら 20 × 共通 は 25,769,803,780 で切らず，1 少なければ切る
        let at = |common| WordCounts {
            parent: max,
            child: max - 1,
            common,
        };
        assert!(!at(1_288_490_189).is_cut());
        assert!(at(1_288_490_188).is_cut());
    }

    #[test]
    fn common_words_take_the_smaller_count() {
        assert_eq!(common_words(b"a a b", b"a b b"), 2);
        assert_eq!(common_words(b"return result;", b"return value;"), 1);
        assert_eq!(common_words(b"f(\"x y\")", b"g(\"x y\", x)"), 1);
        assert_eq!(common_words(b"}", b"}"), 0);
        // 語はバイト列で比べる
        assert_eq!(common_words(b"Abc", b"abc"), 0);
    }

    /// 片の類似度が，fragment_mapping の表の CHECK と同じ値になるか．
    /// tests/ からは core の関数を呼べないので，ここで表に書いて確かめる．
    /// 外部キーは見ないので，ほかの表の行は作らない．
    #[test]
    fn similarity_is_accepted_by_the_check_of_fragment_mapping() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../schema/events.sql"))
            .unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        let mut rng = Lcg(7);
        let mut cases = vec![
            (0, 0, 0),
            (10, 10, 3),
            (10, 11, 3),
            (1, 2, 1),
            (3, 7, 1),
            (u32::MAX, u32::MAX, u32::MAX),
            (u32::MAX, u32::MAX - 1, 1),
            (u32::MAX, 0, 12_345),
        ];
        for _ in 0..5_000 {
            let parent = rng.next() as u32 >> rng.below(32);
            let child = rng.next() as u32 >> rng.below(32);
            let common = (rng.next() as u32 >> rng.below(32)).min(parent).min(child);
            cases.push((parent, child, common));
        }
        let mut insert = conn
            .prepare(
                "INSERT INTO fragment_mapping (edge, parent_fragment, child_fragment, child_kind,
                   renamed, event, similarity_cut, file_deleted, parent_lines, parent_symbol_lines,
                   parent_words, retained_lines, retained_symbol_lines, retained_words, common_words,
                   modified_lines, whitespace_lines, taken_in_lines, taken_in_modified_lines,
                   added_lines, child_words, text_similarity)
                 VALUES (1, ?1, ?2, ?3, 0, ?4, ?5, ?6, 5, 0, ?7, 5, 0, ?8, ?8,
                   1, 0, 0, 0, 0, ?9, ?10)",
            )
            .unwrap();
        for (id, &(parent, child, common)) in cases.iter().enumerate() {
            let counts = WordCounts {
                parent,
                child,
                common,
            };
            let cut = counts.is_cut();
            // 切った片は Deleted で，子の片を書かない
            let (child_fragment, kind, event, file_deleted) = if cut {
                (None, None, "Deleted", Some(0))
            } else {
                (Some(2), Some("detected"), "Modified", None)
            };
            insert
                .execute(rusqlite::params![
                    id as i64 + 1,
                    child_fragment,
                    kind,
                    event,
                    cut,
                    file_deleted,
                    parent,
                    common,
                    child,
                    counts.similarity(),
                ])
                .unwrap_or_else(|e| panic!("{counts:?} was rejected: {e}"));
        }
    }
}
