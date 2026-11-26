// 途中で止めて再開した記録と，止めずに動かした記録が同じか，再開と読み出しのコマンドの止まり方．

mod common;

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Stdio;

use common::*;

fn words(prefix: &str, n: u32) -> String {
    (1..=n).map(|i| format!("{prefix}{i} = {i};\n")).collect()
}

/// 版を n 個並べた一列の場面．版ごとに a.java の 1 行を書き換え，ペアの片はどれも続く
struct Scene {
    root: std::path::PathBuf,
    commits: Vec<String>,
}

fn scene(dir: &TempDir, n: usize) -> Scene {
    let root = repo(dir, "repo");
    let text = words("a", 10);
    let mut commits = vec![commit(
        &root,
        &[
            ("A.java", Some(&text)),
            ("B.java", Some(&text)),
            ("C.txt", Some("0")),
        ],
        "v0",
    )];
    for i in 1..n {
        let changed = text.replace("a5 = 5;", &format!("a5 = {};", 5 + i));
        commits.push(commit(
            &root,
            &[("A.java", Some(&changed)), ("C.txt", Some(&i.to_string()))],
            &format!("v{i}"),
        ));
    }
    Scene { root, commits }
}

fn graph_of(commits: &[String]) -> String {
    let mut text = format!("{}\n", commits[0]);
    for pair in commits.windows(2) {
        text.push_str(&format!("{} {}\n", pair[1], pair[0]));
    }
    text
}

/// 版ごとのペア．bad の版には範囲の外の片を返させ，止める誤りを起こす
fn pairs_for(commits: &[String], bad: Option<usize>, fail_at: Option<usize>) -> String {
    let good: &[(At, At)] = &[
        (("A.java", 1, 10), ("B.java", 1, 10)),
        (("A.java", 2, 4), ("B.java", 2, 4)),
    ];
    let wrong: &[(At, At)] = &[(("A.java", 1, 10), ("Z.java", 1, 1))];
    let versions: Vec<DetectorVersion> = commits
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let pairs = if Some(i) == bad { wrong } else { good };
            (c.as_str(), pairs, u32::from(Some(i) == fail_at))
        })
        .collect();
    pairs_json(&versions)
}

/// 分類 fixture．fail_scope_at の版では scope が 1 回失敗してからやり直して通る
fn services_for(commits: &[String], fail_scope_at: usize) -> String {
    let entries: Vec<String> = commits
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                r#""{c}": {{"services": {{"s": ["A.java"], "t": ["B.java", "C.txt"]}}, "fail_scope": {}}}"#,
                u32::from(i == fail_scope_at)
            )
        })
        .collect();
    format!(r#"{{"versions": {{{}}}}}"#, entries.join(", "))
}

fn resume(dir: &TempDir, repo: &Path, rev: &str) -> std::process::Output {
    run(&[
        "record",
        "--repo",
        repo.to_str().unwrap(),
        "--graph",
        dir.path().join("graph.txt").to_str().unwrap(),
        "--config",
        dir.path().join("config.toml").to_str().unwrap(),
        "--db",
        dir.path().join("e.db").to_str().unwrap(),
        "--resume",
        rev,
    ])
}

/// run と run_resume と plugin の設定を除いて全部の表を読む．plugin の設定は fixture の絶対パスと SHA-256 を持つ
fn dump(dir: &TempDir) -> Vec<(String, Vec<Vec<String>>)> {
    let conn = open(dir);
    let tables: Vec<String> = rows(
        &conn,
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
    )
    .into_iter()
    .map(|row| row[0].clone())
    .filter(|name| name != "run" && name != "run_resume")
    .collect();
    tables
        .into_iter()
        .map(|table| {
            let sql = if table == "plugin" {
                "SELECT id, kind, name FROM plugin".to_string()
            } else {
                format!("SELECT * FROM {table}")
            };
            let mut all = rows(&conn, &sql);
            all.sort();
            (table, all)
        })
        .collect()
}

fn next_of(stderr: &str) -> String {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("next: "))
        .unwrap_or_else(|| panic!("no next line in {stderr}"))
        .to_string()
}

