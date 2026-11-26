use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::core::model::{ObjectKind, Resolution, TreeEntry};
use crate::git::{Git, failure, strip_newline};
use crate::process::Interrupt;

pub fn git_version(git: &Git, dir: &Path, interrupt: &Interrupt) -> Result<String> {
    let output = git.output(dir, &["--version"], interrupt)?;
    String::from_utf8(strip_newline(output)).context("git --version printed non-UTF-8 output")
}

/// この確かめを足すのは，作業ツリーの下のディレクトリでは解析用の複製が作れず，
/// データベースを作った後で止まって，--resume の要るデータベースが残るからである．
pub fn check_repository(git: &Git, repo: &Path, interrupt: &Interrupt) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(repo)
        .with_context(|| format!("cannot resolve --repo {}", repo.display()))?;
    let not_a_root = || {
        format!(
            "--repo {} is not the root of a work tree or a bare repository",
            canonical.display()
        )
    };
    let bare = git
        .output(
            &canonical,
            &["rev-parse", "--is-bare-repository"],
            interrupt,
        )
        .with_context(|| format!("cannot check --repo {}", canonical.display()))?;
    let root_arg = match strip_newline(bare).as_slice() {
        b"true" => "--absolute-git-dir",
        b"false" => "--show-toplevel",
        other => bail!(
            "git rev-parse --is-bare-repository printed {:?}",
            String::from_utf8_lossy(other)
        ),
    };
    let root = git
        .output(&canonical, &["rev-parse", root_arg], interrupt)
        .with_context(not_a_root)?;
    let root = PathBuf::from(std::ffi::OsString::from_vec(strip_newline(root)));
    // 文字列のまま比べないのは，Git の出すパスとシンボリックリンクの解き方が違いうるからである
    let root = std::fs::canonicalize(&root)
        .with_context(|| format!("cannot resolve {} printed by git", root.display()))?;
    if root != canonical {
        bail!("{}; its root is {}", not_a_root(), root.display());
    }
    Ok(canonical)
}

pub fn check_not_shallow(git: &Git, repo: &Path, interrupt: &Interrupt) -> Result<()> {
    let shallow = git.output(repo, &["rev-parse", "--is-shallow-repository"], interrupt)?;
    match strip_newline(shallow).as_slice() {
        b"false" => Ok(()),
        b"true" => bail!(
            "--repo {} is a shallow repository; fetch the whole history first",
            repo.display()
        ),
        other => bail!(
            "git rev-parse --is-shallow-repository printed {:?}",
            String::from_utf8_lossy(other)
        ),
    }
}

/// 書いた文字列で始まるかをここで確かめないのは，誤りの文に行番号を入れるグラフの側で確かめるからである．
pub fn resolve_commit(
    git: &Git,
    repo: &Path,
    rev: &str,
    interrupt: &Interrupt,
) -> Result<Resolution> {
    let spec = format!("{rev}^{{commit}}");
    let output = git.run(repo, &["rev-parse", "--verify", spec.as_str()], interrupt)?;
    if !output.status.success() {
        if output.status.code().is_none() {
            return Err(failure(&["rev-parse"], &output));
        }
        let reason = reason_of(&output.stderr)
            .unwrap_or_else(|| format!("exit code {}", output.status.code().unwrap_or(-1)));
        return Ok(Resolution::Unresolved { reason });
    }
    let commit = String::from_utf8(strip_newline(output.stdout))
        .context("git rev-parse printed non-UTF-8 output")?;
    Ok(Resolution::Commit(commit))
}

/// 標準エラーの最後の行だけにしないのは，無いハッシュでもコミットでないオブジェクトでも，
/// 最後の行が同じ `fatal: Needed a single revision` になるからである．hint の行は理由でないので入れない
fn reason_of(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("hint:"))
        .collect();
    (!lines.is_empty()).then(|| lines.join("; "))
}

/// 時刻の形に直さないのは，git/ と store/ が互いを呼ばないためである．
pub fn committer_times(
    git: &Git,
    repo: &Path,
    commits: &[String],
    interrupt: &Interrupt,
) -> Result<Vec<i64>> {
    // 空のまま呼ばないのは，入力が無いと git log が HEAD を出すからである
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    let args = [
        "--no-replace-objects",
        "log",
        "--no-walk=unsorted",
        "--no-show-signature",
        "--stdin",
        "--format=%H%x00%ct",
    ];
    let mut child = git.spawn(repo, &args)?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input: Vec<u8> = commits
        .iter()
        .flat_map(|c| format!("{c}\n").into_bytes())
        .collect();
    // 同じスレッドで書かないのは，書き終わる前に出力がたまって Git が止まるからである．
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().context("cannot read git log")?;
    let written = writer.join().expect("the writing thread does not panic");
    if !output.status.success() {
        interrupt.check()?;
        return Err(failure(&args, &output));
    }
    written.context("cannot write the versions to git log")?;
    parse_times(&output.stdout, commits)
}

