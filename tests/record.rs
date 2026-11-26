// 規則ごとの最小の入力を，使い捨てのリポジトリと 2 つの fixture で記録して確かめる．

mod common;

use common::*;

/// 1 行に 1 語ずつ違う語の並んだ中身
fn words(prefix: &str, n: u32) -> String {
    (1..=n).map(|i| format!("{prefix}{i} = {i};\n")).collect()
}

fn graph(lines: &[String]) -> String {
    lines.iter().map(|line| format!("{line}\n")).collect()
}

fn ok(output: &std::process::Output) {
    assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
}

#[test]
fn unchanged_pairs_and_fragments_are_counted_but_not_written() {
    let dir = TempDir::new("implicit");
    let root = repo(&dir, "repo");
    let one = commit(
        &root,
        &[
            ("A.java", Some(&words("a", 6))),
            ("B.java", Some(&words("b", 6))),
        ],
        "one",
    );
    let two = commit(&root, &[("C.txt", Some("unrelated\n"))], "two");
    let pair = [(("A.java", 1, 6), ("B.java", 1, 6))];
    let pairs = pairs_json(&[(&one, &pair, 0), (&two, &pair, 0)]);
    let output = record(
        &dir,
        &root,
        &graph(&[one.clone(), format!("{two} {one}")]),
        &pairs,
        None,
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT parent, child, implicit_pairs, implicit_fragments FROM edge WHERE parent IS NOT NULL"
        ),
        [["1", "2", "1", "2"]]
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM pair_event WHERE edge = 1"),
        0
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM fragment_mapping WHERE edge = 1"
        ),
        0
    );
}

#[test]
fn a_broken_version_is_skipped_and_counted_on_the_edge() {
    let dir = TempDir::new("broken");
    let root = repo(&dir, "repo");
    let text = words("a", 6);
    let a = commit(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "a",
    );
    let b = commit(&root, &[("C.txt", Some("b\n"))], "b");
    let c = commit(&root, &[("D.txt", Some("c\n"))], "c");
    let pair = [(("A.java", 1, 6), ("B.java", 1, 6))];
    // b は 3 回とも失敗する．やり直しは 2 回である
    let pairs = pairs_json(&[(&a, &pair, 0), (&b, &pair, 3), (&c, &pair, 0)]);
    let output = record(
        &dir,
        &root,
        &graph(&[a.clone(), format!("{b} {a}"), format!("{c} {b}")]),
        &pairs,
        None,
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT id, status, failed_plugin, failure_message FROM version ORDER BY id"
        ),
        [
            ["1", "ok", "NULL", "NULL"],
            ["2", "broken", "1", "planned failure (fixture)"],
            ["3", "ok", "NULL", "NULL"],
        ]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT parent, child, skipped_versions, is_ancestor FROM edge WHERE parent IS NOT NULL"
        ),
        [["1", "3", "1", "1"]]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT version, plugin, attempt FROM plugin_failure ORDER BY attempt"
        ),
        [["2", "1", "1"], ["2", "1", "2"], ["2", "1", "3"]]
    );
}

