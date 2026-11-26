// テストの道具．
//
// 一時ディレクトリを作る，cgweave を動かす，などの共通の関数を置く．
// テストのファイルごとに使う関数が違うので，使わない関数の警告を止める．
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// テストの一時ディレクトリ．捨てると中身ごと消す．
/// 依存の crate を足さないので，標準ライブラリで作る．
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(name: &str) -> TempDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cgweave-test-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        if path.exists() {
            std::fs::remove_dir_all(&path).unwrap();
        }
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// ディレクトリの中にファイルを書いて，そのパスを返す．
    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// テストする cgweave の実行ファイル．Cargo がパスを渡す．
pub fn cgweave() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cgweave"))
}

/// cgweave を引数で動かし，終わるまで待つ．
pub fn run(args: &[&str]) -> Output {
    cgweave().args(args).output().unwrap()
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// リポジトリの schema/ の SQL を読む．
pub fn schema_sql(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("schema")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// 使い捨てのリポジトリを作る Git．利用者の設定を読まず，名前と時刻を固定する
pub fn git(dir: &Path, args: &[&str]) -> String {
    git_at(dir, args, 1_700_000_000)
}

/// 時刻を unix の秒で決めて Git を動かす．系譜の日数を確かめるのに使う
pub fn git_at(dir: &Path, args: &[&str], unix_secs: i64) -> String {
    let date = format!("@{unix_secs} +0000");
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

/// 1 つのコミットのある作業ツリーを dir の下の name に作り，その根を返す
pub fn repo(dir: &TempDir, name: &str) -> PathBuf {
    let root = dir.path().join(name);
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "first"]);
    root
}

/// コミットを足す．中身が None のファイルは消す
pub fn commit(root: &Path, files: &[(&str, Option<&str>)], message: &str) -> String {
    commit_at(root, files, message, 1_700_000_000)
}

/// committer の時刻を決めてコミットを足す
pub fn commit_at(
    root: &Path,
    files: &[(&str, Option<&str>)],
    message: &str,
    unix_secs: i64,
) -> String {
    for (path, contents) in files {
        let path = root.join(path);
        match contents {
            Some(contents) => {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, contents).unwrap();
            }
            None => std::fs::remove_file(path).unwrap(),
        }
    }
    git(root, &["add", "-A"]);
    git_at(
        root,
        &["commit", "-q", "--allow-empty", "-m", message],
        unix_secs,
    );
    git(root, &["rev-parse", "HEAD"])
}

/// 片を fixture の JSON の形にする
pub fn fragment_json(path: &str, start: u32, end: u32) -> String {
    format!(r#"{{"path": "{path}", "start": {start}, "end": {end}}}"#)
}

/// 片を (パス, 開始行, 終了行) で書く
pub type At<'a> = (&'a str, u32, u32);

/// 版，その版のペア，何回目まで失敗するか
pub type DetectorVersion<'a> = (&'a str, &'a [(At<'a>, At<'a>)], u32);

/// 版と，その版のサービスの名前とファイル
pub type ServiceVersion<'a> = (&'a str, &'a [(&'a str, &'a [&'a str])]);

