use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};

use crate::core::model::Hunk;
use crate::git::{Git, failure, last_line};
use crate::process::Interrupt;

/// 4 GiB 以上の blob を読まないのは，行の数と語の数を u32 で数えるからである．
const MAX_BLOB_BYTES: u64 = 1 << 32;

#[derive(PartialEq, Eq, Debug)]
pub enum BlobDiff {
    Hunks(Vec<Hunk>),
    Failed,
}

/// ファイルの組ごとに呼び，まとめて呼ばないのは，1 回 4 ミリ秒ほどで検出の時間と比べて小さいからである．
pub fn diff_blobs(
    git: &Git,
    clone: &Path,
    old_oid: &str,
    new_oid: &str,
    interrupt: &Interrupt,
) -> Result<BlobDiff> {
    let args = [
        "--no-replace-objects",
        "diff",
        "--histogram",
        "--indent-heuristic",
        "-w",
        "-U0",
        "--inter-hunk-context=0",
        "--text",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        old_oid,
        new_oid,
    ];
    let output = git.run(clone, &args, interrupt)?;
    match output.status.code() {
        Some(0) => parse_hunks(&output.stdout)
            .map(BlobDiff::Hunks)
            .with_context(|| format!("cannot read the hunks from {old_oid} to {new_oid}")),
        Some(_) => Ok(BlobDiff::Failed),
        // 信号を diff の失敗にしないのは，Git が diff を出せなかったのではなく，外から止められたからである．
        None => Err(failure(&args, &output)),
    }
}

/// 見出しのほかの行で止めないのは，中身の行が + か - で始まり，@@ で始まる行と取り違えないからである．
pub fn parse_hunks(bytes: &[u8]) -> Result<Vec<Hunk>> {
    bytes
        .split(|&b| b == b'\n')
        .filter(|line| line.starts_with(b"@@"))
        .map(|line| {
            parse_header(line).ok_or_else(|| {
                anyhow!(
                    "Git output is inconsistent: malformed hunk header {:?}",
                    String::from_utf8_lossy(line)
                )
            })
        })
        .collect()
}

