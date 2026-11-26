// 記録したイベントのデータベースから系譜のファイルを作って確かめる．

mod common;

use common::*;

const T0: i64 = 1_700_000_000;
const DAY: i64 = 86_400;

fn words(prefix: &str, n: u32) -> String {
    (1..=n).map(|i| format!("{prefix}{i} = {i};\n")).collect()
}

fn ok(output: &std::process::Output) {
    assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
}

/// 系譜のファイルを開き，イベントのデータベースを events の名前でつなぐ
fn open_genealogy(dir: &TempDir) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(dir.path().join("g.db")).unwrap();
    conn.execute(
        "ATTACH DATABASE ?1 AS events",
        [dir.path().join("e.db").to_str().unwrap()],
    )
    .unwrap();
    conn
}

/// どのペアイベントも，ちょうど 1 つの系譜に入っている
fn check_events(conn: &rusqlite::Connection) {
    assert_eq!(
        count(conn, "SELECT count(*) FROM events.pair_event"),
        count(conn, "SELECT count(*) FROM genealogy_event")
    );
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM genealogy_event g LEFT JOIN events.pair_event e ON e.id = g.pair_event
             WHERE e.id IS NULL"
        ),
        0
    );
    assert_eq!(
        count(
            conn,
            "SELECT count(*) FROM genealogy g
             WHERE root_count <> (SELECT count(*) FROM genealogy_root r WHERE r.genealogy = g.id)"
        ),
        0
    );
}

#[test]
fn a_resolved_and_a_censored_genealogy_from_the_empty_tree() {
    let dir = TempDir::new("genealogy-chain");
    let root = repo(&dir, "repo");
    let first = git(&root, &["rev-parse", "HEAD"]);
    let text = words("a", 6);
    let one = commit_at(
        &root,
        &[
            ("A.java", Some(&text)),
            ("B.java", Some(&text)),
            ("C.java", Some(&text)),
            ("D.java", Some(&text)),
        ],
        "one",
        T0 + DAY,
    );
    let two = commit_at(&root, &[("README.md", Some("x\n"))], "two", T0 + 2 * DAY);
    let three = commit_at(&root, &[("B.java", None)], "three", T0 + 4 * DAY);
    let ab = (("A.java", 1, 6), ("B.java", 1, 6));
    let cd = (("C.java", 1, 6), ("D.java", 1, 6));
    let pairs = pairs_json(&[
        (&one, &[ab, cd], 0),
        (&two, &[ab, cd], 0),
        (&three, &[cd], 0),
    ]);
    let graph = format!("{first}\n{one} {first}\n{two} {one}\n{three} {two}\n");
    ok(&record(&dir, &root, &graph, &pairs, None, 0));
    ok(&genealogy(&dir));

    let conn = open_genealogy(&dir);
    check_events(&conn);
    assert_eq!(
        count(&conn, "PRAGMA main.application_id"),
        i64::from(i32::from_be_bytes(*b"CGWG"))
    );
    assert_eq!(count(&conn, "PRAGMA main.user_version"), 1);
    assert_eq!(
        rows(&conn, "SELECT * FROM genealogy ORDER BY id"),
        [
            ["1", "1", "2", "appear", "0", "3", "2", "2"],
            ["2", "1", "2", "appear", "1", "3", "2", "2"],
        ]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT g.genealogy, e.event, g.role, a.path FROM genealogy_event g
             JOIN events.pair_event e ON e.id = g.pair_event
             JOIN events.pair p ON p.id = coalesce(e.parent_pair, e.child_pair)
             JOIN events.fragment_view a ON a.id = p.fragment_a ORDER BY e.id"
        ),
        [
            ["1", "Appear", "birth", "A.java"],
            ["2", "Appear", "birth", "C.java"],
            ["1", "Disappear", "end", "A.java"],
        ]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT t.genealogy, v.commit_id FROM genealogy_censored_tip t
             JOIN events.version v ON v.id = t.version"
        ),
        [["2".to_string(), three.clone()]]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT version, effective_at FROM version_time ORDER BY version"
        ),
        [
            ["1", "2023-11-14T22:13:20Z"],
            ["2", "2023-11-15T22:13:20Z"],
            ["3", "2023-11-16T22:13:20Z"],
            ["4", "2023-11-18T22:13:20Z"],
        ]
    );
    let run = &rows(
        &conn,
        "SELECT g.events_run_id = r.run_id, g.events_path, g.schema_version FROM genealogy_run g, events.run r",
    )[0];
    assert_eq!(run[0], "1");
    let canonical = std::fs::canonicalize(dir.path().join("e.db")).unwrap();
    assert_eq!(run[1], canonical.to_str().unwrap());
    assert_eq!(run[2], "1");
    // 書き終えた後に一時ファイルが残らない
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(names.is_empty(), "{names:?}");
}

