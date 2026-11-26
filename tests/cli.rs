// 引数，終了コード，版の表示，骨組みの段の record のテスト．
//
// cgweave の実行ファイルを動かして確かめる．

mod common;

use common::{TempDir, git, repo, run, stderr, stdout};

const DETECTOR_ONLY: &str = "[detector]\nname = \"fixture\"\nfile = \"pairs.json\"\n";

#[test]
fn version_shows_the_tool_version_and_plugins() {
    let output = run(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].starts_with("cgweave 0.1.0+"), "{text}");
    let hash = lines[0]
        .trim_start_matches("cgweave 0.1.0+")
        .trim_end_matches(".dirty");
    assert_eq!(hash.len(), 12, "{text}");
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()), "{text}");
    assert_eq!(lines[1], "detectors: fixture, msccd");
    assert_eq!(
        lines[2],
        "classifiers: claim, fixture, generated, test_path, vendor"
    );
    assert!(output.stderr.is_empty());

    assert_eq!(stdout(&run(&["-V"])), text);
    // コマンドの後でも出せる．先頭の名前は clap が cgweave-record にする
    let after = run(&["record", "--version"]);
    assert_eq!(after.status.code(), Some(0));
    assert!(stdout(&after).ends_with(&text["cgweave".len()..]));
}

