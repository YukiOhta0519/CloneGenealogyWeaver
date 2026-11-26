// 小さな Java のプロジェクトで，MSCCD のアダプタを確かめる．
// MSCCD は環境変数 CGWEAVE_TEST_MSCCD_DIR のディレクトリから探し，無ければ飛ばして，飛ばしたことを出す．

mod common;

use std::path::{Path, PathBuf};

use common::*;

fn msccd_dir() -> Option<PathBuf> {
    let Some(dir) = std::env::var_os("CGWEAVE_TEST_MSCCD_DIR").map(PathBuf::from) else {
        eprintln!("skipped: set CGWEAVE_TEST_MSCCD_DIR to the MSCCD directory");
        return None;
    };
    if dir.join("modules/msccd_detector.jar").exists() {
        Some(dir)
    } else {
        eprintln!(
            "skipped: MSCCD is not at {}; set CGWEAVE_TEST_MSCCD_DIR",
            dir.display()
        );
        None
    }
}

/// 33 行で LF で終わる．MSCCD はファイル全体の bag の終了行を 34 にする
fn class(name: &str) -> String {
    format!(
        "package demo;

import java.util.ArrayList;
import java.util.List;

public class {name} {{
    private int count = 0;

    public int sumList(List<Integer> values, int offset) {{
        int total = 0;
        for (int i = 0; i < values.size(); i++) {{
            int v = values.get(i);
            if (v > offset) {{
                total += v * 2;
            }} else {{
                total -= v + offset;
            }}
        }}
        count = count + total;
        return total + count;
    }}

    public String describe(String name, int level) {{
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < level; i++) {{
            sb.append(name);
            sb.append(i);
            sb.append(\",\");
        }}
        sb.append(count);
        return sb.toString() + name.length();
    }}
}}
"
    )
}

/// 1 行に同じメソッドを 2 つ書く．2 つの bag が同じ 3 つ組になる
const ONE_LINE: &str = "package demo;
public class D {
    int one(int[] v, int o) { int t = 0; for (int i = 0; i < v.length; i++) { if (v[i] > o) { t += v[i] * 2; } else { t -= v[i] + o; } } return t + o; } int two(int[] v, int o) { int t = 0; for (int i = 0; i < v.length; i++) { if (v[i] > o) { t += v[i] * 2; } else { t -= v[i] + o; } } return t + o; }
}
";

fn config(msccd: &Path, extra: &str) -> String {
    format!(
        "retries = 0\n[detector]\nname = \"msccd\"\nmsccd_dir = {:?}\n{extra}",
        msccd.to_str().unwrap()
    )
}

