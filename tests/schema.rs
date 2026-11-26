// 表の制約のテスト．
//
// schema/ の SQL で作った表に，制約に違反する行を 1 つずつ入れて，拒まれることを確かめる．
// 違反する行は，受け付けられる行から 1 か所だけを変えて作る．
// 受け付けられる行も確かめるので，拒まれたのが狙った制約のためだと分かる．
// プラグインのラベルの表が，どれも同じ形かも確かめる．

mod common;

use rusqlite::{Connection, ErrorCode};

use common::schema_sql;

/// 分類のプラグインのうち，表を持つもの．
const PLUGINS_WITH_TABLES: &[&str] = &["claim", "fixture", "test_path"];

fn events_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn.execute_batch(&schema_sql("events.sql")).unwrap();
    for name in PLUGINS_WITH_TABLES {
        conn.execute_batch(&schema_sql(&format!("plugins/{name}.sql")))
            .unwrap();
    }
    seed(&conn);
    conn
}

/// どの表のテストでも参照される行を入れる．
fn seed(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO run (run_id, schema_version, tool_version, git_version, repository, config,
                          graph_sha256, started_at, status)
         VALUES ('r1', 1, 't', 'g', '/repo', 'c', 'h', '2026-11-12T00:00:00Z', 'running');
         INSERT INTO plugin VALUES (1, 'detector', 'fixture', '{}'), (2, 'classifier', 'claim', '{}');
         INSERT INTO version VALUES
           (1, 'aaaa', 1, '2026-11-01T00:00:00Z', 'ok', NULL, NULL, NULL, 2, 3, 's1'),
           (2, 'bbbb', 2, '2026-11-02T00:00:00Z', 'ok', NULL, NULL, NULL, 2, 3, 's2'),
           (3, 'cccc', 3, '2026-11-03T00:00:00Z', 'pending', NULL, NULL, NULL, NULL, NULL, NULL);
         INSERT INTO input_parent VALUES (2, 1, 1);
         INSERT INTO edge VALUES (1, 1, 2, 1, 0, 0, 0);
         INSERT INTO path VALUES (1, 'A.java'), (2, 'B.java');
         INSERT INTO fragment VALUES (1, 1, 1, 5), (2, 2, 1, 5), (3, 1, 10, 12);
         INSERT INTO pair VALUES (1, 1, 2), (2, 1, 3);
         INSERT INTO pair_event (id, edge, event, child_pair, entered_scope) VALUES (1, 1, 'Appear', 1, 0);
         INSERT INTO root_pair VALUES (1, 1, NULL);",
    )
    .unwrap();
}

fn genealogy_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn.execute_batch(&schema_sql("genealogy.sql")).unwrap();
    conn
}

/// 行が受け付けられることを確かめ，入れた行は戻す．
fn accepts(conn: &Connection, sql: &str) {
    conn.execute_batch("SAVEPOINT probe").unwrap();
    let result = conn.execute_batch(sql);
    conn.execute_batch("ROLLBACK TO probe; RELEASE probe")
        .unwrap();
    if let Err(err) = result {
        panic!("rejected: {sql}\n{err}");
    }
}

/// 行が制約で拒まれることを確かめる．
fn rejects(conn: &Connection, sql: &str) {
    conn.execute_batch("SAVEPOINT probe").unwrap();
    let result = conn.execute_batch(sql);
    conn.execute_batch("ROLLBACK TO probe; RELEASE probe")
        .unwrap();
    match result {
        Ok(()) => panic!("accepted: {sql}"),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == ErrorCode::ConstraintViolation => {}
        Err(err) => panic!("failed for another reason: {sql}\n{err}"),
    }
}