/// 2 つ目の @@ の後の関数の名前を読まないのは，行イベントに使わないからである
fn parse_header(line: &[u8]) -> Option<Hunk> {
    let rest = line.strip_prefix(b"@@ -")?;
    let (old_start, old_count, rest) = range(rest)?;
    let rest = rest.strip_prefix(b" +")?;
    let (new_start, new_count, rest) = range(rest)?;
    let rest = rest.strip_prefix(b" @@")?;
    if !(rest.is_empty() || rest.starts_with(b" ")) {
        return None;
    }
    Some(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

fn range(text: &[u8]) -> Option<(u32, u32, &[u8])> {
    let (start, rest) = number(text)?;
    match rest.strip_prefix(b",") {
        Some(rest) => {
            let (count, rest) = number(rest)?;
            Some((start, count, rest))
        }
        None => Some((start, 1, rest)),
    }
}

/// u32 に収まらない数を形に合わないとするのは，行の数を u32 で数えるからである．
fn number(text: &[u8]) -> Option<(u32, &[u8])> {
    let digits = text.iter().take_while(|b| b.is_ascii_digit()).count();
    let value = std::str::from_utf8(&text[..digits]).ok()?.parse().ok()?;
    Some((value, &text[digits..]))
}

/// blob ごとに Git を起動しないのは，1 回に 3 ミリ秒ほどかかるからである．
pub struct BlobReader {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    waited: bool,
}

impl BlobReader {
    pub fn start(git: &Git, clone: &Path) -> Result<BlobReader> {
        let mut child = git.spawn(clone, &["cat-file", "--batch"])?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().map(BufReader::new);
        let mut stderr = child.stderr.take().expect("stderr is piped");
        // 読まずにおかないのは，Git が標準エラーに書き続けると止まるからである
        let stderr = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
            buffer
        });
        Ok(BlobReader {
            child,
            stdin,
            stdout,
            stderr: Some(stderr),
            waited: false,
        })
    }

    /// 誤りの後に読めなくするのは，読みかけの中身が残り，次の答えとずれるからである
    pub fn read(&mut self, oid: &str, interrupt: &Interrupt) -> Result<Vec<u8>> {
        let result = self.read_one(oid, interrupt);
        if result.is_err() {
            self.close_pipes();
        }
        result
    }

    fn read_one(&mut self, oid: &str, interrupt: &Interrupt) -> Result<Vec<u8>> {
        let (Some(stdin), Some(stdout)) = (self.stdin.as_mut(), self.stdout.as_mut()) else {
            bail!("git cat-file --batch is no longer usable");
        };
        if let Err(err) = stdin
            .write_all(format!("{oid}\n").as_bytes())
            .and_then(|()| stdin.flush())
        {
            interrupt.check()?;
            return Err(anyhow!(err).context("git cat-file --batch stopped"));
        }
        let mut header = Vec::new();
        stdout
            .read_until(b'\n', &mut header)
            .context("cannot read from git cat-file --batch")?;
        if header.pop() != Some(b'\n') {
            interrupt.check()?;
            bail!("git cat-file --batch stopped before answering {oid}");
        }
        let size = blob_size(&header, oid)?;
        let mut content = vec![0; size as usize];
        stdout
            .read_exact(&mut content)
            .with_context(|| format!("cannot read blob {oid} from git cat-file --batch"))?;
        let mut newline = [0u8; 1];
        stdout
            .read_exact(&mut newline)
            .with_context(|| format!("cannot read blob {oid} from git cat-file --batch"))?;
        if newline != *b"\n" {
            bail!("git cat-file --batch did not end blob {oid} with a newline");
        }
        Ok(content)
    }

    pub fn finish(mut self) -> Result<()> {
        self.close_pipes();
        let status = self.child.wait().context("cannot wait for git cat-file")?;
        self.waited = true;
        let stderr = self
            .stderr
            .take()
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        if !status.success() {
            return Err(match last_line(&stderr) {
                Some(line) => anyhow!("git cat-file failed with {status}: {line}"),
                None => anyhow!("git cat-file failed with {status}"),
            });
        }
        Ok(())
    }

    /// 標準出力も閉じるのは，大きな blob を書いている途中の Git を，待たせたままにしないためである
    fn close_pipes(&mut self) {
        self.stdin = None;
        self.stdout = None;
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        if !self.waited {
            self.close_pipes();
            let _ = self.child.wait();
        }
    }
}

