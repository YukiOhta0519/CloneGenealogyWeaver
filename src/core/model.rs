use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::ops::Range;
use std::rc::Rc;

use anyhow::{Result, bail};

/// 比べ方を手で書かずに derive にするのは，項目の順がそのまま 3 つ組の順になるからである．
/// str の比べ方は UTF-8 のバイト列の比べ方なので，パスをバイト列の順に比べる決まりとも合う．
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Fragment {
    /// String にしないのは，25 万ペアの版でも，パスの文字列を片の数だけ写さないためである
    pub path: Rc<str>,
    pub start: u32,
    pub end: u32,
}

impl Fragment {
    pub fn lines(&self) -> Range<u32> {
        assert!(
            1 <= self.start && self.start <= self.end,
            "fragment {self} has an invalid line range"
        );
        self.start - 1..self.end
    }

    pub fn from_lines(path: Rc<str>, lines: Range<u32>) -> Fragment {
        assert!(
            lines.start < lines.end,
            "an empty line range {lines:?} for a fragment of {path}"
        );
        Fragment {
            path,
            start: lines.start + 1,
            end: lines.end,
        }
    }
}

impl fmt::Display for Fragment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}-{}", self.path, self.start, self.end)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Pair {
    pub a: Fragment,
    pub b: Fragment,
}