/// 列と値の組から INSERT の文を作る．overrides の列だけ値を替える．同じ列が 2 回あれば後の方を使う．
fn insert(table: &str, base: &[(&str, &str)], overrides: &[(&str, &str)]) -> String {
    for (column, _) in overrides {
        assert!(
            base.iter().any(|(c, _)| c == column),
            "unknown column {column}"
        );
    }
    let columns: Vec<&str> = base.iter().map(|(c, _)| *c).collect();
    let values: Vec<&str> = base
        .iter()
        .map(|(c, v)| {
            overrides
                .iter()
                .rev()
                .find(|(o, _)| o == c)
                .map_or(*v, |(_, o)| *o)
        })
        .collect();
    format!(
        "INSERT INTO {table} ({}) VALUES ({})",
        columns.join(", "),
        values.join(", ")
    )
}

#[test]
fn run_rows() {
    let conn = events_db();
    let base = [
        ("run_id", "'r2'"),
        ("schema_version", "1"),
        ("tool_version", "'t'"),
        ("git_version", "'g'"),
        ("repository", "'/repo'"),
        ("config", "'c'"),
        ("graph_sha256", "'h'"),
        ("started_at", "'2026-11-12T00:00:00Z'"),
        ("status", "'failed'"),
        ("stop_message", "'boom'"),
    ];
    accepts(&conn, &insert("run", &base, &[]));
    rejects(&conn, &insert("run", &base, &[("status", "'done'")]));
    rejects(&conn, &insert("run", &base, &[("stop_message", "NULL")]));
    rejects(&conn, &insert("run", &base, &[("status", "'running'")]));
}

#[test]
fn plugin_rows() {
    let conn = events_db();
    accepts(
        &conn,
        "INSERT INTO plugin VALUES (3, 'classifier', 'fixture', '{}')",
    );
    rejects(
        &conn,
        "INSERT INTO plugin VALUES (3, 'filter', 'vendor', '{}')",
    );
    rejects(
        &conn,
        "INSERT INTO plugin VALUES (3, 'detector', 'msccd', '{}')",
    );
    rejects(
        &conn,
        "INSERT INTO plugin VALUES (3, 'classifier', 'claim', '{}')",
    );
    let fresh = Connection::open_in_memory().unwrap();
    fresh.execute_batch(&schema_sql("events.sql")).unwrap();
    rejects(
        &fresh,
        "INSERT INTO plugin VALUES (1, 'classifier', 'claim', '{}')",
    );
}

#[test]
fn version_rows() {
    let conn = events_db();
    let base = [
        ("id", "4"),
        ("commit_id", "'dddd'"),
        ("topo_order", "4"),
        ("committed_at", "'2026-11-04T00:00:00Z'"),
        ("status", "'broken'"),
        ("failed_plugin", "2"),
        ("failure_message", "'scope: no context'"),
        ("root", "NULL"),
        ("pair_count", "NULL"),
        ("fragment_count", "NULL"),
        ("scope_sha256", "NULL"),
    ];
    accepts(&conn, &insert("version", &base, &[]));
    accepts(&conn, &insert("version", &base, &[("root", "'censored'")]));
    rejects(&conn, &insert("version", &base, &[("status", "'done'")]));
    rejects(&conn, &insert("version", &base, &[("status", "'ok'")]));
    rejects(&conn, &insert("version", &base, &[("scope_sha256", "'s'")]));
    rejects(&conn, &insert("version", &base, &[("failed_plugin", "9")]));
    rejects(&conn, &insert("version", &base, &[("root", "'empty'")]));
    rejects(&conn, &insert("version", &base, &[("commit_id", "'aaaa'")]));
    rejects(&conn, &insert("version", &base, &[("topo_order", "1")]));
}

