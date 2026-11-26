use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git::Git;
use crate::process::Interrupt;

const ATTRIBUTES: &str = "* -text -eol -filter -ident -working-tree-encoding\n";

/// 複製の側にロックを置かないのは，データベースのロックとこの印で，
/// 1 つの複製を使う記録が 1 つに限られるからである．
const MARKER: &str = "cgweave-run";

/// 置き場所に何かあれば作らないのは，別の記録の複製や，利用者の作業中のリポジトリを上書きしないためである．
pub fn create(
    git: &Git,
    repo: &Path,
    clone: &Path,
    run_id: &str,
    interrupt: &Interrupt,
) -> Result<()> {
    let clone = std::path::absolute(clone)
        .with_context(|| format!("cannot resolve the clone directory {}", clone.display()))?;
    if clone.symlink_metadata().is_ok() {
        bail!(
            "the clone directory {} already exists; delete it or pass another --clone-dir",
            clone.display()
        );
    }
    git.output(
        repo,
        &[
            OsStr::new("clone"),
            OsStr::new("--shared"),
            OsStr::new("--no-checkout"),
            repo.as_os_str(),
            clone.as_os_str(),
        ],
        interrupt,
    )?;
    let info = clone.join(".git/info");
    std::fs::create_dir_all(&info).with_context(|| format!("cannot create {}", info.display()))?;
    let attributes = info.join("attributes");
    std::fs::write(&attributes, ATTRIBUTES)
        .with_context(|| format!("cannot write {}", attributes.display()))?;
    // 印を最後に書くのは，途中で止まった複製を再開で使わず，消して作り直させるためである．
    let marker = marker_path(&clone);
    std::fs::write(&marker, format!("{run_id}\n"))
        .with_context(|| format!("cannot write {}", marker.display()))?;
    Ok(())
}

pub fn reuse_or_create(
    git: &Git,
    repo: &Path,
    clone: &Path,
    run_id: &str,
    interrupt: &Interrupt,
) -> Result<()> {
    if clone.symlink_metadata().is_err() {
        return create(git, repo, clone, run_id, interrupt);
    }
    if !marked_by(clone, run_id)? {
        bail!(
            "{} is not the analysis clone of run {run_id}; delete it and resume again",
            clone.display()
        );
    }
    Ok(())
}

/// 印を確かめずに消さないのは，--clone-dir に別の記録の複製や利用者のリポジトリが渡されうるからである．
pub fn remove(clone: &Path, run_id: &str) -> Result<()> {
    if !marked_by(clone, run_id)? {
        bail!(
            "{} is not the analysis clone of run {run_id}; it is left as it is",
            clone.display()
        );
    }
    std::fs::remove_dir_all(clone).with_context(|| format!("cannot delete {}", clone.display()))
}

fn marker_path(clone: &Path) -> PathBuf {
    clone.join(".git").join(MARKER)
}

fn marked_by(clone: &Path, run_id: &str) -> Result<bool> {
    let marker = marker_path(clone);
    match std::fs::read(&marker) {
        Ok(contents) => Ok(contents == format!("{run_id}\n").as_bytes()),
        Err(err) if err.kind() == ErrorKind::NotFound || err.kind() == ErrorKind::NotADirectory => {
            Ok(false)
        }
        Err(err) => Err(err).with_context(|| format!("cannot read {}", marker.display())),
    }
}

/// clean に -x を付けるのは，前の版のファイルだけでなく，無視されるファイルも消すためである
pub fn checkout(git: &Git, clone: &Path, commit: &str, interrupt: &Interrupt) -> Result<()> {
    git.output(
        clone,
        &["checkout", "--force", "--detach", commit],
        interrupt,
    )?;
    git.output(clone, &["clean", "-ffdx"], interrupt)?;
    Ok(())
}