#[test]
fn a_version_with_git_parents_but_no_graph_parent_keeps_its_pairs_as_root_pairs() {
    let dir = TempDir::new("censored");
    let root = repo(&dir, "repo");
    let text = words("a", 6);
    let c = commit(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "c",
    );
    let pair = [(("A.java", 1, 6), ("B.java", 1, 6))];
    let services = services_json(&[(&c, &[("s", &["A.java", "B.java"])])]);
    let output = record(
        &dir,
        &root,
        &graph(std::slice::from_ref(&c)),
        &pairs_json(&[(&c, &pair, 0)]),
        Some(&services),
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(rows(&conn, "SELECT root FROM version"), [["censored"]]);
    assert_eq!(count(&conn, "SELECT count(*) FROM edge"), 0);
    assert_eq!(
        rows(&conn, "SELECT version, pair FROM root_pair"),
        [["1", "1"]]
    );
    assert_eq!(
        rows(&conn, "SELECT label FROM fixture_root_pair"),
        [["intra"]]
    );
}

#[test]
fn a_deleted_file_deletes_its_fragments_and_the_pair_disappears() {
    let dir = TempDir::new("deleted");
    let root = repo(&dir, "repo");
    let text = words("a", 6);
    let one = commit(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "one",
    );
    let two = commit(&root, &[("B.java", None)], "two");
    let pairs = pairs_json(&[
        (&one, &[(("A.java", 1, 6), ("B.java", 1, 6))], 0),
        (&two, &[], 0),
    ]);
    let output = record(
        &dir,
        &root,
        &graph(&[one.clone(), format!("{two} {one}")]),
        &pairs,
        None,
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT f.path, m.event, m.file_deleted, m.child_kind FROM fragment_mapping m
             JOIN fragment_view f ON f.id = m.parent_fragment WHERE m.edge = 1 ORDER BY f.path"
        ),
        [
            // A.java の片の対応は，同じ 3 つ組の投影した範囲に続いた Stable なので書かない
            ["B.java", "Deleted", "1", "NULL"]
        ]
    );
    assert_eq!(
        rows(&conn, "SELECT event, reason FROM pair_event WHERE edge = 1"),
        [["Disappear", "fragment_deleted"]]
    );
}

#[test]
fn leaving_and_entering_the_scope() {
    let dir = TempDir::new("scope");
    let root = repo(&dir, "repo");
    let text = words("a", 6);
    let one = commit(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "one",
    );
    let two = commit(&root, &[("C.txt", Some("2\n"))], "two");
    let three = commit(&root, &[("C.txt", Some("3\n"))], "three");
    let pair = [(("A.java", 1, 6), ("B.java", 1, 6))];
    let pairs = pairs_json(&[(&one, &pair, 0), (&two, &[], 0), (&three, &pair, 0)]);
    let both: &[(&str, &[&str])] = &[("s", &["A.java", "B.java"])];
    let only_a: &[(&str, &[&str])] = &[("s", &["A.java"])];
    let services = services_json(&[(&one, both), (&two, only_a), (&three, both)]);
    let output = record(
        &dir,
        &root,
        &graph(&[
            one.clone(),
            format!("{two} {one}"),
            format!("{three} {two}"),
        ]),
        &pairs,
        Some(&services),
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT edge, event, reason, entered_scope FROM pair_event ORDER BY id"
        ),
        [
            ["1", "Disappear", "out_of_scope", "NULL"],
            ["2", "Appear", "NULL", "1"]
        ]
    );
}

#[test]
fn two_pairs_reaching_one_pair_merge_with_one_row_each() {
    let dir = TempDir::new("merge");
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
    let output = record(
        &dir,
        &root,
        &graph(&[one.clone(), format!("{two} {one}")]),
        &pairs,
        None,
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT event, parent_pair, child_pair FROM pair_event WHERE edge = 1 ORDER BY id"
        ),
        [["Merge", "1", "3"], ["Merge", "2", "3"]]
    );
}

#[test]
fn a_renamed_file_carries_the_mark() {
    let dir = TempDir::new("renamed");
    let root = repo(&dir, "repo");
    let text = words("a", 6);
    let one = commit(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "one",
    );
    let two = commit(
        &root,
        &[("B.java", None), ("sub/B2.java", Some(&text))],
        "two",
    );
    let pairs = pairs_json(&[
        (&one, &[(("A.java", 1, 6), ("B.java", 1, 6))], 0),
        (&two, &[(("A.java", 1, 6), ("sub/B2.java", 1, 6))], 0),
    ]);
    let output = record(
        &dir,
        &root,
        &graph(&[one.clone(), format!("{two} {one}")]),
        &pairs,
        None,
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT p.path, c.path, m.renamed, m.event FROM fragment_mapping m
             JOIN fragment_view p ON p.id = m.parent_fragment JOIN fragment_view c ON c.id = m.child_fragment
             WHERE m.edge = 1"
        ),
        [["B.java", "sub/B2.java", "1", "Stable"]]
    );
    assert_eq!(
        rows(&conn, "SELECT event FROM pair_event WHERE edge = 1"),
        [["Persist"]]
    );
}