#[test]
fn input_parent_and_edge_rows() {
    let conn = events_db();
    accepts(&conn, "INSERT INTO input_parent VALUES (3, 1, 2)");
    rejects(&conn, "INSERT INTO input_parent VALUES (9, 1, 2)");
    rejects(&conn, "INSERT INTO input_parent VALUES (2, 1, 3)");
    rejects(&conn, "INSERT INTO input_parent VALUES (3, 1, 9)");

    let base = [
        ("id", "2"),
        ("parent", "2"),
        ("child", "3"),
        ("is_ancestor", "0"),
        ("skipped_versions", "0"),
        ("implicit_pairs", "0"),
        ("implicit_fragments", "0"),
    ];
    accepts(&conn, &insert("edge", &base, &[]));
    accepts(
        &conn,
        &insert(
            "edge",
            &base,
            &[("parent", "NULL"), ("is_ancestor", "NULL")],
        ),
    );
    rejects(&conn, &insert("edge", &base, &[("is_ancestor", "2")]));
    rejects(&conn, &insert("edge", &base, &[("is_ancestor", "NULL")]));
    rejects(&conn, &insert("edge", &base, &[("parent", "NULL")]));
    rejects(
        &conn,
        &insert("edge", &base, &[("parent", "1"), ("child", "2")]),
    );
    rejects(&conn, &insert("edge", &base, &[("child", "9")]));
    rejects(&conn, &insert("edge", &base, &[("parent", "9")]));
    rejects(
        &conn,
        "INSERT INTO edge VALUES (2, NULL, 3, NULL, 0, 0, 0);
         INSERT INTO edge VALUES (3, NULL, 3, NULL, 0, 0, 0);",
    );
}

#[test]
fn dictionary_rows() {
    let conn = events_db();
    rejects(&conn, "INSERT INTO path VALUES (3, 'A.java')");
    rejects(&conn, "INSERT INTO fragment VALUES (4, 1, 1, 5)");
    rejects(&conn, "INSERT INTO fragment VALUES (4, 9, 1, 5)");
    accepts(&conn, "INSERT INTO pair VALUES (3, 2, 3)");
    rejects(&conn, "INSERT INTO pair VALUES (3, 1, 1)");
    rejects(&conn, "INSERT INTO pair VALUES (3, 1, 2)");
    rejects(&conn, "INSERT INTO pair VALUES (3, 1, 9)");
    rejects(&conn, "INSERT INTO pair VALUES (3, 9, 1)");
}

#[test]
fn edge_file_issue_rows() {
    let conn = events_db();
    accepts(
        &conn,
        "INSERT INTO edge_file_issue VALUES (1, 1, 2, 'hunk_too_large', 3)",
    );
    accepts(
        &conn,
        "INSERT INTO edge_file_issue VALUES (1, NULL, 2, 'diff_failed', NULL)",
    );
    rejects(
        &conn,
        "INSERT INTO edge_file_issue VALUES (1, 1, 2, 'binary', NULL)",
    );
    rejects(
        &conn,
        "INSERT INTO edge_file_issue VALUES (9, 1, 2, 'diff_failed', NULL)",
    );
    rejects(
        &conn,
        "INSERT INTO edge_file_issue VALUES (1, 9, 2, 'diff_failed', NULL)",
    );
    rejects(
        &conn,
        "INSERT INTO edge_file_issue VALUES (1, 1, 9, 'diff_failed', NULL)",
    );
}

/// 子の片が同じ 3 つ組で続いた Stable の片の対応．
const STABLE: &[(&str, &str)] = &[
    ("edge", "1"),
    ("parent_fragment", "1"),
    ("child_fragment", "1"),
    ("child_kind", "'detected'"),
    ("renamed", "0"),
    ("event", "'Stable'"),
    ("similarity_cut", "0"),
    ("file_deleted", "NULL"),
    ("parent_lines", "5"),
    ("parent_symbol_lines", "0"),
    ("parent_words", "10"),
    ("retained_lines", "5"),
    ("retained_symbol_lines", "0"),
    ("retained_words", "10"),
    ("common_words", "10"),
    ("modified_lines", "0"),
    ("whitespace_lines", "0"),
    ("taken_in_lines", "0"),
    ("taken_in_modified_lines", "0"),
    ("added_lines", "0"),
    ("child_words", "10"),
    ("text_similarity", "1.0"),
];