#[test]
fn argument_errors_exit_with_2() {
    for args in [
        vec!["record", "--db", "e.db"],
        vec!["status", "--db", "e.db", "--log-level", "loud"],
        vec!["status", "--db", "e.db", "--unknown"],
        vec!["pairs", "--db", "e.db", "--version-of", "x"],
        vec!["status", "e.db"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn log_level_works_before_and_after_the_command() {
    for args in [
        vec!["--log-level", "debug", "status", "--db", "e.db"],
        vec!["status", "--db", "e.db", "--log-level", "debug"],
    ] {
        let output = run(&args);
        // status はまだ無いので 1 で止まるが，引数は受け付けられている
        assert_eq!(output.status.code(), Some(1), "{args:?}");
    }
}

fn record(dir: &TempDir, config: &str) -> std::process::Output {
    let root = repo(dir, "repo");
    record_in(dir, &root, config)
}

fn record_in(dir: &TempDir, repo: &std::path::Path, config: &str) -> std::process::Output {
    let head = git(repo, &["rev-parse", "HEAD"]);
    record_with_graph(dir, repo, config, &format!("{head}\n"))
}

fn record_with_graph(
    dir: &TempDir,
    repo: &std::path::Path,
    config: &str,
    graph: &str,
) -> std::process::Output {
    let graph = dir.write("graph.txt", graph);
    let config = dir.write("config.toml", config);
    dir.write("pairs.json", r#"{"versions": {}}"#);
    let db = dir.path().join("e.db");
    run(&[
        "record",
        "--repo",
        repo.to_str().unwrap(),
        "--graph",
        graph.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
    ])
}

#[test]
fn record_finishes_a_version_without_pairs_and_cleans_up() {
    let dir = TempDir::new("record-create");
    let output = record(&dir, DETECTOR_ONLY);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("[1/1] "), "{}", stderr(&output));

    let db = dir.path().join("e.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let id: i32 = conn
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .unwrap();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(id, 0x4347_5745);
    assert_eq!(version, 1);
    let journal: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
    let status: String = conn
        .query_row("SELECT status FROM run", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "finished");
    // 記録が終わったので，ロック，複製，一時ファイルのディレクトリを片づける
    for side in ["e.db.lock", "e.db.clone", "e.db.tmp", "e.db-wal"] {
        assert!(!dir.path().join(side).exists(), "{side}");
    }
}

#[test]
fn record_stops_on_a_plugin_item_error_before_making_the_database() {
    let dir = TempDir::new("record-plugin-item");
    let output = record(
        &dir,
        &format!("{DETECTOR_ONLY}[classifiers.vendor]\nwords = 1\n"),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("invalid config")
            && stderr(&output).contains("[classifiers.vendor] unknown item `words` for vendor"),
        "{}",
        stderr(&output)
    );
    assert!(!dir.path().join("e.db").exists());
}

#[test]
fn record_refuses_an_existing_database() {
    let dir = TempDir::new("record-exists");
    std::fs::write(dir.path().join("e.db"), b"keep").unwrap();
    let output = record(&dir, DETECTOR_ONLY);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("already exists; pass --resume"),
        "{}",
        stderr(&output)
    );
    assert_eq!(std::fs::read(dir.path().join("e.db")).unwrap(), b"keep");
}

#[test]
fn record_checks_the_config_before_creating_files() {
    let dir = TempDir::new("record-config");
    let output = record(&dir, &format!("retry = 3\n{DETECTOR_ONLY}"));
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("error: invalid config "), "{message}");
    assert!(message.contains("retry"), "{message}");
    assert_eq!(message.lines().count(), 1, "{message}");
    assert!(!dir.path().join("e.db").exists());
    assert!(!dir.path().join("e.db.lock").exists());
}

#[test]
fn record_resume_needs_an_existing_database() {
    let dir = TempDir::new("record-resume");
    let config = dir.write("config.toml", DETECTOR_ONLY);
    let db = dir.path().join("e.db");
    let output = run(&[
        "record",
        "--repo",
        ".",
        "--graph",
        "graph.txt",
        "--config",
        config.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--resume",
        "abcd",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("does not exist; nothing to resume"));
}

#[test]
fn record_refuses_a_subdirectory_of_a_work_tree_before_creating_files() {
    let dir = TempDir::new("record-subdir");
    let root = repo(&dir, "repo");
    std::fs::create_dir(root.join("sub")).unwrap();
    let output = record_in(&dir, &root.join("sub"), DETECTOR_ONLY);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("is not the root of a work tree or a bare repository; its root is"),
        "{message}"
    );
    assert!(!dir.path().join("e.db").exists());
    assert!(!dir.path().join("e.db.lock").exists());
}

#[test]
fn record_refuses_a_shallow_repository_before_creating_files() {
    let dir = TempDir::new("record-shallow");
    let root = repo(&dir, "repo");
    std::fs::write(root.join("b.txt"), "b\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "second"]);
    let url = format!("file://{}", root.display());
    git(
        dir.path(),
        &["clone", "-q", "--depth", "1", &url, "shallow"],
    );
    let output = record_in(&dir, &dir.path().join("shallow"), DETECTOR_ONLY);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("is a shallow repository"),
        "{}",
        stderr(&output)
    );
    assert!(!dir.path().join("e.db").exists());
}

#[test]
fn record_accepts_a_bare_repository() {
    let dir = TempDir::new("record-bare");
    repo(&dir, "repo");
    git(dir.path(), &["clone", "-q", "--bare", "repo", "bare.git"]);
    let output = record_in(&dir, &dir.path().join("bare.git"), DETECTOR_ONLY);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn record_checks_the_graph_before_creating_files() {
    let dir = TempDir::new("record-graph");
    let root = repo(&dir, "repo");
    let output = record_with_graph(&dir, &root, DETECTOR_ONLY, "\nffff\n");
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("graph.txt at line 2: cannot resolve \"ffff\" to a commit: fatal: Needed a single revision"),
        "{message}"
    );
    assert!(!dir.path().join("e.db").exists());
    assert!(!dir.path().join("e.db.lock").exists());
}

#[test]
fn record_refuses_a_short_hash_taken_by_a_tag_of_another_commit() {
    let dir = TempDir::new("record-tag");
    let root = repo(&dir, "repo");
    let first = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "second"]);
    let second = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["tag", &second[..8], &first]);
    let graph = format!("{first}\n{} {first}\n", &second[..8]);
    let output = record_with_graph(&dir, &root, DETECTOR_ONLY, &graph);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains(&format!(
            "at line 2: \"{}\" resolved to {}, which does not start with it",
            &second[..8],
            &first[..12]
        )),
        "{message}"
    );
    assert!(!dir.path().join("e.db").exists());
}