impl Pair {
    pub fn new(x: Fragment, y: Fragment) -> Result<Pair> {
        match x.cmp(&y) {
            std::cmp::Ordering::Less => Ok(Pair { a: x, b: y }),
            std::cmp::Ordering::Greater => Ok(Pair { a: y, b: x }),
            std::cmp::Ordering::Equal => bail!("a pair of the same fragment {x}"),
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct PairInfo {
    pub similarity: Option<f64>,
    /// ラベルを列挙型にせず，プラグインの名前の順の列で持つのは，
    /// 分類のプラグインをいくつでも並べられるようにするためである．
    pub labels: Vec<&'static str>,
}

/// 行を i64，パスをバイト列で受け取るのは，0 や負の行や UTF-8 でないパスを型で先に弾くと，
/// アダプタごとに止め方がばらばらになるからである．detection.rs が 1 つの決まりで確かめて止める．
#[derive(Clone, PartialEq, Debug)]
pub struct RawFragment {
    pub path: Vec<u8>,
    pub start: i64,
    pub end: i64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct RawPair {
    pub a: RawFragment,
    pub b: RawFragment,
    pub similarity: Option<f64>,
}

/// パスをバイト列で持つのは，範囲が Git の tree の一覧から作られるからである
#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct Scope(pub BTreeSet<Vec<u8>>);

impl Scope {
    pub fn contains(&self, path: &[u8]) -> bool {
        self.0.contains(path)
    }
}

/// 3 つ組の順の入れ物にするのは，どこから取り出しても同じ順に出て，書き出す順が入力だけで決まるからである．
/// 追う片は，その版で検出された片だけとする．
pub struct VersionState {
    pub pairs: BTreeMap<Pair, PairInfo>,
    pub fragments: BTreeSet<Fragment>,
    /// 順の決まらない入れ物にするのは，引くだけに使うからである
    pub index: HashMap<Rc<str>, Vec<Fragment>>,
    pub scope: Scope,
}

impl VersionState {
    pub fn new(pairs: BTreeMap<Pair, PairInfo>, scope: Scope) -> VersionState {
        let fragments: BTreeSet<Fragment> = pairs
            .keys()
            .flat_map(|pair| [pair.a.clone(), pair.b.clone()])
            .collect();
        let mut index: HashMap<Rc<str>, Vec<Fragment>> = HashMap::new();
        // 並べ直さないのは，BTreeSet の順が 3 つ組の順で，パスごとに開始行，終了行の順に入るからである
        for fragment in &fragments {
            index
                .entry(Rc::clone(&fragment.path))
                .or_default()
                .push(fragment.clone());
        }
        VersionState {
            pairs,
            fragments,
            index,
            scope,
        }
    }

    pub fn empty() -> VersionState {
        VersionState::new(BTreeMap::new(), Scope::default())
    }
}

/// 0 から数えた範囲に直さずに Git の数のまま持つのは，
/// 直す場所を line_events の入り口の 1 か所にするためである．
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
}

/// T の分け方と，普通のファイルでないものを外すのをここで行わないのは，
/// git/raw.rs を読むだけにし，規則を core/file_mapping.rs にまとめるためである．
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RawEntry {
    pub status: RawStatus,
    pub old: Option<Side>,
    pub new: Option<Side>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RawStatus {
    Added,
    Deleted,
    Modified,
    Renamed { score: u8 },
    TypeChanged,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Side {
    pub mode: u32,
    pub oid: String,
    /// String にしないのは，Git のパスが UTF-8 とは限らないからである．
    pub path: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TreeEntry {
    pub mode: u32,
    pub kind: ObjectKind,
    pub oid: String,
    /// UTF-8 として読めないパスで止めないのは，片の無いファイルなら困らないからである．
    pub path: Vec<u8>,
}

impl TreeEntry {
    /// mode で決めるのは，シンボリックリンクと submodule を範囲の候補に入れないためである．
    pub fn is_regular_file(&self) -> bool {
        self.kind == ObjectKind::Blob && matches!(self.mode, 0o100644 | 0o100755)
    }
}

/// Tree が無いのは，ls-tree -r が tree を出さずに中へ降りるからである
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectKind {
    Blob,
    Commit,
}

/// rusqlite の型にしないのは，プラグインが store を呼ばず，データベースの型を知らずに済むようにするためである．
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Cell {
    #[allow(dead_code)] // 整数の列を持つプラグインの表はまだ無いが，表の値の型として残す
    Int(i64),
    Text(String),
}

/// 直せなかったことを誤りにしないのは，グラフの側が行番号を付けた誤りの文にするからである．
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Resolution {
    Commit(String),
    Unresolved { reason: String },
}

impl fmt::Display for Hunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_count, self.new_start, self.new_count
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(path: &str, start: u32, end: u32) -> Fragment {
        Fragment {
            path: Rc::from(path),
            start,
            end,
        }
    }

    #[test]
    fn lines_and_from_lines_go_back_and_forth() {
        let f = fragment("src/A.java", 3, 5);
        assert_eq!(f.lines(), 2..5);
        assert_eq!(Fragment::from_lines(Rc::clone(&f.path), f.lines()), f);
        let one = fragment("src/A.java", 1, 1);
        assert_eq!(one.lines(), 0..1);
        assert_eq!(Fragment::from_lines(Rc::clone(&one.path), 0..1), one);
        let last = fragment("src/A.java", u32::MAX, u32::MAX);
        assert_eq!(last.lines(), u32::MAX - 1..u32::MAX);
        assert_eq!(
            Fragment::from_lines(Rc::clone(&last.path), last.lines()),
            last
        );
    }

    #[test]
    #[should_panic(expected = "invalid line range")]
    fn lines_panics_on_line_zero() {
        fragment("a", 0, 3).lines();
    }

    #[test]
    #[should_panic(expected = "empty line range")]
    fn from_lines_panics_on_an_empty_range() {
        Fragment::from_lines(Rc::from("a"), 4..4);
    }

    #[test]
    fn fragments_are_ordered_by_path_bytes_then_lines() {
        let mut fragments = [
            fragment("é", 1, 2),
            fragment("a0", 1, 2),
            fragment("a/b", 5, 9),
            fragment("a/b", 5, 6),
            fragment("a/b", 2, 9),
            fragment("B", 1, 1),
            fragment("z", 1, 1),
        ];
        fragments.sort();
        let order: Vec<String> = fragments.iter().map(ToString::to_string).collect();
        // '/' は 0x2F で '0' の 0x30 より前，'B' は 'a' より前，'é' は 0xC3 で始まり 'z' より後
        assert_eq!(
            order,
            [
                "B:1-1", "a/b:2-9", "a/b:5-6", "a/b:5-9", "a0:1-2", "z:1-1", "é:1-2"
            ]
        );
    }

    #[test]
    fn pair_new_puts_the_smaller_fragment_first() {
        let x = fragment("b", 1, 3);
        let y = fragment("a", 7, 9);
        let pair = Pair::new(x.clone(), y.clone()).unwrap();
        assert_eq!((&pair.a, &pair.b), (&y, &x));
        assert_eq!(Pair::new(y, x).unwrap(), pair);
    }

    #[test]
    fn pair_new_rejects_the_same_fragment() {
        let x = fragment("a", 1, 3);
        let err = Pair::new(x.clone(), x).unwrap_err();
        assert_eq!(err.to_string(), "a pair of the same fragment a:1-3");
    }

    #[test]
    fn pairs_are_ordered_by_a_then_b() {
        let p = |a: (&str, u32), b: (&str, u32)| {
            Pair::new(fragment(a.0, a.1, a.1), fragment(b.0, b.1, b.1)).unwrap()
        };
        let mut pairs = vec![
            p(("b", 1), ("c", 1)),
            p(("a", 2), ("b", 1)),
            p(("a", 1), ("c", 1)),
        ];
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                p(("a", 1), ("c", 1)),
                p(("a", 2), ("b", 1)),
                p(("b", 1), ("c", 1))
            ]
        );
    }

    #[test]
    fn hunk_is_shown_like_the_git_header() {
        let hunk = Hunk {
            old_start: 3,
            old_count: 0,
            new_start: 4,
            new_count: 2,
        };
        assert_eq!(hunk.to_string(), "@@ -3,0 +4,2 @@");
    }
}