#[test]
fn a_hunk_too_large_to_pair_is_recorded_on_the_edge() {
    let dir = TempDir::new("large");
    let root = repo(&dir, "repo");
    let old = words("a", 1001);
    let new = words("z", 1000);
    let one = commit(
        &root,
        &[("A.java", Some(&old)), ("B.java", Some(&old))],
        "one",
    );
    let two = commit(&root, &[("A.java", Some(&new))], "two");
    let pairs = pairs_json(&[
        (&one, &[(("A.java", 1, 1001), ("B.java", 1, 1001))], 0),
        (&two, &[], 0),
    ]);
    let output = record(
        &dir,
        &root,
        &graph(&[one.clone(), format!("{two} {one}")]),
        &pairs,
        None,
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT o.path, n.path, i.issue, i.hunks FROM edge_file_issue i
             JOIN path o ON o.id = i.old_path JOIN path n ON n.id = i.new_path"
        ),
        [["A.java", "A.java", "hunk_too_large", "1"]]
    );
}

#[test]
fn an_adapter_error_stops_the_record_and_tells_how_to_resume() {
    let dir = TempDir::new("adapter");
    let root = repo(&dir, "repo");
    let one = commit(
        &root,
        &[("A.java", Some("x\ny\n")), ("B.java", Some("x\n\n"))],
        "one",
    );
    let two = commit(&root, &[("C.txt", Some("2\n"))], "two");
    for (bad, message) in [
        (
            (("A.java", 1, 2), ("B.java", 1, 3)),
            "fragment B.java:1-3 ends after the last line 2 of the file",
        ),
        (
            (("A.java", 1, 2), ("B.java", 2, 2)),
            "fragment B.java:2-2 has only blank lines",
        ),
        (
            (("A.java", 1, 2), ("D.java", 1, 1)),
            "fragment D.java:1-1 is out of the detection scope",
        ),
    ] {
        let _ = std::fs::remove_file(dir.path().join("e.db"));
        let _ = std::fs::remove_file(dir.path().join("e.db.lock"));
        let _ = std::fs::remove_dir_all(dir.path().join("e.db.clone"));
        let _ = std::fs::remove_dir_all(dir.path().join("e.db.tmp"));
        let pairs = pairs_json(&[
            (&one, &[(("A.java", 1, 2), ("B.java", 1, 1))], 0),
            (&two, &[bad], 0),
        ]);
        let output = record(
            &dir,
            &root,
            &graph(&[one.clone(), format!("{two} {one}")]),
            &pairs,
            None,
            2,
        );
        let text = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert!(text.contains(message), "{text}");
        assert!(text.contains(&format!("next: {two}")), "{text}");
        assert!(text.contains(&format!("--resume {two}")), "{text}");
        let conn = open(&dir);
        check_tables(&conn);
        check_rebuilt(&dir);
        assert_eq!(
            rows(&conn, "SELECT status, stop_message IS NOT NULL FROM run"),
            [["failed", "1"]]
        );
        assert_eq!(
            rows(&conn, "SELECT status FROM version ORDER BY id"),
            [["ok"], ["pending"]]
        );
        // 途中で止まったので，ロックのファイルと複製は残す
        assert!(dir.path().join("e.db.lock").exists());
        assert!(dir.path().join("e.db.clone").exists());
    }
}

#[test]
fn labels_retries_and_context_rows_of_the_classifier() {
    let dir = TempDir::new("labels");
    let root = repo(&dir, "repo");
    let text = words("a", 6);
    let one = commit(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "one",
    );
    let pair = [(("A.java", 1, 6), ("B.java", 1, 6))];
    let services = format!(
        r#"{{"versions": {{"{one}": {{"services": {{"s": ["A.java"], "t": ["B.java"]}},
            "dropped": ["deploy/old"], "fail_classify": 1}}}}}}"#
    );
    let output = record(
        &dir,
        &root,
        &graph(std::slice::from_ref(&one)),
        &pairs_json(&[(&one, &pair, 0)]),
        Some(&services),
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    // repo の最初のコミットが Git の親になるので，この版は起点にあったペアとして書く
    assert_eq!(
        rows(&conn, "SELECT label FROM fixture_root_pair"),
        [["inter"]]
    );
    assert_eq!(
        rows(&conn, "SELECT version, context FROM fixture_context_issue"),
        [["1", "deploy/old"]]
    );
    assert_eq!(
        rows(&conn, "SELECT plugin, attempt, message FROM plugin_failure"),
        [["2", "2", "classify: planned failure (fixture)"]]
    );
}

