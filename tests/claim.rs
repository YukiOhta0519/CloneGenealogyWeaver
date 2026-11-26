// docker-compose のファイルを置いた小さなリポジトリで，CLAIM のプラグインを確かめる．
// 仮想環境が無ければ，CLAIM を動かすテストは飛ばして，飛ばしたことを出す．

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use common::*;

fn venv_python() -> Option<PathBuf> {
    let python = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins/claim/.venv/bin/python");
    if python.exists() {
        Some(python)
    } else {
        eprintln!(
            "skipped: {} is missing; make it with python3 -m venv and \
             pip install -r plugins/claim/requirements.txt",
            python.display()
        );
        None
    }
}

fn words(prefix: &str, n: u32) -> String {
    (1..=n).map(|i| format!("{prefix}{i} = {i};\n")).collect()
}

fn ok(output: &std::process::Output) {
    assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
}

const COMPOSE: &str = "services:
  a:
    build: ./svc-a
  b:
    build:
      context: ../svc-b
  gone:
    build: ./missing
  cache:
    image: redis
";

#[test]
fn services_narrow_the_scope_and_label_the_pairs() {
    let Some(python) = venv_python() else {
        return;
    };
    let dir = TempDir::new("claim");
    let root = repo(&dir, "repo");
    let text = words("a", 6);
    // compose のファイルを deploy に置くと，CLAIM は deploy/svc-a のような context を返し，ツールが svc-a に直す
    let one = commit(
        &root,
        &[
            ("deploy/docker-compose.yml", Some(COMPOSE)),
            ("svc-a/A.java", Some(&text)),
            ("svc-a/A2.java", Some(&text)),
            ("svc-b/B.java", Some(&text)),
            ("tools/T.java", Some(&text)),
        ],
        "one",
    );
    let two = commit(
        &root,
        &[(
            "deploy/docker-compose.yml",
            Some("services:\n  a:\n    build: ./svc-a\n"),
        )],
        "two",
    );
    let three = commit(&root, &[("deploy/docker-compose.yml", None)], "three");
    let a = ("svc-a/A.java", 1, 6);
    let a2 = ("svc-a/A2.java", 1, 6);
    let b = ("svc-b/B.java", 1, 6);
    dir.write(
        "pairs.json",
        &pairs_json(&[
            (&one, &[(a, a2), (a, b)], 0),
            (&two, &[(a, a2)], 0),
            (&three, &[], 0),
        ]),
    );
    let config = format!(
        "retries = 1\n[detector]\nname = \"fixture\"\nfile = \"pairs.json\"\n\
         [classifiers.claim]\npython = {:?}\nclaim_name = \"owner.repo\"\ntimeout_secs = 600\n",
        python.to_str().unwrap()
    );
    let graph = format!("{one}\n{two} {one}\n{three} {two}\n");
    let output = record_config(&dir, &root, &graph, &config, &[]);
    ok(&output);
    let conn = open(&dir);
    check_tables(&conn);
    check_rebuilt(&dir);

    let config: serde_json::Value =
        serde_json::from_str(&rows(&conn, "SELECT config FROM plugin WHERE name = 'claim'")[0][0])
            .unwrap();
    assert_eq!(config["claim_name"], "owner.repo");
    assert_eq!(config["python"], python.to_str().unwrap());
    assert_eq!(config["packages"]["PyYAML"], "6.0.3");
    assert!(config["python_version"].as_str().unwrap().starts_with('3'));

    assert_eq!(
        rows(
            &conn,
            "SELECT b.path, l.label FROM claim_root_pair l
             JOIN pair p ON p.id = l.pair JOIN fragment_view b ON b.id = p.fragment_b
             ORDER BY b.path"
        ),
        [["svc-a/A2.java", "intra"], ["svc-b/B.java", "inter"]]
    );
    assert_eq!(
        rows(&conn, "SELECT version, context FROM claim_context_issue"),
        [["1", "deploy/missing"]]
    );
    // svc-b のサービスが無くなった版で，B.java は範囲の外になる
    assert_eq!(
        rows(
            &conn,
            "SELECT edge, event, reason FROM pair_event ORDER BY id"
        ),
        [["1", "Disappear", "out_of_scope"]]
    );
    // compose のファイルが無い版は，サービスが無いので壊れた版になる
    assert_eq!(
        rows(
            &conn,
            "SELECT id, status, failed_plugin, failure_message FROM version ORDER BY id"
        ),
        [
            ["1", "ok", "NULL", "NULL"],
            ["2", "ok", "NULL", "NULL"],
            [
                "3",
                "broken",
                "2",
                "scope: CLAIM found no service built from a context"
            ],
        ]
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM plugin_failure WHERE version = 3"
        ),
        2
    );
}

#[test]
fn a_virtual_environment_with_other_versions_is_refused() {
    let dir = TempDir::new("claim-versions");
    let root = repo(&dir, "repo");
    // 引数を読まずに版の JSON を出す偽の python で，版の確かめを試す
    let fake = dir.write(
        "python",
        r#"#!/bin/sh
echo '{"python": "3.12.3", "packages": {"PyYAML": "6.0.2", "dockerfile": "3.4.0", "parameter-expansion-patched": "0.3.1", "python-dotenv": "1.2.2"}}'
"#,
    );
    std::fs::set_permissions(&fake, PermissionsExt::from_mode(0o755)).unwrap();
    dir.write("pairs.json", r#"{"versions": {}}"#);
    let config = "[detector]\nname = \"fixture\"\nfile = \"pairs.json\"\n\
                  [classifiers.claim]\npython = \"./python\"\nclaim_name = \"owner.repo\"\n";
    let head = git(&root, &["rev-parse", "HEAD"]);
    let output = record_config(&dir, &root, &format!("{head}\n"), config, &[]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("[classifiers.claim]")
            && message.contains("has PyYAML 6.0.2, but claim needs 6.0.3"),
        "{message}"
    );
    assert!(!dir.path().join("e.db").exists());
}