fn blob_size(header: &[u8], oid: &str) -> Result<u64> {
    let header = std::str::from_utf8(header)
        .with_context(|| format!("unexpected answer of git cat-file --batch for {oid}"))?;
    let fields: Vec<&str> = header.split(' ').collect();
    match fields[..] {
        [name, "missing"] if name == oid => bail!("object {oid} is missing"),
        [name, "blob", size] if name == oid => {
            let size: u64 = size
                .parse()
                .with_context(|| format!("unexpected answer {header:?} of git cat-file --batch"))?;
            if size >= MAX_BLOB_BYTES {
                bail!("blob {oid} has {size} bytes; blobs of 4 GiB or more are not supported");
            }
            Ok(size)
        }
        [name, kind, _] if name == oid => bail!("object {oid} is a {kind}, not a blob"),
        _ => bail!("unexpected answer {header:?} of git cat-file --batch for {oid}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testing::*;

    fn hunk(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> Hunk {
        Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
        }
    }

    #[test]
    fn headers_are_read_and_other_lines_are_skipped() {
        let output = b"diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n\
            @@ -2 +2 @@ a\n-b\n+B\n@@ -4,0 +5,2 @@ int main()\n+@@ -9 +9 @@\n-@@ x\n\
            @@ -7,3 +8,0 @@\n-x\n-y\n-z\n\\ No newline at end of file\n";
        assert_eq!(
            parse_hunks(output).unwrap(),
            [hunk(2, 1, 2, 1), hunk(4, 0, 5, 2), hunk(7, 3, 8, 0)]
        );
        assert!(parse_hunks(b"").unwrap().is_empty());
    }

    #[test]
    fn malformed_headers_are_refused() {
        for bad in [
            &b"@@ -a +1 @@\n"[..],
            b"@@ -1 +1\n",
            b"@@ -1 +1 @@x\n",
            b"@@ -1, +1 @@\n",
            b"@@ -4294967296 +1 @@\n",
            b"@@@ -1 +1 @@@\n",
        ] {
            let err = parse_hunks(bad).unwrap_err();
            assert!(err.to_string().contains("malformed hunk header"), "{err}");
        }
        assert_eq!(
            parse_hunks(b"@@ -4294967295,1 +1 @@\n").unwrap(),
            [hunk(u32::MAX, 1, 1, 1)]
        );
    }

    fn blob(root: &Path, rev: &str) -> String {
        git(root, &["rev-parse", rev])
    }

    #[test]
    fn git_gives_hunks_and_ignores_whitespace() {
        let dir = TempDir::new("hunks");
        let root = repo(&dir, "r");
        commit(&root, &[("f.txt", "a\nb\nc\n")], "one", "1700000100");
        commit(&root, &[("f.txt", "  a\nB\nc\nd\n")], "two", "1700000200");
        commit(&root, &[("f.txt", "a\r\nB\nc\nd\n")], "three", "1700000300");
        let (one, two, three) = (
            blob(&root, "HEAD~2:f.txt"),
            blob(&root, "HEAD~1:f.txt"),
            blob(&root, "HEAD:f.txt"),
        );
        let g = Git::new();
        let none = Interrupt::default();
        assert_eq!(
            diff_blobs(&g, &root, &one, &two, &none).unwrap(),
            BlobDiff::Hunks(vec![hunk(2, 1, 2, 1), hunk(3, 0, 4, 1)])
        );
        assert_eq!(
            diff_blobs(&g, &root, &two, &three, &none).unwrap(),
            BlobDiff::Hunks(vec![])
        );
    }

    #[test]
    fn a_diff_that_git_cannot_make_is_failed() {
        let dir = TempDir::new("failed");
        let root = repo(&dir, "r");
        let missing = "f".repeat(40);
        let a = blob(&root, "HEAD:a.txt");
        assert_eq!(
            diff_blobs(&Git::new(), &root, &a, &missing, &Interrupt::default()).unwrap(),
            BlobDiff::Failed
        );
    }

    #[test]
    fn blobs_are_read_one_after_another() {
        let dir = TempDir::new("blobs");
        let root = repo(&dir, "r");
        commit(
            &root,
            &[("e.txt", ""), ("b.bin", "x\0\ny\n")],
            "two",
            "1700000100",
        );
        let mut reader = BlobReader::start(&Git::new(), &root).unwrap();
        let none = Interrupt::default();
        assert_eq!(
            reader.read(&blob(&root, "HEAD:a.txt"), &none).unwrap(),
            b"a\n"
        );
        assert_eq!(reader.read(&blob(&root, "HEAD:e.txt"), &none).unwrap(), b"");
        assert_eq!(
            reader.read(&blob(&root, "HEAD:b.bin"), &none).unwrap(),
            b"x\0\ny\n"
        );
        reader.finish().unwrap();
    }

    #[test]
    fn missing_objects_and_trees_stop_and_the_reader_is_closed() {
        let dir = TempDir::new("notblob");
        let root = repo(&dir, "r");
        let none = Interrupt::default();
        let mut reader = BlobReader::start(&Git::new(), &root).unwrap();
        let missing = "f".repeat(40);
        let err = reader.read(&missing, &none).unwrap_err();
        assert_eq!(err.to_string(), format!("object {missing} is missing"));
        assert!(reader.read(&blob(&root, "HEAD:a.txt"), &none).is_err());

        let mut reader = BlobReader::start(&Git::new(), &root).unwrap();
        let tree = blob(&root, "HEAD^{tree}");
        let err = reader.read(&tree, &none).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("object {tree} is a tree, not a blob")
        );
    }

    #[test]
    fn blobs_of_4_gib_are_refused_before_reading() {
        let oid = "a".repeat(40);
        assert_eq!(
            blob_size(format!("{oid} blob 4294967295").as_bytes(), &oid).unwrap(),
            4294967295
        );
        let err = blob_size(format!("{oid} blob 4294967296").as_bytes(), &oid).unwrap_err();
        assert!(err.to_string().contains("4 GiB or more"), "{err}");
        assert!(blob_size(format!("{} blob 1", "b".repeat(40)).as_bytes(), &oid).is_err());
    }
}