#[test]
fn the_first_commit_starts_from_the_empty_tree() {
    let dir = TempDir::new("empty-tree");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    let text = words("a", 6);
    let first = commit(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "first",
    );
    let pairs = pairs_json(&[(&first, &[(("A.java", 1, 6), ("B.java", 1, 6))], 0)]);
    let services = services_json(&[(&first, &[("s", &["A.java", "B.java"])])]);
    let output = record(
        &dir,
        &root,
        &graph(std::slice::from_ref(&first)),
        &pairs,
        Some(&services),
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(rows(&conn, "SELECT root FROM version"), [["empty_tree"]]);
    assert_eq!(
        rows(
            &conn,
            "SELECT parent, child, is_ancestor, skipped_versions FROM edge"
        ),
        [["NULL", "1", "NULL", "0"]]
    );
    assert_eq!(
        rows(&conn, "SELECT event, entered_scope FROM pair_event"),
        [["Appear", "0"]]
    );
    assert_eq!(
        rows(&conn, "SELECT label, changed FROM fixture_pair_event"),
        [["intra", "NULL"]]
    );
    assert_eq!(
        rows(&conn, "SELECT added, file_added FROM fragment_entry"),
        [["1", "1"], ["1", "1"]]
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM root_pair"), 0);
}

#[test]
fn a_stable_mapping_that_takes_in_lines_is_written() {
    // 空行が動き，同じ 3 つ組の子の片が外の行 Y を取り込んだ例
    let dir = TempDir::new("f01");
    let root = repo(&dir, "repo");
    let one = commit(
        &root,
        &[
            ("A.java", Some("x = 1;\ny = 2;\n\na = 3;\n")),
            ("B.java", Some("g = 0;\n")),
        ],
        "one",
    );
    let two = commit(
        &root,
        &[("A.java", Some("\nx = 1;\ny = 2;\na = 3;\n"))],
        "two",
    );
    let pair = [(("A.java", 3, 4), ("B.java", 1, 1))];
    let pairs = pairs_json(&[(&one, &pair, 0), (&two, &pair, 0)]);
    let output = record(
        &dir,
        &root,
        &graph(&[one.clone(), format!("{two} {one}")]),
        &pairs,
        None,
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT f.path, m.event, m.parent_lines, m.retained_lines, m.taken_in_lines, m.child_words
             FROM fragment_mapping m JOIN fragment_view f ON f.id = m.parent_fragment WHERE m.edge = 1"
        ),
        [["A.java", "Stable", "1", "1", "1", "4"]]
    );
    // B.java の片の対応とペアの Persist は書かない
    assert_eq!(
        rows(&conn, "SELECT implicit_pairs, implicit_fragments FROM edge"),
        [["1", "1"]]
    );
}

#[test]
fn a_file_replaced_by_a_link_is_a_deleted_file() {
    let dir = TempDir::new("typechange");
    let root = repo(&dir, "repo");
    let text = words("a", 6);
    let one = commit(
        &root,
        &[("A.java", Some(&text)), ("B.java", Some(&text))],
        "one",
    );
    std::fs::remove_file(root.join("B.java")).unwrap();
    std::os::unix::fs::symlink("A.java", root.join("B.java")).unwrap();
    let two = commit(&root, &[], "two");
    let pairs = pairs_json(&[
        (&one, &[(("A.java", 1, 6), ("B.java", 1, 6))], 0),
        (&two, &[], 0),
    ]);
    let output = record(
        &dir,
        &root,
        &graph(&[one.clone(), format!("{two} {one}")]),
        &pairs,
        None,
        2,
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT f.path, m.event, m.file_deleted FROM fragment_mapping m
             JOIN fragment_view f ON f.id = m.parent_fragment WHERE m.edge = 1"
        ),
        [["B.java", "Deleted", "1"]]
    );
}

