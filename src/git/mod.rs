pub mod hunks;
pub mod raw;
pub mod revs;
pub mod worktree;

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use anyhow::{Context, Result, anyhow};

use crate::process::Interrupt;

/// 足すのは，利用者の Git の設定と環境で結果が変わらないようにするためである．
const ADDED_ENV: [(&str, &str); 5] = [
    ("GIT_NO_REPLACE_OBJECTS", "1"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("LC_ALL", "C"),
];

/// プラグインと違って新しいプロセスグループで起動しないのは，端末の Ctrl-C で Git も止めるためである．
/// 時間の上限は置かない．ツールだけに SIGTERM が来たときは Git が終わるのを待つ．
pub struct Git {
    env: Vec<(OsString, OsString)>,
}

impl Git {
    pub fn new() -> Git {
        Git {
            env: env_from(std::env::vars_os()),
        }
    }

    pub fn output<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        interrupt: &Interrupt,
    ) -> Result<Vec<u8>> {
        let output = self.run(dir, args, interrupt)?;
        if !output.status.success() {
            return Err(failure(args, &output));
        }
        Ok(output.stdout)
    }

    /// 失敗で割り込みを先に見るのは，Ctrl-C で Git も同じ信号で止まり，Git の失敗に見えるからである．
    pub fn run<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        interrupt: &Interrupt,
    ) -> Result<Output> {
        let output = self
            .command(dir, args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("cannot run git {}", subcommand(args)))?;
        if !output.stderr.is_empty() {
            log::debug!(
                "git {} wrote to stderr: {}",
                subcommand(args),
                String::from_utf8_lossy(&output.stderr).trim_end()
            );
        }
        if !output.status.success() {
            interrupt.check()?;
        }
        Ok(output)
    }

    pub fn spawn<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<Child> {
        self.command(dir, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("cannot run git {}", subcommand(args)))
    }

    /// `git -C` を使わないのは，作業ディレクトリを起動のときに渡せば足りるからである．
    fn command<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Command {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)));
        command
    }
}

/// 引き継いだ GIT_ の変数を残さないのは，-U0 より優先される GIT_DIFF_OPTS のように，
/// 結果を変える変数があるからである．
fn env_from(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = vars
        .filter(|(name, _)| {
            !name.as_encoded_bytes().starts_with(b"GIT_")
                && !ADDED_ENV.iter().any(|(added, _)| name == added)
        })
        .collect();
    env.extend(
        ADDED_ENV
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
    );
    env
}

/// 標準エラーの最後の行だけを入れるのは，Git が理由の fatal の行を最後に出すからである．
pub fn failure<S: AsRef<OsStr>>(args: &[S], output: &Output) -> anyhow::Error {
    let sub = subcommand(args);
    let how = match output.status.code() {
        Some(code) => format!("failed with exit code {code}"),
        None => {
            use std::os::unix::process::ExitStatusExt;
            format!(
                "was killed by signal {}",
                output.status.signal().unwrap_or(0)
            )
        }
    };
    match last_line(&output.stderr) {
        Some(line) => anyhow!("git {sub} {how}: {line}"),
        None => anyhow!("git {sub} {how}"),
    }
}

pub fn last_line(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

fn subcommand<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(AsRef::as_ref)
        .find(|arg| !arg.as_encoded_bytes().starts_with(b"-"))
        .or_else(|| args.first().map(AsRef::as_ref))
        .map(|arg| arg.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// trim を使わないのは，パスの末尾の空白を消さないためである
pub fn strip_newline(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
}

#[cfg(test)]
pub mod testing {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// tests/common の TempDir を使わないのは，tests/ の外からは読めないからである
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> TempDir {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "cgweave-git-{name}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(std::fs::canonicalize(path).unwrap())
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 使い捨てのリポジトリを作る Git．利用者の設定を読まず，名前と時刻を固定する
    pub fn git(dir: &Path, args: &[&str]) -> String {
        git_at(dir, args, "1700000000")
    }

    pub fn git_at(dir: &Path, args: &[&str], unix_time: &str) -> String {
        let date = format!("@{unix_time} +0000");
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_string()
    }

    /// 1 つのコミットのある作業ツリーを作り，その根を返す
    pub fn repo(dir: &TempDir, name: &str) -> PathBuf {
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        commit(&root, &[("a.txt", "a\n")], "first", "1700000000");
        root
    }

    /// ファイルを書いてコミットし，コミットのハッシュを返す
    pub fn commit(root: &Path, files: &[(&str, &str)], message: &str, unix_time: &str) -> String {
        for (path, contents) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        git(root, &["add", "-A"]);
        git_at(
            root,
            &["commit", "-q", "--allow-empty", "-m", message],
            unix_time,
        );
        git(root, &["rev-parse", "HEAD"])
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(n, v)| (OsString::from(n), OsString::from(v)))
            .collect()
    }

    #[test]
    fn git_variables_are_dropped_and_the_five_are_added() {
        let env = env_from(
            vars(&[
                ("HOME", "/h"),
                ("GIT_DIFF_OPTS", "--unified=3"),
                ("GIT_DIR", "/elsewhere"),
                ("LC_ALL", "ja_JP.UTF-8"),
                ("PATH", "/bin"),
            ])
            .into_iter(),
        );
        assert_eq!(
            env,
            vars(&[
                ("HOME", "/h"),
                ("PATH", "/bin"),
                ("GIT_NO_REPLACE_OBJECTS", "1"),
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
                ("GIT_TERMINAL_PROMPT", "0"),
                ("LC_ALL", "C"),
            ])
        );
    }

    #[test]
    fn a_failure_names_the_subcommand_the_code_and_the_last_line() {
        let dir = TempDir::new("failure");
        let root = repo(&dir, "r");
        let err = Git::new()
            .output(
                &root,
                &[
                    "--no-replace-objects",
                    "rev-parse",
                    "--verify",
                    "nosuch^{commit}",
                ],
                &Interrupt::default(),
            )
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "git rev-parse failed with exit code 128: fatal: Needed a single revision"
        );
    }

    #[test]
    fn a_failure_after_an_interrupt_is_an_interrupt() {
        let dir = TempDir::new("interrupted");
        let root = repo(&dir, "r");
        let interrupt = Interrupt::default();
        interrupt.set();
        let err = Git::new()
            .output(
                &root,
                &["rev-parse", "--verify", "nosuch^{commit}"],
                &interrupt,
            )
            .unwrap_err();
        assert!(err.is::<crate::error::Interrupted>(), "{err:#}");
    }

    #[test]
    fn last_line_skips_blank_lines() {
        assert_eq!(
            last_line(b"hint: a\nfatal: b  \n\n"),
            Some("fatal: b".to_string())
        );
        assert_eq!(last_line(b"\n \n"), None);
    }

    #[test]
    fn the_subcommand_is_the_first_argument_that_is_not_an_option() {
        assert_eq!(
            subcommand(&["--no-replace-objects", "diff", "--raw"]),
            "diff"
        );
        assert_eq!(subcommand(&["--version"]), "--version");
    }
}
