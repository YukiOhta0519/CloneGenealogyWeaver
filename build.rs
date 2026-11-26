use std::path::Path;
use std::process::Command;

/// 走り直す条件と `.dirty` の条件を別の一覧にしないのは，片方だけを直して版の文字列が古いまま残るのを防ぐためである．
const WATCHED: &[&str] = &[
    "src",
    "schema",
    "tests",
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain.toml",
    "plugins/claim/claim.py",
    "plugins/claim/entry.py",
    "plugins/claim/requirements.txt",
];

fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    // 引き継いだ GIT_ の変数を残さないのは，GIT_DIR などが別のリポジトリを指していても，ツールのリポジトリを見るためである．git/mod.rs で Git を呼ぶときと同じ考えである
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(&name);
        }
    }
    // GIT_OPTIONAL_LOCKS を 0 にするのは，ビルドの git status が index を書き直さないようにするためである
    command.env("GIT_OPTIONAL_LOCKS", "0");
    command.arg("-C").arg(dir);
    command
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = git_command(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run git to make the tool version: {e}"));
    if !output.status.success() {
        panic!(
            "git {} failed while making the tool version: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout)
        .unwrap_or_else(|_| panic!("git {} printed non-UTF-8 output", args.join(" ")))
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    let hash = git(dir, &["rev-parse", "--short=12", "HEAD"]);
    // --untracked-files を利用者の設定に任せないのは，status.showUntrackedFiles=no で足したファイルが隠れるからである
    let mut status = vec!["status", "--porcelain", "--untracked-files=normal", "--"];
    status.extend_from_slice(WATCHED);
    let dirty = !git(dir, &status).is_empty();
    let version = format!(
        "{}+{}{}",
        env!("CARGO_PKG_VERSION"),
        hash.trim(),
        if dirty { ".dirty" } else { "" }
    );
    println!("cargo::rustc-env=CGWEAVE_VERSION={version}");

    // HEAD が指す枝のファイルも見るのは，コミットしただけで版を新しくするためである
    let mut rerun = Vec::new();
    for name in ["HEAD", "index", "packed-refs"] {
        rerun.push(
            git(dir, &["rev-parse", "--git-path", name])
                .trim()
                .to_string(),
        );
    }
    let branch = git_command(dir)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    if let Some(branch) = branch {
        rerun.push(
            git(dir, &["rev-parse", "--git-path", &branch])
                .trim()
                .to_string(),
        );
    }
    rerun.extend(WATCHED.iter().map(|path| path.to_string()));
    // 無いパスを渡さないのは，Cargo が毎回走り直すからである
    for path in rerun {
        if dir.join(&path).exists() {
            println!("cargo::rerun-if-changed={path}");
        }
    }
}
