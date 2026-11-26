use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::core::model::{RawEntry, RawStatus, Side};
use crate::git::Git;
use crate::git::revs::{parse_mode, parse_oid};
use crate::process::Interrupt;

pub fn diff_raw(
    git: &Git,
    clone: &Path,
    old: &str,
    new: &str,
    interrupt: &Interrupt,
) -> Result<Vec<RawEntry>> {
    let output = git.output(
        clone,
        &[
            "--no-replace-objects",
            "diff",
            "--raw",
            "-z",
            "--no-abbrev",
            "-M50%",
            "-l0",
            "--no-relative",
            "--no-ext-diff",
            "--no-textconv",
            old,
            new,
        ],
        interrupt,
    )?;
    parse_raw(&output).with_context(|| format!("cannot read git diff --raw from {old} to {new}"))
}

/// Git を呼ぶ部分と分けるのは，Git を使わずに，手で書いたバイト列で確かめるためである．
pub fn parse_raw(bytes: &[u8]) -> Result<Vec<RawEntry>> {
    let mut fields = bytes.split(|&b| b == 0);
    // 最後の空の要素を項目にしないのは，-z の出力が NUL で終わるからである
    if bytes.is_empty() || bytes.last() == Some(&0) {
        fields.next_back();
    }
    let mut entries = Vec::new();
    while let Some(head) = fields.next() {
        let mut next_path = || {
            fields
                .next()
                .map(<[u8]>::to_vec)
                .ok_or_else(|| anyhow!("the output ends before a path"))
        };
        let head = std::str::from_utf8(head)
            .ok()
            .and_then(|h| h.strip_prefix(':'))
            .ok_or_else(|| anyhow!("unexpected record {:?}", String::from_utf8_lossy(head)))?;
        let parts: Vec<&str> = head.split(' ').collect();
        let [old_mode, new_mode, old_oid, new_oid, status] = parts[..] else {
            bail!("unexpected record {head:?}");
        };
        let status = parse_status(status)?;
        let old_path = next_path()?;
        let new_path = match status {
            RawStatus::Renamed { .. } => next_path()?,
            _ => old_path.clone(),
        };
        let side = |mode: &str, oid: &str, path: Vec<u8>| -> Result<Side> {
            Ok(Side {
                mode: parse_mode(mode)?,
                oid: parse_oid(oid)?,
                path,
            })
        };
        let old = match status {
            RawStatus::Added => None,
            _ => Some(side(old_mode, old_oid, old_path)?),
        };
        let new = match status {
            RawStatus::Deleted => None,
            _ => Some(side(new_mode, new_oid, new_path)?),
        };
        entries.push(RawEntry { status, old, new });
    }
    Ok(entries)
}

/// C と U で止めるのは，-C を付けず，コミットどうしの diff なので出ないはずだからである．
fn parse_status(text: &str) -> Result<RawStatus> {
    let status = match text {
        "A" => RawStatus::Added,
        "D" => RawStatus::Deleted,
        "M" => RawStatus::Modified,
        "T" => RawStatus::TypeChanged,
        _ => {
            let score = text
                .strip_prefix('R')
                .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|digits| digits.parse::<u8>().ok())
                .filter(|score| *score <= 100)
                .ok_or_else(|| anyhow!("unexpected status {text:?}"))?;
            RawStatus::Renamed { score }
        }
    };
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testing::*;

    const Z: &str = "0000000000000000000000000000000000000000";
    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    fn side(mode: u32, oid: &str, path: &[u8]) -> Option<Side> {
        Some(Side {
            mode,
            oid: oid.to_string(),
            path: path.to_vec(),
        })
    }

    #[test]
    fn each_status_becomes_one_entry() {
        let mut bytes = Vec::new();
        for (record, paths) in [
            (format!(":000000 100644 {Z} {A} A"), vec!["new.txt"]),
            (format!(":100644 000000 {A} {Z} D"), vec!["old.txt"]),
            (format!(":100644 100755 {A} {B} M"), vec!["run.sh"]),
            (
                format!(":100644 100644 {A} {B} R086"),
                vec!["from.txt", "to.txt"],
            ),
            (format!(":100644 120000 {A} {B} T"), vec!["link"]),
        ] {
            bytes.extend_from_slice(record.as_bytes());
            for path in paths {
                bytes.push(0);
                bytes.extend_from_slice(path.as_bytes());
            }
            bytes.push(0);
        }
        let entries = parse_raw(&bytes).unwrap();
        assert_eq!(
            entries,
            [
                RawEntry {
                    status: RawStatus::Added,
                    old: None,
                    new: side(0o100644, A, b"new.txt"),
                },
                RawEntry {
                    status: RawStatus::Deleted,
                    old: side(0o100644, A, b"old.txt"),
                    new: None,
                },
                RawEntry {
                    status: RawStatus::Modified,
                    old: side(0o100644, A, b"run.sh"),
                    new: side(0o100755, B, b"run.sh"),
                },
                RawEntry {
                    status: RawStatus::Renamed { score: 86 },
                    old: side(0o100644, A, b"from.txt"),
                    new: side(0o100644, B, b"to.txt"),
                },
                RawEntry {
                    status: RawStatus::TypeChanged,
                    old: side(0o100644, A, b"link"),
                    new: side(0o120000, B, b"link"),
                },
            ]
        );
    }

    #[test]
    fn paths_keep_their_bytes() {
        let path = "日本/a\tb\nc".as_bytes();
        let mut bytes = format!(":100644 100644 {A} {B} M\0").into_bytes();
        bytes.extend_from_slice(path);
        bytes.extend_from_slice(b"\0");
        bytes.extend_from_slice(format!(":100644 100644 {A} {B} M\0").as_bytes());
        bytes.extend_from_slice(b"\xff\0");
        let entries = parse_raw(&bytes).unwrap();
        assert_eq!(entries[0].new.as_ref().unwrap().path, path);
        assert_eq!(entries[1].old.as_ref().unwrap().path, b"\xff");
    }

    #[test]
    fn nothing_changed_gives_no_entries() {
        assert!(parse_raw(b"").unwrap().is_empty());
    }

    #[test]
    fn unexpected_output_is_refused() {
        for bad in [
            format!(":100644 100644 {A} {B} C75\0a\0b\0"),
            format!(":100644 100644 {A} {B} U\0a\0"),
            format!(":100644 100644 {A} {B} R\0a\0b\0"),
            format!(":100644 100644 {A} {B} R101\0a\0b\0"),
            format!(":100644 100644 {A} {B} R086\0a\0"),
            format!(":100644 100644 {A} M\0a\0"),
            format!("100644 100644 {A} {B} M\0a\0"),
            format!(":100644 100644 {A} abc M\0a\0"),
        ] {
            assert!(parse_raw(bad.as_bytes()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn git_reports_a_rename_and_files_added_to_the_empty_tree() {
        let dir = TempDir::new("raw");
        let root = repo(&dir, "r");
        let first = git(&root, &["rev-parse", "HEAD"]);
        std::fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
        let second = commit(&root, &[], "rename", "1700000100");
        let g = Git::new();
        let none = Interrupt::default();
        let entries = diff_raw(&g, &root, &first, &second, &none).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, RawStatus::Renamed { score: 100 });
        assert_eq!(entries[0].old.as_ref().unwrap().path, b"a.txt");
        assert_eq!(entries[0].new.as_ref().unwrap().path, b"b.txt");
        let empty = crate::git::revs::empty_tree(&g, &root, &none).unwrap();
        let entries = diff_raw(&g, &root, &empty, &first, &none).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, RawStatus::Added);
    }
}