/// 版ごとのペアの一覧と，何回目まで失敗するかから，検出器 fixture の JSON を作る
pub fn pairs_json(versions: &[DetectorVersion]) -> String {
    let entries: Vec<String> = versions
        .iter()
        .map(|(commit, pairs, fail)| {
            let pairs: Vec<String> = pairs
                .iter()
                .map(|(a, b)| {
                    format!(
                        r#"{{"a": {}, "b": {}}}"#,
                        fragment_json(a.0, a.1, a.2),
                        fragment_json(b.0, b.1, b.2)
                    )
                })
                .collect();
            format!(
                r#""{commit}": {{"pairs": [{}], "fail": {fail}}}"#,
                pairs.join(", ")
            )
        })
        .collect();
    format!(r#"{{"versions": {{{}}}}}"#, entries.join(", "))
}

/// 版ごとのサービスとファイルから，分類 fixture の JSON を作る
pub fn services_json(versions: &[ServiceVersion]) -> String {
    let entries: Vec<String> = versions
        .iter()
        .map(|(commit, services)| {
            let services: Vec<String> = services
                .iter()
                .map(|(name, files)| {
                    let files: Vec<String> = files.iter().map(|f| format!("\"{f}\"")).collect();
                    format!(r#""{name}": [{}]"#, files.join(", "))
                })
                .collect();
            format!(r#""{commit}": {{"services": {{{}}}}}"#, services.join(", "))
        })
        .collect();
    format!(r#"{{"versions": {{{}}}}}"#, entries.join(", "))
}

/// dir に fixture と設定とグラフを書いて記録する．services が None なら分類のプラグインを使わない
pub fn record(
    dir: &TempDir,
    repo: &Path,
    graph: &str,
    pairs: &str,
    services: Option<&str>,
    retries: u32,
) -> Output {
    record_env(dir, repo, graph, pairs, services, retries, &[])
}

/// 環境変数を足して記録する．利用者の環境で結果が変わらないかを確かめるのに使う
pub fn record_env(
    dir: &TempDir,
    repo: &Path,
    graph: &str,
    pairs: &str,
    services: Option<&str>,
    retries: u32,
    env: &[(&str, &str)],
) -> Output {
    dir.write("pairs.json", pairs);
    let mut config =
        format!("retries = {retries}\n[detector]\nname = \"fixture\"\nfile = \"pairs.json\"\n");
    if let Some(services) = services {
        dir.write("services.json", services);
        config.push_str("[classifiers.fixture]\nfile = \"services.json\"\n");
    }
    record_config(dir, repo, graph, &config, env)
}

/// 設定の中身をそのまま書いて記録する．設定が指す fixture などのファイルは，呼ぶ側が dir に書いておく
pub fn record_config(
    dir: &TempDir,
    repo: &Path,
    graph: &str,
    config: &str,
    env: &[(&str, &str)],
) -> Output {
    record_command(dir, repo, graph, config)
        .envs(env.iter().copied())
        .output()
        .unwrap()
}

/// 動かす前の record のコマンドを返す．引数を足して動かすのに使う
pub fn record_command(dir: &TempDir, repo: &Path, graph: &str, config: &str) -> Command {
    let config = dir.write("config.toml", config);
    let graph = dir.write("graph.txt", graph);
    let db = dir.path().join("e.db");
    let mut command = cgweave();
    command.args([
        "record",
        "--repo",
        repo.to_str().unwrap(),
        "--graph",
        graph.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
    ]);
    command
}

pub fn open(dir: &TempDir) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.path().join("e.db")).unwrap()
}

/// 結果の行を，値を文字列にした列の並びで返す
pub fn rows(conn: &rusqlite::Connection, sql: &str) -> Vec<Vec<String>> {
    let mut statement = conn.prepare(sql).unwrap();
    let columns = statement.column_count();
    statement
        .query_map([], |row| {
            (0..columns)
                .map(|i| {
                    Ok(match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => "NULL".to_string(),
                        rusqlite::types::ValueRef::Integer(n) => n.to_string(),
                        rusqlite::types::ValueRef::Real(x) => x.to_string(),
                        rusqlite::types::ValueRef::Text(t) => {
                            String::from_utf8_lossy(t).into_owned()
                        }
                        rusqlite::types::ValueRef::Blob(b) => format!("{b:?}"),
                    })
                })
                .collect()
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

pub fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// SQL が拒まない決まりを，記録した表から確かめる．
/// SQL に防御を足さずに writer とこの検査で保つと，2026-11-14 に決めた．
pub fn check_tables(conn: &rusqlite::Connection) {
    let none = |sql: &str, what: &str| {
        let bad = rows(conn, sql);
        assert!(bad.is_empty(), "{what}: {bad:?}");
    };
    none(
        "SELECT * FROM fragment WHERE start_line < 1 OR start_line > end_line",
        "fragments with a bad line range",
    );
    none(
        "SELECT p.id FROM pair p JOIN fragment_view a ON a.id = p.fragment_a JOIN fragment_view b ON b.id = p.fragment_b
         WHERE CAST(a.path AS BLOB) > CAST(b.path AS BLOB)
            OR (a.path = b.path AND (a.start_line, a.end_line) >= (b.start_line, b.end_line))",
        "pairs whose fragments are not in triple order",
    );
    none(
        "SELECT * FROM edge_file_issue
         WHERE (issue = 'hunk_too_large') <> (hunks IS NOT NULL AND hunks > 0)",
        "file issues with a wrong hunk count",
    );
    none(
        "SELECT id FROM version WHERE
           (status = 'ok' AND (failed_plugin IS NOT NULL OR failure_message IS NOT NULL
                               OR pair_count IS NULL OR fragment_count IS NULL))
        OR (status = 'broken' AND (failed_plugin IS NULL OR failure_message IS NULL
                                   OR pair_count IS NOT NULL OR fragment_count IS NOT NULL OR root IS NOT NULL))
        OR (status = 'pending' AND (failed_plugin IS NOT NULL OR failure_message IS NOT NULL
                                    OR pair_count IS NOT NULL OR fragment_count IS NOT NULL OR root IS NOT NULL))",
        "versions whose columns do not match the status",
    );
    let labelers: Vec<String> = rows(
        conn,
        "SELECT p.name FROM plugin p JOIN sqlite_master m ON m.name = p.name || '_pair_event'
         WHERE p.kind = 'classifier'",
    )
    .into_iter()
    .map(|row| row[0].clone())
    .collect();
    for name in labelers {
        none(
            &format!(
                "SELECT e.id FROM pair_event e
                 WHERE (SELECT count(*) FROM {name}_pair_event l WHERE l.pair_event = e.id)
                       <> (e.event <> 'Disappear')"
            ),
            &format!("pair events without exactly one {name} label"),
        );
        none(
            &format!(
                "SELECT l.pair_event FROM {name}_pair_event l JOIN pair_event e ON e.id = l.pair_event
                 WHERE (l.changed IS NOT NULL) <> (e.event = 'Persist')"
            ),
            &format!("{name} labels whose changed mark is not only on Persist"),
        );
        none(
            &format!(
                "SELECT r.pair FROM root_pair r
                 WHERE (SELECT count(*) FROM {name}_root_pair l
                        WHERE l.version = r.version AND l.pair = r.pair) <> 1"
            ),
            &format!("root pairs without exactly one {name} label"),
        );
    }
}

/// 記録した ok の版ごとに，pairs で組み立てたペアが，fixture が返したペアと同じかを確かめる．
/// 書かないイベントの組み立ての誤りに気づくためである．
pub fn check_rebuilt(dir: &TempDir) {
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("pairs.json")).unwrap()).unwrap();
    let conn = open(dir);
    let commits: Vec<String> = rows(&conn, "SELECT commit_id FROM version WHERE status = 'ok'")
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    let triple = |f: &serde_json::Value| {
        (
            f["path"].as_str().unwrap().to_string(),
            f["start"].as_u64().unwrap(),
            f["end"].as_u64().unwrap(),
        )
    };
    for commit in commits {
        let mut expected: Vec<_> = fixture["versions"][&commit]["pairs"]
            .as_array()
            .map(|pairs| {
                pairs
                    .iter()
                    .map(|p| {
                        let (a, b) = (triple(&p["a"]), triple(&p["b"]));
                        if a < b { (a, b) } else { (b, a) }
                    })
                    .collect()
            })
            .unwrap_or_default();
        expected.sort();
        expected.dedup();
        let output = run(&[
            "pairs",
            "--db",
            dir.path().join("e.db").to_str().unwrap(),
            "--commit",
            &commit,
        ]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let got: Vec<_> = stdout(&output)
            .lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                (triple(&value["a"]), triple(&value["b"]))
            })
            .collect();
        assert_eq!(got, expected, "pairs of {commit}");
    }
}

/// 系譜のファイルを作る．出力は dir の g.db にする
pub fn genealogy(dir: &TempDir) -> Output {
    run(&[
        "genealogy",
        "--db",
        dir.path().join("e.db").to_str().unwrap(),
        "--output",
        dir.path().join("g.db").to_str().unwrap(),
    ])
}