/// 行が全部消えた Deleted の片の対応への替え方．
const DELETED: &[(&str, &str)] = &[
    ("child_fragment", "NULL"),
    ("child_kind", "NULL"),
    ("event", "'Deleted'"),
    ("file_deleted", "1"),
    ("retained_lines", "0"),
    ("retained_words", "0"),
    ("common_words", "0"),
    ("child_words", "NULL"),
    ("text_similarity", "NULL"),
];

/// 片の類似度で切った Deleted の片の対応への替え方．2×1/(10+20) は 0.15 より小さい．
const CUT: &[(&str, &str)] = &[
    ("child_fragment", "NULL"),
    ("child_kind", "NULL"),
    ("event", "'Deleted'"),
    ("similarity_cut", "1"),
    ("file_deleted", "0"),
    ("retained_lines", "3"),
    ("retained_words", "4"),
    ("common_words", "1"),
    ("child_words", "20"),
    ("text_similarity", "2.0 * 1 / 30"),
];

fn with(
    base: &[(&'static str, &'static str)],
    extra: &[(&'static str, &'static str)],
) -> Vec<(&'static str, &'static str)> {
    let mut row = base.to_vec();
    row.extend_from_slice(extra);
    row
}

#[test]
fn fragment_mapping_rows() {
    let conn = events_db();
    let t = "fragment_mapping";
    accepts(&conn, &insert(t, STABLE, &[]));
    accepts(&conn, &insert(t, STABLE, DELETED));
    accepts(
        &conn,
        &insert(t, STABLE, &with(DELETED, &[("file_deleted", "0")])),
    );
    accepts(&conn, &insert(t, STABLE, CUT));
    accepts(
        &conn,
        &insert(
            t,
            STABLE,
            &[
                ("child_fragment", "3"),
                ("child_kind", "'projected'"),
                ("event", "'Modified'"),
                ("modified_lines", "1"),
                ("common_words", "8"),
                ("child_words", "12"),
                ("text_similarity", "2.0 * 8 / 22"),
            ],
        ),
    );

    // 列の値の範囲
    rejects(&conn, &insert(t, STABLE, &[("child_kind", "'guessed'")]));
    rejects(&conn, &insert(t, STABLE, &[("parent_fragment", "9")]));
    rejects(&conn, &insert(t, STABLE, &[("renamed", "2")]));
    rejects(&conn, &insert(t, STABLE, &[("event", "'Moved'")]));
    rejects(&conn, &insert(t, STABLE, &[("similarity_cut", "2")]));
    rejects(
        &conn,
        &insert(t, STABLE, &with(DELETED, &[("file_deleted", "2")])),
    );
    // 子の片が無いのは Deleted だけで，そのときは続き先の種類も無い
    rejects(
        &conn,
        &insert(
            t,
            STABLE,
            &[("child_fragment", "NULL"), ("child_kind", "NULL")],
        ),
    );
    rejects(&conn, &insert(t, STABLE, &[("child_kind", "NULL")]));
    rejects(
        &conn,
        &insert(
            t,
            STABLE,
            &with(
                DELETED,
                &[("child_fragment", "2"), ("child_kind", "'detected'")],
            ),
        ),
    );
    // 類似度で切った印は Deleted だけに付く
    rejects(&conn, &insert(t, STABLE, &[("similarity_cut", "1")]));
    // ファイルが消された印は Deleted だけに付き，Deleted には必ず付く
    rejects(&conn, &insert(t, STABLE, &[("file_deleted", "0")]));
    rejects(
        &conn,
        &insert(t, STABLE, &with(DELETED, &[("file_deleted", "NULL")])),
    );
    // ファイルが消されたなら，残った行は無い
    rejects(
        &conn,
        &insert(t, STABLE, &with(CUT, &[("file_deleted", "1")])),
    );
    // 切っていない Deleted と，残った行が無いことは同じ
    rejects(
        &conn,
        &insert(t, STABLE, &with(CUT, &[("similarity_cut", "0")])),
    );
    rejects(&conn, &insert(t, STABLE, &[("retained_lines", "0")]));
    // 子の語の数と片の類似度は，残った行があるときだけ書く
    rejects(
        &conn,
        &insert(
            t,
            STABLE,
            &[("child_words", "NULL"), ("text_similarity", "NULL")],
        ),
    );
    rejects(&conn, &insert(t, STABLE, &[("text_similarity", "NULL")]));
    rejects(
        &conn,
        &insert(
            t,
            STABLE,
            &with(DELETED, &[("child_words", "4"), ("text_similarity", "0.0")]),
        ),
    );
    // 共通の語は残った語を超えない
    rejects(
        &conn,
        &insert(
            t,
            STABLE,
            &[("common_words", "11"), ("text_similarity", "2.0 * 11 / 20")],
        ),
    );
    // 片の類似度は数から求まる値と同じ
    rejects(&conn, &insert(t, STABLE, &[("text_similarity", "0.5")]));
    rejects(&conn, &insert(t, STABLE, &[("text_similarity", "1.5")]));
    // 類似度が 0.15 より小さいのに切っていない
    rejects(
        &conn,
        &insert(
            t,
            STABLE,
            &[
                ("event", "'Modified'"),
                ("common_words", "1"),
                ("child_words", "20"),
                ("text_similarity", "2.0 * 1 / 30"),
            ],
        ),
    );
    // 主キーと参照
    rejects(
        &conn,
        &format!("{}; {}", insert(t, STABLE, &[]), insert(t, STABLE, &[])),
    );
    rejects(&conn, &insert(t, STABLE, &[("child_fragment", "9")]));
    rejects(&conn, &insert(t, STABLE, &[("edge", "9")]));
}