#[test]
fn a_branch_and_a_broken_version_change_the_end_and_the_lengths() {
    // グラフでは two と side がどちらも one の子で，three は壊れた版である．four の時刻は three より前にする
    let dir = TempDir::new("genealogy-branch");
    let root = repo(&dir, "repo");
    let first = git(&root, &["rev-parse", "HEAD"]);
    let text = words("a", 6);
    let one = commit_at(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "one",
        T0 + DAY,
    );
    let two = commit_at(&root, &[("x.txt", Some("2\n"))], "two", T0 + 2 * DAY);
    let side = commit_at(&root, &[("y.txt", Some("s\n"))], "side", T0 + 3 * DAY);
    let three = commit_at(&root, &[("z.txt", Some("3\n"))], "three", T0 + 5 * DAY);
    let four = commit_at(&root, &[("w.txt", Some("4\n"))], "four", T0 + 4 * DAY);
    let ab = (("A.java", 1, 6), ("B.java", 1, 6));
    let pairs = pairs_json(&[
        (&one, &[ab], 0),
        (&two, &[], 0),
        (&side, &[ab], 0),
        (&three, &[ab], 1),
        (&four, &[ab], 0),
    ]);
    let graph = format!(
        "{first}\n{one} {first}\n{two} {one}\n{side} {one}\n{three} {side}\n{four} {three}\n"
    );
    ok(&record(&dir, &root, &graph, &pairs, None, 0));
    ok(&genealogy(&dir));

    let conn = open_genealogy(&dir);
    check_events(&conn);
    // 1 本だけの系譜で，four の版で打ち切りになる．実効の時刻は three から引き継いで 5 日目になる
    assert_eq!(
        rows(&conn, "SELECT * FROM genealogy"),
        [["1", "1", "2", "appear", "1", "4", "2", "3"]]
    );
    // two への edge でだけ Disappear したので，解消のイベントにしない
    assert_eq!(
        rows(
            &conn,
            "SELECT e.event, g.role FROM genealogy_event g JOIN events.pair_event e ON e.id = g.pair_event
             ORDER BY e.id"
        ),
        [["Appear", "birth"], ["Disappear", "NULL"]]
    );
    let time_of = |commit: &str| {
        rows(
            &conn,
            &format!(
                "SELECT t.effective_at FROM version_time t JOIN events.version v ON v.id = t.version
                 WHERE v.commit_id = '{commit}'"
            ),
        )
    };
    assert_eq!(time_of(&four), time_of(&three));
}

#[test]
fn left_censored_births_merged_into_one_genealogy_have_no_times() {
    let dir = TempDir::new("genealogy-merge");
    let root = repo(&dir, "repo");
    let text = words("a", 8);
    let one = commit(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "one",
    );
    let two = commit(&root, &[("C.txt", Some("2\n"))], "two");
    let pairs = pairs_json(&[
        (
            &one,
            &[
                (("A.java", 2, 4), ("B.java", 2, 4)),
                (("A.java", 3, 5), ("B.java", 3, 5)),
            ],
            0,
        ),
        (&two, &[(("A.java", 1, 6), ("B.java", 1, 6))], 0),
    ]);
    ok(&record(
        &dir,
        &root,
        &format!("{one}\n{two} {one}\n"),
        &pairs,
        None,
        0,
    ));
    ok(&genealogy(&dir));

    let conn = open_genealogy(&dir);
    check_events(&conn);
    assert_eq!(
        rows(&conn, "SELECT * FROM genealogy"),
        [["1", "2", "NULL", "NULL", "1", "NULL", "NULL", "NULL"]]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT genealogy, version, pair, kind FROM genealogy_root ORDER BY pair"
        ),
        [
            ["1", "1", "1", "left_censored"],
            ["1", "1", "2", "left_censored"]
        ]
    );
    assert_eq!(
        rows(&conn, "SELECT genealogy, role FROM genealogy_event"),
        [["1", "NULL"], ["1", "NULL"]]
    );
}