/// 止めずに動かした記録を，同じリポジトリと同じ fixture で作る
fn uninterrupted(scene: &Scene, pairs: &str, services: &str) -> Vec<(String, Vec<Vec<String>>)> {
    let dir = TempDir::new("whole");
    let output = record(
        &dir,
        &scene.root,
        &graph_of(&scene.commits),
        pairs,
        Some(services),
        2,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    dump(&dir)
}

#[test]
fn a_record_stopped_by_an_error_resumes_to_the_same_tables() {
    let dir = TempDir::new("resume-error");
    let s = scene(&dir, 6);
    // 再開する版の親に scope の予定の失敗を置くのは，範囲を求め直すときにも同じ失敗が出るからである
    let services = services_for(&s.commits, 3);
    let good = pairs_for(&s.commits, None, Some(1));
    // 4 番目の版でアダプタの誤りを起こして止める
    let output = record(
        &dir,
        &s.root,
        &graph_of(&s.commits),
        &pairs_for(&s.commits, Some(4), Some(1)),
        Some(&services),
        2,
    );
    assert_eq!(output.status.code(), Some(1));
    let next = next_of(&stderr(&output));
    assert_eq!(next, s.commits[4]);
    dir.write("pairs.json", &good);
    let output = resume(&dir, &s.root, &next[..10]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let conn = open(&dir);
    check_tables(&conn);
    assert_eq!(
        rows(&conn, "SELECT status, stop_message FROM run"),
        [["finished", "NULL"]]
    );
    assert_eq!(rows(&conn, "SELECT from_version FROM run_resume"), [["5"]]);
    // 範囲を求め直したときの scope の予定の失敗は書かない
    assert_eq!(count(&conn, "SELECT count(*) FROM plugin_failure"), 2);
    check_rebuilt(&dir);
    assert_eq!(dump(&dir), uninterrupted(&s, &good, &services));
    for side in ["e.db.lock", "e.db.clone", "e.db.tmp"] {
        assert!(!dir.path().join(side).exists(), "{side}");
    }
}

#[test]
fn a_record_stopped_by_a_signal_resumes_to_the_same_tables() {
    let dir = TempDir::new("resume-signal");
    let s = scene(&dir, 40);
    let services = services_for(&s.commits, 2);
    let pairs = pairs_for(&s.commits, None, Some(1));
    dir.write("pairs.json", &pairs);
    dir.write("services.json", &services);
    dir.write(
        "config.toml",
        "retries = 2\n[detector]\nname = \"fixture\"\nfile = \"pairs.json\"\n[classifiers.fixture]\nfile = \"services.json\"\n",
    );
    dir.write("graph.txt", &graph_of(&s.commits));
    let mut child = cgweave()
        .args([
            "record",
            "--repo",
            s.root.to_str().unwrap(),
            "--graph",
            dir.path().join("graph.txt").to_str().unwrap(),
            "--config",
            dir.path().join("config.toml").to_str().unwrap(),
            "--db",
            dir.path().join("e.db").to_str().unwrap(),
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr_text = String::new();
    let mut reader = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        stderr_text.push_str(&line);
        if line.contains("[5/40]") {
            let status = std::process::Command::new("kill")
                .args(["-INT", &child.id().to_string()])
                .status()
                .unwrap();
            assert!(status.success());
            break;
        }
        line.clear();
    }
    reader.read_to_string(&mut stderr_text).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130), "{stderr_text}");
    assert!(stderr_text.contains("interrupted"), "{stderr_text}");
    let conn = open(&dir);
    assert_eq!(rows(&conn, "SELECT status FROM run"), [["running"]]);
    drop(conn);
    let next = next_of(&stderr_text);
    let output = resume(&dir, &s.root, &next);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    check_tables(&open(&dir));
    assert_eq!(dump(&dir), uninterrupted(&s, &pairs, &services));
}

#[test]
fn resuming_stops_when_the_inputs_or_the_record_do_not_match() {
    let dir = TempDir::new("resume-refuse");
    let s = scene(&dir, 5);
    let services = services_for(&s.commits, 9);
    let output = record(
        &dir,
        &s.root,
        &graph_of(&s.commits),
        &pairs_for(&s.commits, Some(3), None),
        Some(&services),
        2,
    );
    assert_eq!(output.status.code(), Some(1));
    dir.write("pairs.json", &pairs_for(&s.commits, None, None));
    let graph = std::fs::read(dir.path().join("graph.txt")).unwrap();
    let config = std::fs::read(dir.path().join("config.toml")).unwrap();
    let expect = |rev: &str, message: &str| {
        let output = resume(&dir, &s.root, rev);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(stderr(&output).contains(message), "{}", stderr(&output));
    };

    std::fs::write(dir.path().join("graph.txt"), [&graph[..], b"\n"].concat()).unwrap();
    expect(&s.commits[3], "the graph file differ");
    std::fs::write(dir.path().join("graph.txt"), &graph).unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        [&config[..], b"# x\n"].concat(),
    )
    .unwrap();
    expect(&s.commits[3], "the config file differ");
    std::fs::write(dir.path().join("config.toml"), &config).unwrap();
    let tamper = |sql: &str| open(&dir).execute(sql, []).unwrap();
    tamper("UPDATE run SET tool_version = 'old', git_version = 'git version 1.0'");
    expect(&s.commits[3], "the tool version (");
    expect(&s.commits[3], "the Git version (");
    let output = run(&["--version"]);
    let tool = stdout(&output)
        .lines()
        .next()
        .unwrap()
        .trim_start_matches("cgweave ")
        .to_string();
    let git_version = git(&s.root, &["--version"]);
    tamper(&format!(
        "UPDATE run SET tool_version = '{tool}', git_version = '{git_version}'"
    ));
    // 版の文字列と，前に pending がある版
    expect("zzzz", "is not a commit hash");
    expect(&s.commits[4], "before it is not recorded");
    // 範囲のハッシュが記録と違う
    tamper("UPDATE version SET scope_sha256 = 'x' WHERE id = 3");
    expect(&s.commits[3], "differs from the record");
}