#[test]
fn git_diff_opts_of_the_user_does_not_change_the_events() {
    let mut tables = Vec::new();
    for env in [&[][..], &[("GIT_DIFF_OPTS", "--unified=3")][..]] {
        let dir = TempDir::new("diffopts");
        let root = repo(&dir, "repo");
        let old = words("a", 12);
        let new = old.replace("a6 = 6;", "a6 = 66;").replace("a9 = 9;\n", "");
        let one = commit(
            &root,
            &[("A.java", Some(&old)), ("B.java", Some(&old))],
            "one",
        );
        let two = commit(&root, &[("A.java", Some(&new))], "two");
        let pair = [(("A.java", 1, 12), ("B.java", 1, 12))];
        let pairs = pairs_json(&[
            (&one, &pair, 0),
            (&two, &[(("A.java", 1, 11), ("B.java", 1, 12))], 0),
        ]);
        let output = record_env(
            &dir,
            &root,
            &graph(&[one.clone(), format!("{two} {one}")]),
            &pairs,
            None,
            2,
            env,
        );
        ok(&output);
        let conn = open(&dir);
        tables.push(rows(
            &conn,
            "SELECT * FROM fragment_mapping ORDER BY parent_fragment",
        ));
    }
    assert_eq!(tables[0], tables[1]);
    // a6 の行の修正と a9 の行の削除が数えられている
    assert!(
        tables[0]
            .iter()
            .any(|row| row.contains(&"Modified".to_string()))
    );
}

#[test]
fn several_classifiers_narrow_the_scope_together_and_label_each_pair() {
    let dir = TempDir::new("classifiers");
    let root = repo(&dir, "repo");
    let text = words("a", 6);
    let one = commit(
        &root,
        &[
            ("svc-a/src/A.java", Some(&text)),
            ("svc-a/src/test/ATest.java", Some(&text)),
            ("svc-b/B.java", Some(&text)),
            ("svc-b/lib/V.java", Some(&text)),
            ("tools/T.java", Some(&text)),
        ],
        "one",
    );
    let two = commit(
        &root,
        &[
            ("svc-b/lib/V.java", None),
            ("svc-b/vendor/V.java", Some(&text)),
        ],
        "two",
    );
    let a = ("svc-a/src/A.java", 1, 6);
    let a_test = ("svc-a/src/test/ATest.java", 1, 6);
    let b = ("svc-b/B.java", 1, 6);
    let v = ("svc-b/lib/V.java", 1, 6);
    dir.write(
        "pairs.json",
        &pairs_json(&[
            (&one, &[(a, a_test), (a, b), (b, v)], 0),
            (&two, &[(a, a_test), (a, b)], 0),
        ]),
    );
    let services_one: &[(&str, &[&str])] = &[
        ("a", &["svc-a/src/A.java", "svc-a/src/test/ATest.java"]),
        ("b", &["svc-b/B.java", "svc-b/lib/V.java"]),
    ];
    let services_two: &[(&str, &[&str])] = &[
        ("a", &["svc-a/src/A.java", "svc-a/src/test/ATest.java"]),
        ("b", &["svc-b/B.java", "svc-b/vendor/V.java"]),
    ];
    dir.write(
        "services.json",
        &services_json(&[(&one, services_one), (&two, services_two)]),
    );
    let output = record_config(
        &dir,
        &root,
        &graph(&[one.clone(), format!("{two} {one}")]),
        "[detector]\nname = \"fixture\"\nfile = \"pairs.json\"\n\
         [classifiers.vendor]\n[classifiers.test_path]\n\
         [classifiers.fixture]\nfile = \"services.json\"\n",
        &[],
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT id, kind, name, config FROM plugin ORDER BY id"
        )
        .into_iter()
        .map(|row| row[..3].join(" "))
        .collect::<Vec<_>>(),
        [
            "1 detector fixture",
            "2 classifier fixture",
            "3 classifier test_path",
            "4 classifier vendor"
        ]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT config FROM plugin WHERE name IN ('test_path', 'vendor') ORDER BY id"
        ),
        [["{}"], ["{}"]]
    );
    // vendor は表を持たない
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM sqlite_master WHERE name LIKE 'vendor%'"
        ),
        0
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT b.path, f.label, t.label FROM root_pair r
             JOIN fixture_root_pair f ON f.version = r.version AND f.pair = r.pair
             JOIN test_path_root_pair t ON t.version = r.version AND t.pair = r.pair
             JOIN pair p ON p.id = r.pair JOIN fragment_view b ON b.id = p.fragment_b
             ORDER BY b.path"
        ),
        [
            ["svc-a/src/test/ATest.java", "intra", "mixed"],
            ["svc-b/B.java", "inter", "product"],
            ["svc-b/lib/V.java", "intra", "product"],
        ]
    );
    // vendor の下に移ったファイルは，fixture が残しても範囲の外になる
    assert_eq!(
        rows(
            &conn,
            "SELECT edge, event, reason FROM pair_event ORDER BY id"
        ),
        [["1", "Disappear", "out_of_scope"]]
    );
    let output = run(&[
        "pairs",
        "--db",
        dir.path().join("e.db").to_str().unwrap(),
        "--commit",
        &two,
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let labels: Vec<String> = stdout(&output)
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["labels"].to_string()
        })
        .collect();
    assert_eq!(
        labels,
        [
            r#"{"fixture":"intra","test_path":"mixed"}"#,
            r#"{"fixture":"inter","test_path":"product"}"#
        ]
    );
}