#[test]
fn fragment_entry_rows() {
    let conn = events_db();
    accepts(&conn, "INSERT INTO fragment_entry VALUES (1, 3, 1, 1)");
    accepts(&conn, "INSERT INTO fragment_entry VALUES (1, 3, 0, 0)");
    rejects(&conn, "INSERT INTO fragment_entry VALUES (1, 3, 0, 1)");
    rejects(&conn, "INSERT INTO fragment_entry VALUES (1, 3, 2, 0)");
    rejects(&conn, "INSERT INTO fragment_entry VALUES (9, 3, 1, 1)");
    rejects(&conn, "INSERT INTO fragment_entry VALUES (1, 9, 1, 1)");
}

#[test]
fn pair_event_rows() {
    let conn = events_db();
    let base = [
        ("edge", "1"),
        ("event", "'Persist'"),
        ("parent_pair", "1"),
        ("child_pair", "2"),
        ("reason", "NULL"),
        ("entered_scope", "NULL"),
        ("similarity", "0.8"),
    ];
    let t = "pair_event";
    accepts(&conn, &insert(t, &base, &[]));
    accepts(&conn, &insert(t, &base, &[("event", "'Merge'")]));
    rejects(&conn, &insert(t, &base, &[("edge", "9")]));
    rejects(&conn, &insert(t, &base, &[("parent_pair", "9")]));
    accepts(
        &conn,
        &insert(
            t,
            &base,
            &[
                ("event", "'Disappear'"),
                ("child_pair", "NULL"),
                ("reason", "'not_paired'"),
                ("similarity", "NULL"),
            ],
        ),
    );
    accepts(
        &conn,
        &insert(
            t,
            &base,
            &[
                ("event", "'Appear'"),
                ("parent_pair", "NULL"),
                ("entered_scope", "1"),
            ],
        ),
    );

    rejects(&conn, &insert(t, &base, &[("event", "'Gone'")]));
    rejects(
        &conn,
        &insert(t, &base, &[("event", "'Appear'"), ("entered_scope", "0")]),
    );
    rejects(&conn, &insert(t, &base, &[("parent_pair", "NULL")]));
    rejects(
        &conn,
        &insert(
            t,
            &base,
            &[("event", "'Disappear'"), ("reason", "'not_paired'")],
        ),
    );
    rejects(
        &conn,
        &insert(
            t,
            &base,
            &[("event", "'Disappear'"), ("child_pair", "NULL")],
        ),
    );
    rejects(&conn, &insert(t, &base, &[("reason", "'not_paired'")]));
    rejects(
        &conn,
        &insert(
            t,
            &base,
            &[
                ("event", "'Disappear'"),
                ("child_pair", "NULL"),
                ("reason", "'vanished'"),
            ],
        ),
    );
    rejects(&conn, &insert(t, &base, &[("entered_scope", "0")]));
    rejects(
        &conn,
        &insert(t, &base, &[("event", "'Appear'"), ("parent_pair", "NULL")]),
    );
    rejects(
        &conn,
        &insert(
            t,
            &base,
            &[
                ("event", "'Appear'"),
                ("parent_pair", "NULL"),
                ("entered_scope", "2"),
            ],
        ),
    );
    rejects(&conn, &insert(t, &base, &[("similarity", "1.5")]));
    rejects(&conn, &insert(t, &base, &[("child_pair", "9")]));
    rejects(
        &conn,
        &format!(
            "{}; {}",
            insert(t, &base, &[]),
            insert(t, &base, &[("event", "'Merge'")])
        ),
    );
}

