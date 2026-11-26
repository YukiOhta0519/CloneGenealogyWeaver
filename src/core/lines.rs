use std::mem::size_of;
use std::sync::LazyLock;

use regex::bytes::{Regex, RegexBuilder};

const WORD_PATTERN: &str =
    r#"[A-Za-z_$][A-Za-z0-9_$]*|\d+(?:\.\d+)?|"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'"#;

/// Unicode を使う既定の設定にしないのは，全角の数字が `\d` に当たり，
/// 読めないバイトを含む行で語の数が 1 つに決まらなくなるからである．
static WORD: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(WORD_PATTERN)
        .unicode(false)
        .build()
        .expect("the word pattern is a valid regular expression")
});

const BLANK: u8 = 1;
const SYMBOL_ONLY: u8 = 2;

const BYTES_PER_LINE: u64 = (size_of::<usize>() + size_of::<u32>() + size_of::<u8>()) as u64;

/// 垂直タブ，改ページ，全角の空白を入れないのは，Git の `-w` に合わせるためである．
/// 入れると，Git が同じとした hunk の外の行を，ツールが違うとみなす．
pub fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn is_symbol(byte: u8) -> bool {
    matches!(
        byte,
        b'{' | b'}' | b'(' | b')' | b'[' | b']' | b';' | b',' | b':'
    )
}

pub fn strip_spaces(line: &[u8]) -> Vec<u8> {
    line.iter().copied().filter(|&b| !is_space(b)).collect()
}

/// strip_spaces で写してから比べないのは，hunk の外の行ごとに確保をしないためである
pub fn same_without_spaces(x: &[u8], y: &[u8]) -> bool {
    let x = x.iter().filter(|&&b| !is_space(b));
    let y = y.iter().filter(|&&b| !is_space(b));
    x.eq(y)
}

/// 語そのものを BlobLines に持たないのは，blob の大きさの数倍になるからである．
pub fn words_in(line: &[u8]) -> impl Iterator<Item = &[u8]> {
    WORD.find_iter(line).map(|m| m.as_bytes())
}

pub struct BlobLines {
    bytes: Vec<u8>,
    starts: Vec<usize>,
    flags: Vec<u8>,
    words: Vec<u32>,
}

impl BlobLines {
    /// 4 GiB 以上で誤りを返さずに panic するのは，git/hunks.rs が読む前に止めるので，
    /// ここに来るのはツールの誤りのときだけだからである．
    pub fn new(bytes: Vec<u8>) -> BlobLines {
        assert!(
            u32::try_from(bytes.len()).is_ok(),
            "a blob of 4 GiB or more reached BlobLines::new"
        );
        let mut starts = Vec::new();
        if !bytes.is_empty() {
            starts.push(0);
            for (i, &byte) in bytes.iter().enumerate() {
                if byte == b'\n' && i + 1 < bytes.len() {
                    starts.push(i + 1);
                }
            }
        }
        let mut lines = BlobLines {
            bytes,
            starts,
            flags: Vec::new(),
            words: Vec::new(),
        };
        let count = lines.starts.len();
        let mut flags = Vec::with_capacity(count);
        let mut words = Vec::with_capacity(count);
        for i in 0..count {
            let line = lines.line_at(i);
            flags.push(flags_of(line));
            // try_from で確かめないのは，語の数が行のバイト数を超えず，u32 に収まるからである
            words.push(WORD.find_iter(line).count() as u32);
        }
        lines.flags = flags;
        lines.words = words;
        lines
    }

    pub fn len(&self) -> u32 {
        self.starts.len() as u32
    }

    pub fn line(&self, i: u32) -> &[u8] {
        self.line_at(i as usize)
    }

    pub fn is_blank(&self, i: u32) -> bool {
        self.flags[i as usize] & BLANK != 0
    }

    pub fn is_symbol_only(&self, i: u32) -> bool {
        self.flags[i as usize] & SYMBOL_ONLY != 0
    }

    pub fn words(&self, i: u32) -> u32 {
        self.words[i as usize]
    }

    pub fn size(&self) -> u64 {
        self.bytes.len() as u64 + BYTES_PER_LINE * self.starts.len() as u64
    }

    fn line_at(&self, i: usize) -> &[u8] {
        let start = self.starts[i];
        let next = self.starts.get(i + 1).copied().unwrap_or(self.bytes.len());
        let end = if next > start && self.bytes[next - 1] == b'\n' {
            next - 1
        } else {
            next
        };
        &self.bytes[start..end]
    }
}