fn pairs_of(dir: &TempDir, commit: &str) -> Vec<String> {
    let output = run(&[
        "pairs",
        "--db",
        dir.path().join("e.db").to_str().unwrap(),
        "--commit",
        commit,
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    stdout(&output).lines().map(str::to_string).collect()
}

#[test]
fn msccd_pairs_are_recorded_with_the_eof_line_fixed() {
    let Some(msccd) = msccd_dir() else {
        return;
    };
    let dir = TempDir::new("msccd");
    let root = repo(&dir, "repo");
    // パスのカンマは，fileList.txt に書かないので壊れない
    let one = commit(
        &root,
        &[
            ("src/A.java", Some(&class("A"))),
            ("src/dir,comma/B.java", Some(&class("B"))),
            ("src/D.java", Some(ONE_LINE)),
            ("notes.txt", Some(&class("N"))),
        ],
        "one",
    );
    let two = commit(&root, &[("README.md", Some("x\n"))], "two");
    let output = record_command(
        &dir,
        &root,
        &format!("{one}\n{two} {one}\n"),
        &config(&msccd, ""),
    )
    .args(["--log-level", "debug"])
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // D.java の 2 つの bag は同じ 3 つ組になり，アダプタが落とさなければ，ツールが同じ片どうしのペアとして止める
    assert!(
        stderr(&output).contains(&format!(
            "MSCCD paired a fragment with itself 1 times in version {one}; dropped them"
        )),
        "{}",
        stderr(&output)
    );
    let conn = open(&dir);
    check_tables(&conn);

    let config: serde_json::Value =
        serde_json::from_str(&rows(&conn, "SELECT config FROM plugin WHERE id = 1")[0][0]).unwrap();
    assert_eq!(config["minTokens"], 20);
    assert_eq!(config["detectionThreshold"], 0.7);
    assert_eq!(config["tokenizer"], "Java");
    assert!(
        config["java_version"]
            .as_str()
            .unwrap()
            .contains("version \"")
    );
    assert_eq!(config["sha256"].as_object().unwrap().len(), 4);
    assert!(
        config["keywordsList"]
            .as_str()
            .unwrap()
            .ends_with("grammarDefinations/java/Java.reserved")
    );

    let pairs = pairs_of(&dir, &one);
    // ファイル全体の片は，34 でなく 33 行で終わる
    assert!(
        pairs.contains(
            &r#"{"a":{"path":"src/A.java","start":1,"end":33},"b":{"path":"src/dir,comma/B.java","start":1,"end":33},"labels":{},"similarity":null}"#.to_string()
        ),
        "{pairs:#?}"
    );
    assert!(
        pairs.iter().all(|pair| !pair.contains("notes.txt")),
        "{pairs:#?}"
    );
    assert_eq!(pairs_of(&dir, &two), pairs);
}

#[test]
fn tokenizer_threads_do_not_change_the_pairs() {
    let Some(msccd) = msccd_dir() else {
        return;
    };
    let files: Vec<(String, String)> = (0..12)
        .map(|i| (format!("src/C{i}.java"), class(&format!("C{i}"))))
        .collect();
    let mut results = Vec::new();
    for threads in [1, 8] {
        let dir = TempDir::new("msccd-threads");
        let root = repo(&dir, "repo");
        let entries: Vec<(&str, Option<&str>)> = files
            .iter()
            .map(|(path, text)| (path.as_str(), Some(text.as_str())))
            .collect();
        let one = commit(&root, &entries, "one");
        let output = record_config(
            &dir,
            &root,
            &format!("{one}\n"),
            &config(&msccd, &format!("threadNum_tokenizer = {threads}\n")),
            &[],
        );
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        results.push(pairs_of(&dir, &one));
    }
    assert!(!results[0].is_empty());
    assert_eq!(results[0], results[1]);
}

#[test]
fn the_time_limit_covers_the_tokenizer() {
    let Some(msccd) = msccd_dir() else {
        return;
    };
    let dir = TempDir::new("msccd-timeout");
    let root = repo(&dir, "repo");
    let one = commit(&root, &[("A.java", Some(&class("A")))], "one");
    // tokenizer は 1 回に 5 秒ほど待つので，1 秒の上限を超える
    let output = record_config(
        &dir,
        &root,
        &format!("{one}\n"),
        &config(&msccd, "timeout_secs = 1\n"),
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let conn = open(&dir);
    let version = &rows(
        &conn,
        "SELECT status, failed_plugin, failure_message FROM version",
    )[0];
    assert_eq!(version[..2], ["broken", "1"]);
    assert!(
        version[2].starts_with("the tokenizer of MSCCD ran past timeout_secs"),
        "{}",
        version[2]
    );
}

#[test]
fn settings_that_cannot_work_stop_before_recording() {
    let Some(msccd) = msccd_dir() else {
        return;
    };
    let dir = TempDir::new("msccd-config");
    let root = repo(&dir, "repo");
    let head = git(&root, &["rev-parse", "HEAD"]);
    dir.write("empty.reserved", "");
    let mut cases = vec![
        ("tokenizer = \"Python\"\n", "supports only \"Java\""),
        ("keywordsList = \"empty.reserved\"\n", "is empty"),
        (
            "keywordsList = \"missing.reserved\"\n",
            "cannot resolve keywordsList",
        ),
        ("heap_tokenizer = \"8G\"\n", "must be a size like"),
        ("threadNum_detection = 0\n", "must be an integer from 1"),
        ("detectionThreshold = 0\n", "greater than 0 and at most 1"),
        (
            "languageExtensionName = \".a,b\"\n",
            "languageExtensionName",
        ),
        (
            "java = \"cgweave-no-such-java\"\n",
            "cannot find `cgweave-no-such-java` in PATH",
        ),
        ("minToken = 20\n", "unknown item `minToken` for msccd"),
    ];
    let java17_item = std::env::var_os("CGWEAVE_TEST_JAVA17")
        .map(PathBuf::from)
        .filter(|java| java.exists())
        .map(|java| format!("java = {:?}\n", java.to_str().unwrap()));
    match &java17_item {
        Some(item) => cases.push((item.as_str(), "needs Java 18 or later")),
        None => {
            eprintln!("skipped the Java 17 case: set CGWEAVE_TEST_JAVA17 to the java of Java 17")
        }
    }
    for (extra, expected) in cases {
        let output = record_config(
            &dir,
            &root,
            &format!("{head}\n"),
            &config(&msccd, extra),
            &[],
        );
        assert_eq!(output.status.code(), Some(1), "{extra}");
        let message = stderr(&output);
        assert!(
            message.contains("invalid config") && message.contains(expected),
            "{extra}: {message}"
        );
        assert!(!dir.path().join("e.db").exists(), "{extra}");
    }
}
