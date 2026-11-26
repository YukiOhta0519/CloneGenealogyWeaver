// 同じ入力で 2 回記録し，run の UUID と時刻を除いて，全部の表が行の番号まで同じかを比べる．

mod common;

use common::*;

fn words(prefix: &str, n: u32) -> String {
    (1..=n).map(|i| format!("{prefix}{i} = {i};\n")).collect()
}

/// 全部の表を，表の名前の順に，行を全部の列の値の順に並べて読む
fn dump(conn: &rusqlite::Connection) -> Vec<(String, Vec<Vec<String>>)> {
    let tables: Vec<String> = rows(
        conn,
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
    )
    .into_iter()
    .map(|row| row[0].clone())
    .collect();
    tables
        .into_iter()
        .map(|table| {
            let sql = match table.as_str() {
                // UUID と時刻は実行ごとに変わる
                "run" => "SELECT schema_version, tool_version, git_version, repository, config, graph_sha256, status FROM run".to_string(),
                _ => format!("SELECT * FROM {table}"),
            };
            let mut all = rows(conn, &sql);
            if table != "run" {
                all.sort();
            }
            (table, all)
        })
        .collect()
}

#[test]
fn the_same_input_gives_the_same_tables() {
    let base = TempDir::new("determinism");
    let root = repo(&base, "repo");
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
    let two = commit(&root, &[("B.java", Some(&words("b", 8)))], "two");
    let three = commit(&root, &[("C.java", None), ("D.java", Some(&text))], "three");
    let pairs = pairs_json(&[
        (
            &one,
            &[
                (("A.java", 1, 8), ("B.java", 1, 8)),
                (("A.java", 1, 8), ("C.java", 1, 8)),
                (("A.java", 2, 4), ("C.java", 2, 4)),
            ],
            0,
        ),
        (&two, &[(("A.java", 1, 8), ("C.java", 1, 8))], 1),
        (&three, &[(("A.java", 1, 8), ("D.java", 1, 8))], 0),
    ]);
    let services = services_json(&[
        (&one, &[("s", &["A.java", "B.java"]), ("t", &["C.java"])]),
        (&two, &[("s", &["A.java", "B.java", "C.java"])]),
        (&three, &[("s", &["A.java", "B.java"]), ("t", &["D.java"])]),
    ]);
    let graph = format!("{one}\n{two} {one}\n{three} {two}\n");
    let mut dumps = Vec::new();
    for name in ["first", "second"] {
        let dir = TempDir::new(name);
        // fixture を base に置くのは，plugin の表の設定に書く fixture の絶対パスをそろえるためである
        let output = record_with(&base, &dir, &root, &graph, &pairs, &services);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let conn = rusqlite::Connection::open(dir.path().join("e.db")).unwrap();
        check_tables(&conn);
        dumps.push(dump(&conn));
    }
    assert_eq!(dumps[0], dumps[1]);
    assert!(
        dumps[0]
            .iter()
            .any(|(table, rows)| table == "pair_event" && rows.len() >= 3)
    );
}

/// fixture と設定とグラフは base に置き，データベースだけを dir に作る
fn record_with(
    base: &TempDir,
    dir: &TempDir,
    repo: &std::path::Path,
    graph: &str,
    pairs: &str,
    services: &str,
) -> std::process::Output {
    base.write("pairs.json", pairs);
    base.write("services.json", services);
    let config = base.write(
        "config.toml",
        "[detector]\nname = \"fixture\"\nfile = \"pairs.json\"\n[classifiers.fixture]\nfile = \"services.json\"\n",
    );
    let graph = base.write("graph.txt", graph);
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