#[test]
fn a_diff_that_git_cannot_make_deletes_the_old_lines_and_is_recorded_on_the_edge() {
    let dir = TempDir::new("diff-failed");
    let root = repo(&dir, "repo");
    let old = words("a", 6);
    let new = old.replace("a3 = 3;", "a3 = 33;");
    let one = commit(
        &root,
        &[("A.java", Some(&old)), ("B.java", Some(&old))],
        "one",
    );
    let two = commit(&root, &[("A.java", Some(&new))], "two");
    let failing = git(&root, &["rev-parse", &format!("{two}:A.java")]);
    // この blob への hunk を取る diff だけを，Git が出せなかった形で終わらせる
    let real = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let wrapper = bin.join("git");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" --histogram \"*\" {failing} \"*) echo 'fatal: injected' >&2; exit 128;; esac\nexec {} \"$@\"\n",
            real.trim()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let pairs = pairs_json(&[
        (&one, &[(("A.java", 1, 6), ("B.java", 1, 6))], 0),
        (&two, &[(("A.java", 1, 6), ("B.java", 1, 6))], 0),
    ]);
    let output = record_env(
        &dir,
        &root,
        &graph(&[one.clone(), format!("{two} {one}")]),
        &pairs,
        None,
        2,
        &[("PATH", &path)],
    );
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);
    assert_eq!(
        rows(
            &conn,
            "SELECT o.path, n.path, i.issue, i.hunks FROM edge_file_issue i
             JOIN path o ON o.id = i.old_path JOIN path n ON n.id = i.new_path"
        ),
        [["A.java", "A.java", "diff_failed", "NULL"]]
    );
    // 旧行が全部削除になるので，A の片は続かず，同じ 3 つ組でもペアは消えて新しく現れる．B の片は変わらないので書かない
    assert_eq!(
        rows(
            &conn,
            "SELECT p.path, m.event, m.retained_lines, m.file_deleted FROM fragment_mapping m
             JOIN fragment_view p ON p.id = m.parent_fragment ORDER BY p.path"
        ),
        [["A.java", "Deleted", "0", "0"]]
    );
    assert_eq!(
        rows(&conn, "SELECT event, reason FROM pair_event ORDER BY id"),
        [["Disappear", "fragment_deleted"], ["Appear", "NULL"]]
    );
}