#[test]
fn root_pair_and_failure_rows() {
    let conn = events_db();
    accepts(&conn, "INSERT INTO root_pair VALUES (1, 2, 0.5)");
    rejects(&conn, "INSERT INTO root_pair VALUES (1, 2, -0.1)");
    rejects(&conn, "INSERT INTO root_pair VALUES (1, 1, NULL)");
    rejects(&conn, "INSERT INTO root_pair VALUES (1, 9, NULL)");
    rejects(&conn, "INSERT INTO root_pair VALUES (9, 2, 0.5)");
    accepts(
        &conn,
        "INSERT INTO plugin_failure VALUES (2, 2, 1, 'scope: no context')",
    );
    rejects(&conn, "INSERT INTO plugin_failure VALUES (2, 9, 1, 'm')");
    rejects(&conn, "INSERT INTO plugin_failure VALUES (9, 2, 1, 'm')");
    rejects(
        &conn,
        "INSERT INTO plugin_failure VALUES (2, 1, 1, 'm'); INSERT INTO plugin_failure VALUES (2, 1, 1, 'n');",
    );
    accepts(
        &conn,
        "INSERT INTO run_resume VALUES ('2026-11-12T00:00:00Z', 2)",
    );
    rejects(
        &conn,
        "INSERT INTO run_resume VALUES ('2026-11-12T00:00:00Z', 9)",
    );
}