#[test]
fn a_finished_record_cannot_be_resumed() {
    let dir = TempDir::new("resume-finished");
    let s = scene(&dir, 2);
    let output = record(
        &dir,
        &s.root,
        &graph_of(&s.commits),
        &pairs_for(&s.commits, None, None),
        None,
        2,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = resume(&dir, &s.root, &s.commits[1]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("the record is finished"),
        "{}",
        stderr(&output)
    );
    // ジャーナルを WAL に切り替えず，ロックのファイルも作らない
    let conn = open(&dir);
    let journal: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
    assert!(!dir.path().join("e.db.lock").exists());
}

#[test]
fn pairs_finds_a_version_by_a_short_hash_and_stops_on_broken_ones() {
    let dir = TempDir::new("pairs");
    let s = scene(&dir, 3);
    // 2 番目の版は 3 回とも失敗して壊れた版になる
    let good: &[(At, At)] = &[
        (("A.java", 1, 10), ("B.java", 1, 10)),
        (("A.java", 2, 4), ("B.java", 2, 4)),
    ];
    let versions: Vec<DetectorVersion> = s
        .commits
        .iter()
        .enumerate()
        .map(|(i, c)| (c.as_str(), good, if i == 1 { 3 } else { 0 }))
        .collect();
    let output = record(
        &dir,
        &s.root,
        &graph_of(&s.commits),
        &pairs_json(&versions),
        None,
        2,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let db = dir.path().join("e.db");
    let db = db.to_str().unwrap();
    let output = run(&["pairs", "--db", db, "--commit", &s.commits[2][..7]]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let first = stdout(&output).lines().next().unwrap().to_string();
    assert_eq!(
        first,
        r#"{"a":{"path":"A.java","start":1,"end":10},"b":{"path":"B.java","start":1,"end":10},"labels":{},"similarity":null}"#
    );
    let output = run(&["pairs", "--db", db, "--commit", &s.commits[1]]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("is broken"), "{}", stderr(&output));
    // 記録の版の中で曖昧な短いハッシュ
    let conn = open(&dir);
    conn.execute(
        "UPDATE version SET commit_id = 'abcd' || substr(commit_id, 5) WHERE id IN (1, 3)",
        [],
    )
    .unwrap();
    drop(conn);
    let output = run(&["pairs", "--db", db, "--commit", "abcd"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("abcd is ambiguous; 2 recorded versions"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn pairs_ends_quietly_when_the_reader_closes_early() {
    let dir = TempDir::new("pairs-head");
    let root = repo(&dir, "repo");
    let text = words("a", 400);
    let c = commit(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "c",
    );
    let many: Vec<(At, At)> = (1..=400)
        .map(|i| (("A.java", i, i), ("B.java", i, i)))
        .collect();
    let output = record(
        &dir,
        &root,
        &format!("{c}\n"),
        &pairs_json(&[(&c, &many, 0)]),
        None,
        2,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let mut child = cgweave()
        .args([
            "pairs",
            "--db",
            dir.path().join("e.db").to_str().unwrap(),
            "--commit",
            &c,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(line.starts_with(r#"{"a":"#));
}

#[test]
fn status_shows_the_state_and_how_to_resume() {
    let dir = TempDir::new("status");
    let s = scene(&dir, 4);
    let output = record(
        &dir,
        &s.root,
        &graph_of(&s.commits),
        &pairs_for(&s.commits, Some(2), None),
        None,
        2,
    );
    assert_eq!(output.status.code(), Some(1));
    let db = dir.path().join("e.db");
    let output = run(&["status", "--db", db.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "status: failed");
    assert_eq!(lines[1], "versions: 4 total, 2 ok, 0 broken, 2 pending");
    assert!(
        lines[2].starts_with("stopped: invalid detector output"),
        "{text}"
    );
    assert_eq!(lines[3], format!("next: {}", s.commits[2]));
    assert_eq!(
        lines[4],
        format!(
            "resume: cgweave record --repo {} --graph <graph> --config <config> --db {} --resume {}",
            std::fs::canonicalize(&s.root).unwrap().display(),
            db.display(),
            s.commits[2]
        )
    );
}

#[test]
fn the_journal_goes_back_to_delete_after_a_reader_closes_within_five_seconds() {
    for keep_open in [false, true] {
        let dir = TempDir::new("journal");
        let s = scene(&dir, 3);
        let output = record(
            &dir,
            &s.root,
            &graph_of(&s.commits),
            &pairs_for(&s.commits, Some(2), None),
            None,
            2,
        );
        assert_eq!(output.status.code(), Some(1));
        dir.write("pairs.json", &pairs_for(&s.commits, None, None));
        let reader = rusqlite::Connection::open_with_flags(
            dir.path().join("e.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        // 1 回読んだ接続が開いている間は，ジャーナルを DELETE に戻せない
        let _: i64 = reader
            .query_row("SELECT count(*) FROM version", [], |r| r.get(0))
            .unwrap();
        let child = cgweave()
            .args([
                "record",
                "--repo",
                s.root.to_str().unwrap(),
                "--graph",
                dir.path().join("graph.txt").to_str().unwrap(),
                "--config",
                dir.path().join("config.toml").to_str().unwrap(),
                "--db",
                dir.path().join("e.db").to_str().unwrap(),
                "--resume",
                &s.commits[2],
            ])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let reader = if keep_open {
            Some(reader)
        } else {
            drop(reader);
            None
        };
        let output = child.wait_with_output().unwrap();
        drop(reader);
        let text = String::from_utf8_lossy(&output.stderr);
        let conn = open(&dir);
        assert_eq!(
            rows(&conn, "SELECT status FROM run"),
            [["finished"]],
            "{text}"
        );
        assert!(!dir.path().join("e.db.clone").exists());
        assert!(!dir.path().join("e.db.lock").exists());
        if keep_open {
            assert_eq!(output.status.code(), Some(1), "{text}");
            assert!(
                text.contains("cannot switch the journal back to DELETE"),
                "{text}"
            );
            assert!(text.contains("-wal"), "{text}");
        } else {
            assert_eq!(output.status.code(), Some(0), "{text}");
            let journal: String = conn
                .pragma_query_value(None, "journal_mode", |r| r.get(0))
                .unwrap();
            assert_eq!(journal, "delete");
        }
    }
}