/// index の書き換えを確かめないのは，プラグインが .git を変えないことを約束事にしたからである．
pub fn check_unchanged(git: &Git, clone: &Path, interrupt: &Interrupt) -> Result<()> {
    let status = git.output(clone, &["status", "--porcelain", "--ignored"], interrupt)?;
    if let Some(first) = status.split(|&b| b == b'\n').find(|line| !line.is_empty()) {
        bail!(
            "the plugin changed the work tree of the analysis clone: {}",
            String::from_utf8_lossy(first)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testing::*;

    const RUN: &str = "6f1c2b8e-0d4a-4c51-9a3e-2f7b9c1d0e55";
    const OTHER: &str = "0b9e4c1a-7f3d-4e2b-8c6a-1d5f0a9b3c77";

    fn none() -> Interrupt {
        Interrupt::default()
    }

    #[test]
    fn a_new_clone_has_the_attributes_and_the_marker_but_no_files() {
        let dir = TempDir::new("create");
        let root = repo(&dir, "r");
        let clone = dir.path().join("e.db.clone");
        create(&Git::new(), &root, &clone, RUN, &none()).unwrap();
        assert_eq!(
            std::fs::read_to_string(clone.join(".git/info/attributes")).unwrap(),
            "* -text -eol -filter -ident -working-tree-encoding\n"
        );
        assert_eq!(
            std::fs::read_to_string(clone.join(".git/cgweave-run")).unwrap(),
            format!("{RUN}\n")
        );
        assert!(!clone.join("a.txt").exists());
        assert!(clone.join(".git/objects/info/alternates").exists());
    }

    #[test]
    fn a_new_record_refuses_anything_at_the_clone_directory() {
        let dir = TempDir::new("exists");
        let root = repo(&dir, "r");
        let clone = dir.path().join("c");
        std::fs::write(&clone, b"keep").unwrap();
        let err = create(&Git::new(), &root, &clone, RUN, &none()).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(std::fs::read(&clone).unwrap(), b"keep");
    }

    #[test]
    fn resuming_uses_only_the_clone_with_the_same_marker() {
        let dir = TempDir::new("resume");
        let root = repo(&dir, "r");
        let g = Git::new();
        // 何も無ければ作る
        let clone = dir.path().join("c");
        reuse_or_create(&g, &root, &clone, RUN, &none()).unwrap();
        assert!(marked_by(&clone, RUN).unwrap());
        // 印が同じなら使う
        reuse_or_create(&g, &root, &clone, RUN, &none()).unwrap();
        // 印が違えば止める
        let err = reuse_or_create(&g, &root, &clone, OTHER, &none()).unwrap_err();
        assert!(
            err.to_string().contains("is not the analysis clone of run"),
            "{err}"
        );
        // 印が無ければ止める．利用者の作業中のリポジトリを渡したときに当たる
        let err = reuse_or_create(&g, &root, &root, RUN, &none()).unwrap_err();
        assert!(
            err.to_string().contains("delete it and resume again"),
            "{err}"
        );
    }

    #[test]
    fn only_the_clone_with_the_same_marker_is_removed() {
        let dir = TempDir::new("remove");
        let root = repo(&dir, "r");
        let clone = dir.path().join("c");
        create(&Git::new(), &root, &clone, RUN, &none()).unwrap();
        assert!(remove(&clone, OTHER).is_err());
        assert!(clone.exists());
        assert!(remove(&root, RUN).is_err());
        assert!(root.join(".git").exists());
        remove(&clone, RUN).unwrap();
        assert!(!clone.exists());
    }

    #[test]
    fn checkout_removes_the_files_of_the_previous_version_and_ignored_files() {
        let dir = TempDir::new("checkout");
        let root = repo(&dir, "r");
        let first = git(&root, &["rev-parse", "HEAD"]);
        let second = commit(
            &root,
            &[
                ("b.txt", "b\n"),
                (".gitignore", "*.log\n"),
                ("crlf.txt", "x\r\n"),
            ],
            "second",
            "1700000100",
        );
        let clone = dir.path().join("c");
        let g = Git::new();
        create(&g, &root, &clone, RUN, &none()).unwrap();
        checkout(&g, &clone, &second, &none()).unwrap();
        assert_eq!(std::fs::read(clone.join("crlf.txt")).unwrap(), b"x\r\n");
        check_unchanged(&g, &clone, &none()).unwrap();
        std::fs::write(clone.join("out.log"), "ignored").unwrap();
        std::fs::write(clone.join("a.txt"), "changed").unwrap();
        checkout(&g, &clone, &first, &none()).unwrap();
        assert!(!clone.join("b.txt").exists());
        assert!(!clone.join("out.log").exists());
        assert_eq!(std::fs::read(clone.join("a.txt")).unwrap(), b"a\n");
        check_unchanged(&g, &clone, &none()).unwrap();
    }

    #[test]
    fn a_changed_work_tree_is_reported_with_its_first_line() {
        let dir = TempDir::new("status");
        let root = repo(&dir, "r");
        let second = commit(&root, &[(".gitignore", "*.log\n")], "ignore", "1700000100");
        let clone = dir.path().join("c");
        let g = Git::new();
        create(&g, &root, &clone, RUN, &none()).unwrap();
        checkout(&g, &clone, &second, &none()).unwrap();
        std::fs::write(clone.join("out.log"), "x").unwrap();
        let err = check_unchanged(&g, &clone, &none()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the plugin changed the work tree of the analysis clone: !! out.log"
        );
        checkout(&g, &clone, &second, &none()).unwrap();
        std::fs::write(clone.join("new.txt"), "x").unwrap();
        let err = check_unchanged(&g, &clone, &none()).unwrap_err();
        assert!(err.to_string().ends_with(": ?? new.txt"), "{err}");
    }
}