#[test]
fn plugin_table_rows() {
    let conn = events_db();
    accepts(
        &conn,
        "INSERT INTO claim_pair_event VALUES (1, 'intra', NULL)",
    );
    accepts(
        &conn,
        "INSERT INTO test_path_pair_event VALUES (1, 'mixed', NULL)",
    );
    rejects(
        &conn,
        "INSERT INTO claim_pair_event VALUES (1, 'both', NULL)",
    );
    rejects(&conn, "INSERT INTO claim_pair_event VALUES (1, 'intra', 2)");
    rejects(
        &conn,
        "INSERT INTO claim_pair_event VALUES (9, 'intra', NULL)",
    );
    rejects(
        &conn,
        "INSERT INTO test_path_pair_event VALUES (1, 'unit', NULL)",
    );
    accepts(&conn, "INSERT INTO claim_root_pair VALUES (1, 1, 'inter')");
    rejects(&conn, "INSERT INTO claim_root_pair VALUES (1, 2, 'inter')");
    accepts(
        &conn,
        "INSERT INTO claim_context_issue VALUES (1, '../svc')",
    );
    rejects(
        &conn,
        "INSERT INTO claim_context_issue VALUES (9, '../svc')",
    );
    // fixture と test_path の表の外部キーも，行を入れて確かめる
    accepts(
        &conn,
        "INSERT INTO fixture_pair_event VALUES (1, 'intra', NULL)",
    );
    rejects(
        &conn,
        "INSERT INTO fixture_pair_event VALUES (9, 'intra', NULL)",
    );
    rejects(
        &conn,
        "INSERT INTO test_path_pair_event VALUES (9, 'mixed', NULL)",
    );
    accepts(
        &conn,
        "INSERT INTO fixture_root_pair VALUES (1, 1, 'inter')",
    );
    rejects(
        &conn,
        "INSERT INTO fixture_root_pair VALUES (1, 2, 'inter')",
    );
    accepts(
        &conn,
        "INSERT INTO test_path_root_pair VALUES (1, 1, 'mixed')",
    );
    rejects(
        &conn,
        "INSERT INTO test_path_root_pair VALUES (1, 2, 'mixed')",
    );
    accepts(
        &conn,
        "INSERT INTO fixture_context_issue VALUES (1, 'deploy/old')",
    );
    rejects(
        &conn,
        "INSERT INTO fixture_context_issue VALUES (9, 'deploy/old')",
    );
}