fn flags_of(line: &[u8]) -> u8 {
    let mut rest = line.iter().copied().filter(|&b| !is_space(b)).peekable();
    if rest.peek().is_none() {
        BLANK
    } else if rest.all(is_symbol) {
        SYMBOL_ONLY
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(bytes: &[u8]) -> BlobLines {
        BlobLines::new(bytes.to_vec())
    }

    fn all_lines(lines: &BlobLines) -> Vec<&[u8]> {
        (0..lines.len()).map(|i| lines.line(i)).collect()
    }

    fn count(line: &[u8]) -> usize {
        words_in(line).count()
    }

    #[test]
    fn an_empty_file_has_no_lines() {
        let lines = blob(b"");
        assert_eq!(lines.len(), 0);
        assert_eq!(lines.size(), 0);
    }

    #[test]
    fn lines_are_split_at_lf() {
        assert_eq!(all_lines(&blob(b"a\n")), [b"a"]);
        assert_eq!(all_lines(&blob(b"a\n\n")), [&b"a"[..], b""]);
        assert_eq!(all_lines(&blob(b"\n")), [b""]);
        assert_eq!(all_lines(&blob(b"\n\n\n")), [b"", b"", b""]);
    }

    #[test]
    fn the_last_line_without_lf_is_a_line() {
        assert_eq!(all_lines(&blob(b"a")), [b"a"]);
        assert_eq!(all_lines(&blob(b"a\nbc")), [&b"a"[..], b"bc"]);
    }

    #[test]
    fn cr_stays_in_the_line() {
        let lines = blob(b"a\r\n\r\nb\r");
        assert_eq!(all_lines(&lines), [&b"a\r"[..], b"\r", b"b\r"]);
        assert!(lines.is_blank(1));
    }

    #[test]
    fn unreadable_bytes_are_kept_as_they_are() {
        let lines = blob(b"a\xff\n\xf0\x9f\x98\n");
        assert_eq!(all_lines(&lines), [&b"a\xff"[..], b"\xf0\x9f\x98"]);
    }

    #[test]
    fn size_adds_13_bytes_per_line() {
        assert_eq!(blob(b"ab\ncd").size(), 5 + 2 * 13);
        assert_eq!(blob(b"\n").size(), 1 + 13);
    }

    #[test]
    fn blank_lines_are_made_of_the_four_spaces_only() {
        let lines = blob(b"\n \t \r\n\x0b\n\x0c\n\xe3\x80\x80\nx\n");
        let blank: Vec<bool> = (0..lines.len()).map(|i| lines.is_blank(i)).collect();
        // 垂直タブ，改ページ，全角の空白だけの行は空行にならない
        assert_eq!(blank, [true, true, false, false, false, false]);
    }

    #[test]
    fn symbol_only_lines() {
        let lines = blob(b"});\n  } \n{}()[];,:\n\n} x\n};.\n");
        let symbol: Vec<bool> = (0..lines.len()).map(|i| lines.is_symbol_only(i)).collect();
        // 空行は記号だけの行にしない
        assert_eq!(symbol, [true, true, true, false, false, false]);
    }

    #[test]
    fn words_follow_the_examples_of_the_design() {
        assert_eq!(count(b"return result;"), 2);
        assert_eq!(count(b"}"), 0);
        assert_eq!(count(b"0x1F"), 2);
        assert_eq!(count(b"3.14.15"), 2);
        assert_eq!(count(br#"say("a \" b", 'c');"#), 3);
        assert_eq!(count(b"`a b`"), 2);
        assert_eq!(count(b"$x _y z9"), 3);
    }

    #[test]
    fn words_do_not_use_unicode_classes() {
        // 全角の数字とアラビア文字の数字は \d に当たらない
        assert_eq!(count("１２３".as_bytes()), 0);
        assert_eq!(count("٣٤".as_bytes()), 0);
        // 日本語の語は数えない
        assert_eq!(count("// 日本語のコメント abc".as_bytes()), 1);
        // 読めないバイトを含む文字列は 1 語になる
        assert_eq!(count(b"\"\xe6\x97\xa5\xff\""), 1);
    }

    #[test]
    fn words_are_counted_per_line() {
        let lines = blob(b"int a = 1;\n}\n\"s\" 'c'");
        let words: Vec<u32> = (0..lines.len()).map(|i| lines.words(i)).collect();
        assert_eq!(words, [3, 0, 2]);
    }

    #[test]
    fn words_in_returns_the_words_themselves() {
        let words: Vec<&[u8]> = words_in(b"x = foo(\"a b\", 3.5);").collect();
        assert_eq!(words, [&b"x"[..], b"foo", b"\"a b\"", b"3.5"]);
    }

    #[test]
    fn spaces_are_the_four_characters_of_git() {
        assert_eq!(strip_spaces(b" a\tb\rc\nd\x0be\x0c"), b"abcd\x0be\x0c");
        assert!(same_without_spaces(b"  if (x) {\r", b"if(x){"));
        assert!(!same_without_spaces(b"a\x0b", b"a"));
        assert!(same_without_spaces(b"", b" \t"));
    }
}