fn parse_times(stdout: &[u8], commits: &[String]) -> Result<Vec<i64>> {
    let text = std::str::from_utf8(stdout).context("git log printed non-UTF-8 output")?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != commits.len() {
        bail!(
            "git log printed {} lines for {} versions",
            lines.len(),
            commits.len()
        );
    }
    lines
        .iter()
        .zip(commits)
        .map(|(line, commit)| {
            let (hash, time) = line
                .split_once('\0')
                .ok_or_else(|| anyhow!("unexpected git log line {line:?}"))?;
            if hash != commit {
                bail!("git log printed {hash} where {commit} was expected");
            }
            time.parse::<i64>()
                .with_context(|| format!("unexpected committer time {time:?} of {commit}"))
        })
        .collect()
}

pub fn is_ancestor(
    git: &Git,
    clone: &Path,
    parent: &str,
    child: &str,
    interrupt: &Interrupt,
) -> Result<bool> {
    let args = ["merge-base", "--is-ancestor", parent, child];
    let output = git.run(clone, &args, interrupt)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure(&args, &output)),
    }
}

/// 全部の版について前もって読まないのは，呼ぶのが起点になった版だけで，起点は少ないからである．
pub fn has_git_parents(
    git: &Git,
    clone: &Path,
    commit: &str,
    interrupt: &Interrupt,
) -> Result<bool> {
    let output = git.output(
        clone,
        &["rev-list", "--parents", "--no-walk", commit],
        interrupt,
    )?;
    let line = String::from_utf8(strip_newline(output))
        .context("git rev-list printed non-UTF-8 output")?;
    let mut hashes = line.split(' ');
    if hashes.next() != Some(commit) {
        bail!("git rev-list printed {line:?} for {commit}");
    }
    Ok(hashes.next().is_some())
}

pub fn tree(
    git: &Git,
    clone: &Path,
    commit: &str,
    interrupt: &Interrupt,
) -> Result<Vec<TreeEntry>> {
    let output = git.output(
        clone,
        &["ls-tree", "-r", "-z", "--full-tree", commit],
        interrupt,
    )?;
    parse_tree(&output).with_context(|| format!("cannot read the tree of {commit}"))
}

fn parse_tree(bytes: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut records = bytes.split(|&b| b == 0);
    // 最後の空の要素を項目にしないのは，-z の出力が NUL で終わるからである
    if bytes.last() == Some(&0) || bytes.is_empty() {
        records.next_back();
    }
    for record in records {
        let tab = record.iter().position(|&b| b == b'\t').ok_or_else(|| {
            anyhow!(
                "unexpected git ls-tree record {:?}",
                String::from_utf8_lossy(record)
            )
        })?;
        let (head, path) = (&record[..tab], &record[tab + 1..]);
        let head = std::str::from_utf8(head).context("unexpected git ls-tree record")?;
        let mut fields = head.split(' ');
        let (Some(mode), Some(kind), Some(oid), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            bail!("unexpected git ls-tree record {head:?}");
        };
        let kind = match kind {
            "blob" => ObjectKind::Blob,
            "commit" => ObjectKind::Commit,
            _ => bail!("unexpected object type {kind:?} in git ls-tree -r"),
        };
        entries.push(TreeEntry {
            mode: parse_mode(mode)?,
            kind,
            oid: parse_oid(oid)?,
            path: path.to_vec(),
        });
    }
    Ok(entries)
}

/// ハッシュの値を書かずに Git に聞くのは，SHA-1 と SHA-256 のリポジトリで値が違うからである．
pub fn empty_tree(git: &Git, clone: &Path, interrupt: &Interrupt) -> Result<String> {
    let output = git.output(
        clone,
        &["hash-object", "-t", "tree", "/dev/null"],
        interrupt,
    )?;
    parse_oid(
        std::str::from_utf8(&strip_newline(output))
            .context("git hash-object printed non-UTF-8 output")?,
    )
}

pub fn parse_mode(text: &str) -> Result<u32> {
    if text.len() != 6 {
        bail!("unexpected file mode {text:?}");
    }
    u32::from_str_radix(text, 8).with_context(|| format!("unexpected file mode {text:?}"))
}