/// 表の形を，比べられる文字列の列にする．名前の先頭のプラグインの名前と，ラベルの値は除く．
fn shape(conn: &Connection, plugin: &str, suffix: &str) -> Vec<String> {
    let table = format!("{plugin}_{suffix}");
    let mut rows: Vec<String> = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap()
        .query_map([], |r| {
            Ok(format!(
                "column {} {} {} notnull={} pk={}",
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(5)?
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let keys: Vec<String> = conn
        .prepare(&format!("PRAGMA foreign_key_list({table})"))
        .unwrap()
        .query_map([], |r| {
            Ok(format!(
                "foreign key {} {}.{}",
                r.get::<_, String>(3)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(4)?
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows.extend(keys);
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = ?1",
            [&table],
            |r| r.get(0),
        )
        .unwrap();
    let label_check = regex::Regex::new(r"CHECK \(label IN \([^)]*\)\)").unwrap();
    rows.push(
        label_check
            .replace_all(
                &sql.replace(&format!("{plugin}_"), "X_"),
                "CHECK (label IN (...))",
            )
            .into_owned(),
    );
    rows
}

#[test]
fn label_tables_have_one_shape() {
    let conn = events_db();
    for suffix in ["pair_event", "root_pair"] {
        let reference = shape(&conn, "claim", suffix);
        for plugin in PLUGINS_WITH_TABLES {
            assert_eq!(shape(&conn, plugin, suffix), reference, "{plugin}_{suffix}");
        }
    }
}

#[test]
fn plugin_tables_are_named_after_the_plugin_and_start_with_the_version() {
    for plugin in PLUGINS_WITH_TABLES {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&schema_sql("events.sql")).unwrap();
        let before: Vec<String> = table_names(&conn);
        conn.execute_batch(&schema_sql(&format!("plugins/{plugin}.sql")))
            .unwrap();
        let added: Vec<String> = table_names(&conn)
            .into_iter()
            .filter(|t| !before.contains(t))
            .collect();
        assert!(added.contains(&format!("{plugin}_pair_event")));
        assert!(added.contains(&format!("{plugin}_root_pair")));
        for table in &added {
            assert!(table.starts_with(&format!("{plugin}_")), "{table}");
            if table.ends_with("_pair_event") || table.ends_with("_root_pair") {
                continue;
            }
            let (first, ty): (String, String) = conn
                .query_row(
                    &format!("SELECT name, type FROM pragma_table_info('{table}') WHERE cid = 0"),
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                (first.as_str(), ty.as_str()),
                ("version", "INTEGER"),
                "{table}"
            );
            let refers: String = conn
                .query_row(&format!("SELECT \"table\" FROM pragma_foreign_key_list('{table}') WHERE \"from\" = 'version'"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(refers, "version", "{table}");
        }
    }
}

fn table_names(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn genealogy_rows() {
    let conn = genealogy_db();
    let base = [
        ("id", "1"),
        ("root_count", "1"),
        ("birth_version", "5"),
        ("birth_kind", "'appear'"),
        ("censored", "0"),
        ("effective_days", "2.5"),
        ("dag_transitions", "3"),
        ("dag_transitions_with_skipped", "4"),
    ];
    let t = "genealogy";
    let left_censored: &[(&str, &str)] = &[
        ("birth_kind", "'left_censored'"),
        ("effective_days", "NULL"),
        ("dag_transitions", "NULL"),
        ("dag_transitions_with_skipped", "NULL"),
    ];
    let two_roots: &[(&str, &str)] = &[
        ("root_count", "2"),
        ("birth_version", "NULL"),
        ("birth_kind", "NULL"),
        ("effective_days", "NULL"),
        ("dag_transitions", "NULL"),
        ("dag_transitions_with_skipped", "NULL"),
    ];
    accepts(&conn, &insert(t, &base, &[]));
    accepts(&conn, &insert(t, &base, left_censored));
    accepts(&conn, &insert(t, &base, two_roots));

    rejects(&conn, &insert(t, &base, &[("root_count", "0")]));
    rejects(&conn, &insert(t, &base, &[("birth_version", "NULL")]));
    rejects(&conn, &insert(t, &base, &[("birth_kind", "NULL")]));
    rejects(&conn, &insert(t, &base, &[("birth_kind", "'merge'")]));
    rejects(&conn, &insert(t, &base, &[("censored", "2")]));
    rejects(&conn, &insert(t, &base, &[("effective_days", "-1.0")]));
    rejects(&conn, &insert(t, &base, &[("effective_days", "NULL")]));
    rejects(&conn, &insert(t, &base, &[("dag_transitions", "NULL")]));
    rejects(
        &conn,
        &insert(t, &base, &[("dag_transitions_with_skipped", "NULL")]),
    );
    rejects(
        &conn,
        &insert(t, &base, &[("dag_transitions_with_skipped", "2")]),
    );
    rejects(
        &conn,
        &insert(
            t,
            &base,
            &[
                ("dag_transitions", "-1"),
                ("dag_transitions_with_skipped", "0"),
            ],
        ),
    );
    rejects(
        &conn,
        &insert(t, &base, &with(left_censored, &[("effective_days", "1.0")])),
    );
    rejects(
        &conn,
        &insert(t, &base, &with(two_roots, &[("birth_version", "5")])),
    );

    conn.execute_batch(&insert(t, &base, &[])).unwrap();
    accepts(
        &conn,
        "INSERT INTO genealogy_root VALUES (1, 5, 7, 'appear')",
    );
    rejects(
        &conn,
        "INSERT INTO genealogy_root VALUES (1, 5, 7, 'merge')",
    );
    rejects(
        &conn,
        "INSERT INTO genealogy_root VALUES (9, 5, 7, 'appear')",
    );
    accepts(&conn, "INSERT INTO genealogy_event VALUES (10, 1, 'birth')");
    accepts(&conn, "INSERT INTO genealogy_event VALUES (10, 1, NULL)");
    rejects(
        &conn,
        "INSERT INTO genealogy_event VALUES (10, 1, 'middle')",
    );
    rejects(&conn, "INSERT INTO genealogy_event VALUES (10, 9, NULL)");
    accepts(&conn, "INSERT INTO genealogy_censored_tip VALUES (1, 8, 7)");
    rejects(&conn, "INSERT INTO genealogy_censored_tip VALUES (9, 8, 7)");
}