#[test]
fn genealogy_stops_before_writing_on_records_it_cannot_use() {
    let dir = TempDir::new("genealogy-refuse");
    let root = repo(&dir, "repo");
    let first = git(&root, &["rev-parse", "HEAD"]);
    let one = commit(&root, &[("x.txt", Some("1\n"))], "one");
    let two = commit(&root, &[("y.txt", Some("2\n"))], "two");
    let pairs = pairs_json(&[]);

    // 親が 2 つある版
    ok(&record(
        &dir,
        &root,
        &format!("{first}\n{one} {first}\n{two} {first} {one}\n"),
        &pairs,
        None,
        0,
    ));
    let output = genealogy(&dir);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains(&format!("version {two} has two or more parents")),
        "{}",
        stderr(&output)
    );
    assert!(!dir.path().join("g.db").exists());

    // 既にある系譜のファイルは上書きしない
    let dir = TempDir::new("genealogy-exists");
    let root = repo(&dir, "repo");
    let head = git(&root, &["rev-parse", "HEAD"]);
    ok(&record(&dir, &root, &format!("{head}\n"), &pairs, None, 0));
    std::fs::write(dir.path().join("g.db"), "keep").unwrap();
    let output = genealogy(&dir);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("already exists"),
        "{}",
        stderr(&output)
    );
    assert_eq!(std::fs::read(dir.path().join("g.db")).unwrap(), b"keep");
    std::fs::remove_file(dir.path().join("g.db")).unwrap();

    // ジャーナルが WAL のままの記録
    let conn = open(&dir);
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    drop(conn);
    let output = genealogy(&dir);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("still WAL"), "{}", stderr(&output));
    assert!(!dir.path().join("g.db").exists());
    // 確かめるだけで，イベントのデータベースの隣に -wal と -shm を作らない
    assert!(!dir.path().join("e.db-wal").exists() && !dir.path().join("e.db-shm").exists());
    let conn = open(&dir);
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();

    // 終わっていない記録
    conn.execute(
        "UPDATE run SET status = 'failed', stop_message = 'stopped', finished_at = NULL",
        [],
    )
    .unwrap();
    drop(conn);
    let output = genealogy(&dir);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("the record is failed, not finished"),
        "{}",
        stderr(&output)
    );
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("g.db"))
        .collect();
    assert!(names.is_empty(), "{names:?}");
}

#[test]
fn two_runs_give_the_same_tables_but_the_run_row() {
    let dir = TempDir::new("genealogy-twice");
    let root = repo(&dir, "repo");
    let first = git(&root, &["rev-parse", "HEAD"]);
    let text = words("a", 8);
    let one = commit(
        &root,
        &[
            ("A.java", Some(&text)),
            ("B.java", Some(&text)),
            ("C.java", Some(&text)),
        ],
        "one",
    );
    let two = commit(&root, &[("C.java", None)], "two");
    let pairs = pairs_json(&[
        (
            &one,
            &[
                (("A.java", 2, 4), ("B.java", 2, 4)),
                (("A.java", 3, 5), ("C.java", 3, 5)),
                (("B.java", 1, 8), ("C.java", 1, 8)),
            ],
            0,
        ),
        (&two, &[(("A.java", 1, 6), ("B.java", 1, 6))], 0),
    ]);
    ok(&record(
        &dir,
        &root,
        &format!("{first}\n{one} {first}\n{two} {one}\n"),
        &pairs,
        None,
        0,
    ));
    let db = dir.path().join("e.db");
    let mut tables = Vec::new();
    for name in ["g1.db", "g2.db"] {
        let output = run(&[
            "genealogy",
            "--db",
            db.to_str().unwrap(),
            "--output",
            dir.path().join(name).to_str().unwrap(),
        ]);
        ok(&output);
        let conn = rusqlite::Connection::open(dir.path().join(name)).unwrap();
        let mut all = Vec::new();
        for table in [
            "version_time",
            "genealogy",
            "genealogy_root",
            "genealogy_event",
            "genealogy_censored_tip",
        ] {
            all.push(rows(&conn, &format!("SELECT * FROM {table} ORDER BY 1, 2")));
        }
        assert_eq!(count(&conn, "SELECT count(*) FROM genealogy_run"), 1);
        tables.push(all);
    }
    assert!(!tables[0][1].is_empty());
    assert_eq!(tables[0], tables[1]);
}