/// 長さを 40 と 64 に限るのは，SHA-1 と SHA-256 の完全なハッシュだけを受け取るからである
pub fn parse_oid(text: &str) -> Result<String> {
    let hex = text
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !hex || !(text.len() == 40 || text.len() == 64) {
        bail!("unexpected object name {text:?}");
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testing::*;

    fn none() -> Interrupt {
        Interrupt::default()
    }

    #[test]
    fn the_root_of_a_work_tree_and_a_bare_repository_are_accepted() {
        let dir = TempDir::new("roots");
        let root = repo(&dir, "r");
        let g = Git::new();
        assert_eq!(check_repository(&g, &root, &none()).unwrap(), root);
        git(dir.path(), &["clone", "-q", "--bare", "r", "bare.git"]);
        let bare = dir.path().join("bare.git");
        assert_eq!(check_repository(&g, &bare, &none()).unwrap(), bare);
        // 正規化したパスを返す
        let dotted = dir.path().join("r/../r");
        assert_eq!(check_repository(&g, &dotted, &none()).unwrap(), root);
    }

    #[test]
    fn a_subdirectory_and_the_git_directory_are_refused() {
        let dir = TempDir::new("subdirs");
        let root = repo(&dir, "r");
        std::fs::create_dir(root.join("sub")).unwrap();
        let g = Git::new();
        let err = check_repository(&g, &root.join("sub"), &none()).unwrap_err();
        assert!(
            format!("{err:#}")
                .contains("is not the root of a work tree or a bare repository; its root is"),
            "{err:#}"
        );
        let err = check_repository(&g, &root.join(".git"), &none()).unwrap_err();
        assert!(
            format!("{err:#}").contains("is not the root of a work tree"),
            "{err:#}"
        );
        let err = check_repository(&g, dir.path(), &none()).unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot check --repo"),
            "{err:#}"
        );
    }

    #[test]
    fn a_shallow_clone_is_refused() {
        let dir = TempDir::new("shallow");
        let root = repo(&dir, "r");
        commit(&root, &[("b.txt", "b\n")], "second", "1700000100");
        let url = format!("file://{}", root.display());
        git(dir.path(), &["clone", "-q", "--depth", "1", &url, "s"]);
        let g = Git::new();
        check_not_shallow(&g, &root, &none()).unwrap();
        let err = check_not_shallow(&g, &dir.path().join("s"), &none()).unwrap_err();
        assert!(err.to_string().contains("is a shallow repository"), "{err}");
    }

    #[test]
    fn short_hashes_resolve_and_bad_ones_give_the_reason_of_git() {
        let dir = TempDir::new("resolve");
        let root = repo(&dir, "r");
        let head = git(&root, &["rev-parse", "HEAD"]);
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        let g = Git::new();
        assert_eq!(
            resolve_commit(&g, &root, &head[..7], &none()).unwrap(),
            Resolution::Commit(head.clone())
        );
        assert_eq!(
            resolve_commit(&g, &root, "ffffffff", &none()).unwrap(),
            Resolution::Unresolved {
                reason: "fatal: Needed a single revision".to_string()
            }
        );
        let Resolution::Unresolved { reason } = resolve_commit(&g, &root, &tree, &none()).unwrap()
        else {
            panic!("a tree must not resolve to a commit");
        };
        assert_eq!(
            reason,
            format!(
                "error: {tree}^{{commit}}: expected commit type, but the object dereferences \
                 to tree type; fatal: Needed a single revision"
            )
        );
    }

    #[test]
    fn a_tag_named_like_a_short_hash_wins_over_the_hash() {
        // 直した結果が書いた文字列で始まるかを確かめるわけ．Git は短いハッシュより同じ名前のタグを先に採る
        let dir = TempDir::new("tagname");
        let root = repo(&dir, "r");
        let a = git(&root, &["rev-parse", "HEAD"]);
        let b = commit(&root, &[("b.txt", "b\n")], "second", "1700000100");
        git(&root, &["tag", &b[..7], &a]);
        let g = Git::new();
        assert_eq!(
            resolve_commit(&g, &root, &b[..7], &none()).unwrap(),
            Resolution::Commit(a)
        );
        assert_eq!(
            resolve_commit(&g, &root, &b, &none()).unwrap(),
            Resolution::Commit(b)
        );
    }

    #[test]
    fn committer_times_come_in_the_given_order() {
        let dir = TempDir::new("times");
        let root = repo(&dir, "r");
        let a = git(&root, &["rev-parse", "HEAD"]);
        let b = commit(&root, &[("b.txt", "b\n")], "second", "1700000100");
        let g = Git::new();
        assert_eq!(
            committer_times(&g, &root, &[b.clone(), a.clone()], &none()).unwrap(),
            [1700000100, 1700000000]
        );
        assert_eq!(
            committer_times(&g, &root, &[a, b], &none()).unwrap(),
            [1700000000, 1700000100]
        );
        assert!(committer_times(&g, &root, &[], &none()).unwrap().is_empty());
    }

    #[test]
    fn times_that_do_not_match_the_input_are_refused() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let both = format!("{a}\x001\n{b}\x002\n");
        assert_eq!(
            parse_times(both.as_bytes(), &[a.clone(), b.clone()]).unwrap(),
            [1, 2]
        );
        assert!(parse_times(both.as_bytes(), &[b.clone(), a.clone()]).is_err());
        assert!(parse_times(both.as_bytes(), std::slice::from_ref(&a)).is_err());
        assert!(parse_times(format!("{a}\x00x\n").as_bytes(), &[a]).is_err());
    }

    #[test]
    fn ancestors_and_parents() {
        let dir = TempDir::new("ancestors");
        let root = repo(&dir, "r");
        let a = git(&root, &["rev-parse", "HEAD"]);
        let b = commit(&root, &[("b.txt", "b\n")], "second", "1700000100");
        let g = Git::new();
        assert!(is_ancestor(&g, &root, &a, &b, &none()).unwrap());
        assert!(!is_ancestor(&g, &root, &b, &a, &none()).unwrap());
        assert!(is_ancestor(&g, &root, &a, "nosuch", &none()).is_err());
        assert!(!has_git_parents(&g, &root, &a, &none()).unwrap());
        assert!(has_git_parents(&g, &root, &b, &none()).unwrap());
    }

    #[test]
    fn the_tree_lists_blobs_links_and_submodules_with_byte_paths() {
        let dir = TempDir::new("tree");
        let root = repo(&dir, "r");
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(root.join("d/x y\tz.sh"), "#!/bin/sh\n").unwrap();
        git(
            &root,
            &["update-index", "--add", "--chmod=+x", "d/x y\tz.sh"],
        );
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
        let head = git(&root, &["rev-parse", "HEAD"]);
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{head},sub"),
            ],
        );
        std::fs::write(root.join("日本.txt"), "j\n").unwrap();
        // commit を使わないのは，add -A が作業ツリーに無い sub を消すからである
        git(&root, &["add", "link", "日本.txt"]);
        git_at(&root, &["commit", "-q", "-m", "second"], "1700000100");
        let c = git(&root, &["rev-parse", "HEAD"]);
        let entries = tree(&Git::new(), &root, &c, &none()).unwrap();
        let summary: Vec<(u32, ObjectKind, Vec<u8>)> = entries
            .iter()
            .map(|e| (e.mode, e.kind, e.path.clone()))
            .collect();
        assert_eq!(
            summary,
            [
                (0o100644, ObjectKind::Blob, b"a.txt".to_vec()),
                (0o100755, ObjectKind::Blob, b"d/x y\tz.sh".to_vec()),
                (0o120000, ObjectKind::Blob, b"link".to_vec()),
                (0o160000, ObjectKind::Commit, b"sub".to_vec()),
                (0o100644, ObjectKind::Blob, "日本.txt".as_bytes().to_vec()),
            ]
        );
        assert_eq!(entries[3].oid, head);
    }

    #[test]
    fn the_empty_tree_is_the_one_of_sha1() {
        let dir = TempDir::new("emptytree");
        let root = repo(&dir, "r");
        assert_eq!(
            empty_tree(&Git::new(), &root, &none()).unwrap(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }

    #[test]
    fn hints_are_left_out_of_the_reason() {
        assert_eq!(
            reason_of(
                b"error: short object ID abcd is ambiguous\nhint: The candidates are:\n\
                  hint:   abcd1 commit\nfatal: Needed a single revision\n"
            ),
            Some(
                "error: short object ID abcd is ambiguous; fatal: Needed a single revision"
                    .to_string()
            )
        );
        assert_eq!(reason_of(b"hint: only\n\n"), None);
    }

    #[test]
    fn modes_and_object_names_are_checked() {
        assert_eq!(parse_mode("100755").unwrap(), 0o100755);
        assert!(parse_mode("10075").is_err());
        assert!(parse_mode("100785").is_err());
        assert!(parse_oid(&"a".repeat(40)).is_ok());
        assert!(parse_oid(&"a".repeat(64)).is_ok());
        assert!(parse_oid(&"A".repeat(40)).is_err());
        assert!(parse_oid(&"a".repeat(12)).is_err());
    }
}
