use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};

use crate::core::model::Resolution;
use crate::graph::{Graph, VersionIdx};

pub struct Line {
    pub number: usize,
    pub version: String,
    pub parents: Vec<String>,
}

pub struct Parsed {
    pub lines: Vec<Line>,
}

impl Parsed {
    /// 同じ文字列を 1 回だけ返すのは，Git で 1 回だけ直すためである．
    pub fn revisions(&self) -> Vec<&str> {
        let mut seen = std::collections::HashSet::new();
        self.lines
            .iter()
            .flat_map(|line| std::iter::once(&line.version).chain(&line.parents))
            .map(String::as_str)
            .filter(|rev| seen.insert(*rev))
            .collect()
    }
}

pub fn parse(name: &str, bytes: &[u8]) -> Result<Parsed> {
    parse_with_limit(name, bytes, u32::MAX as usize)
}

/// 版の数の上限を引数にするのは，4,294,967,295 行のファイルを作らずにテストするためである
fn parse_with_limit(name: &str, bytes: &[u8], max_versions: usize) -> Result<Parsed> {
    let mut lines = Vec::new();
    for (i, line) in bytes.split(|&b| b == b'\n').enumerate() {
        let number = i + 1;
        // u8::is_ascii_whitespace に任せるのは，空白，タブ，CR，改ページの 4 つで，垂直タブが入らないからである．
        let mut tokens = line
            .split(u8::is_ascii_whitespace)
            .filter(|token| !token.is_empty())
            .map(|token| hash_text(name, number, token));
        let Some(version) = tokens.next() else {
            continue;
        };
        let version = version?;
        let parents = tokens.collect::<Result<Vec<String>>>()?;
        lines.push(Line {
            number,
            version,
            parents,
        });
        if lines.len() > max_versions {
            bail!("invalid graph {name}: more than {max_versions} versions");
        }
    }
    if lines.is_empty() {
        bail!("invalid graph {name}: no versions");
    }
    Ok(Parsed { lines })
}

/// 大文字を受け付けないのは，git rev-list が出す小文字の 1 つにそろえるためである．
fn hash_text(name: &str, number: usize, token: &[u8]) -> Result<String> {
    let lower_hex = token
        .iter()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b));
    if !lower_hex || !(4..=64).contains(&token.len()) {
        bail!(
            "invalid graph {name} at line {number}: {:?} is not a commit hash",
            String::from_utf8_lossy(token)
        );
    }
    Ok(String::from_utf8(token.to_vec()).expect("hex digits are ASCII"))
}

/// 検査を種類ごとに全部の行で順に行うのは，決まった検査の順で最初に見つけた誤りを出すためである
pub fn build(name: &str, parsed: &Parsed, resolved: &HashMap<String, Resolution>) -> Result<Graph> {
    let at = |number: usize, message: String| {
        anyhow!("invalid graph {name} at line {number}: {message}")
    };
    let tokens = |line: &Line| -> Vec<String> {
        std::iter::once(&line.version)
            .chain(&line.parents)
            .cloned()
            .collect()
    };
    let full = |rev: &str| -> &Resolution {
        resolved
            .get(rev)
            .unwrap_or_else(|| panic!("{rev:?} was not resolved before build"))
    };

    // 2: 完全なハッシュに直せるか
    for line in &parsed.lines {
        for rev in tokens(line) {
            if let Resolution::Unresolved { reason } = full(&rev) {
                return Err(at(
                    line.number,
                    format!("cannot resolve {rev:?} to a commit: {reason}"),
                ));
            }
        }
    }
    let commit = |rev: &str| -> String {
        match full(rev) {
            Resolution::Commit(commit) => commit.clone(),
            Resolution::Unresolved { .. } => unreachable!("checked above"),
        }
    };
    // 3: 直した結果が，書いた文字列で始まるか
    for line in &parsed.lines {
        for rev in tokens(line) {
            let commit = commit(&rev);
            if !commit.starts_with(&rev) {
                return Err(at(
                    line.number,
                    format!(
                        "{rev:?} resolved to {}, which does not start with it; \
                         a tag or branch may have the same name",
                        short(&commit)
                    ),
                ));
            }
        }
    }
    // 4: 同じ版が 2 行に出ないか
    let mut index: HashMap<String, (VersionIdx, usize)> = HashMap::new();
    for (i, line) in parsed.lines.iter().enumerate() {
        let commit = commit(&line.version);
        if let Some((_, first)) = index.get(&commit) {
            return Err(at(
                line.number,
                format!("version {} also appears at line {first}", short(&commit)),
            ));
        }
        index.insert(commit, (VersionIdx(i as u32), line.number));
    }
    // 5: 1 行に同じ親が 2 回出ないか
    for line in &parsed.lines {
        let mut seen = std::collections::HashSet::new();
        for parent in &line.parents {
            let commit = commit(parent);
            if !seen.insert(commit.clone()) {
                return Err(at(
                    line.number,
                    format!("parent {} appears twice", short(&commit)),
                ));
            }
        }
    }
    // 6: 親に書かれた版が一覧の中にあるか
    let mut versions = Vec::with_capacity(parsed.lines.len());
    for line in &parsed.lines {
        let mut parents = Vec::with_capacity(line.parents.len());
        for parent in &line.parents {
            let commit = commit(parent);
            let Some((idx, _)) = index.get(&commit) else {
                return Err(at(
                    line.number,
                    format!("parent {} is not a version in the graph", short(&commit)),
                ));
            };
            parents.push(*idx);
        }
        versions.push((commit(&line.version), parents));
    }
    // 7: 閉路が無いか
    Graph::new(versions).map_err(|v| {
        let line = &parsed.lines[v.index()];
        anyhow!(
            "invalid graph {name}: version {} at line {} is on a cycle",
            short(&commit(&line.version)),
            line.number
        )
    })
}

/// 誤りの文のハッシュを先頭 12 文字にするのは，ツールの版の書き方とそろえるためである
fn short(commit: &str) -> &str {
    &commit[..12.min(commit.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(c: char) -> String {
        c.to_string().repeat(40)
    }

    /// 文字列を，先頭の 1 文字を 40 回並べた完全なハッシュに直したことにする
    fn resolve_all(parsed: &Parsed) -> HashMap<String, Resolution> {
        parsed
            .revisions()
            .into_iter()
            .map(|rev| {
                (
                    rev.to_string(),
                    Resolution::Commit(hash(rev.chars().next().unwrap())),
                )
            })
            .collect()
    }

    fn error_of(text: &str, adjust: impl FnOnce(&mut HashMap<String, Resolution>)) -> String {
        let parsed = match parse("g.txt", text.as_bytes()) {
            Ok(parsed) => parsed,
            Err(err) => return err.to_string(),
        };
        let mut resolved = resolve_all(&parsed);
        adjust(&mut resolved);
        match build("g.txt", &parsed, &resolved) {
            Ok(_) => panic!("accepted: {text:?}"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn lines_are_split_on_four_kinds_of_ascii_whitespace() {
        let text = "aaaa\r\n\nbbbb\taaaa\x0c\r\n  \ncccc  bbbb aaaa";
        let parsed = parse("g.txt", text.as_bytes()).unwrap();
        let lines: Vec<(usize, &str, Vec<&str>)> = parsed
            .lines
            .iter()
            .map(|l| {
                (
                    l.number,
                    l.version.as_str(),
                    l.parents.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            lines,
            [
                (1, "aaaa", vec![]),
                (3, "bbbb", vec!["aaaa"]),
                (5, "cccc", vec!["bbbb", "aaaa"]),
            ]
        );
        assert_eq!(parsed.revisions(), ["aaaa", "bbbb", "cccc"]);
    }

    #[test]
    fn only_lowercase_hex_of_4_to_64_characters_is_a_commit_hash() {
        for (text, bad) in [
            ("abcd\x0bef01\n", "abcd\\u{b}ef01"),
            ("ABCD\n", "ABCD"),
            ("abc\n", "abc"),
            ("aaaa HEAD\n", "HEAD"),
            ("aaaa main~3\n", "main~3"),
        ] {
            assert_eq!(
                error_of(text, |_| {}),
                format!("invalid graph g.txt at line 1: \"{bad}\" is not a commit hash")
            );
        }
        let long = "a".repeat(65);
        assert!(error_of(&long, |_| {}).ends_with("is not a commit hash"));
        let longest = "a".repeat(64);
        assert!(parse("g.txt", longest.as_bytes()).is_ok());
    }

    #[test]
    fn a_graph_without_versions_is_refused() {
        assert_eq!(
            error_of("\n \r\n", |_| {}),
            "invalid graph g.txt: no versions"
        );
        assert_eq!(error_of("", |_| {}), "invalid graph g.txt: no versions");
    }

    #[test]
    fn too_many_versions_are_refused_when_the_lines_are_read() {
        let err = parse_with_limit("g.txt", b"aaaa\nbbbb\ncccc\n", 2)
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "invalid graph g.txt: more than 2 versions");
        assert!(parse_with_limit("g.txt", b"aaaa\nbbbb\n", 2).is_ok());
    }

    #[test]
    fn an_unresolved_hash_gives_the_reason_of_git() {
        let message = error_of("aaaa\nbbbb aaaa\n", |r| {
            r.insert(
                "bbbb".to_string(),
                Resolution::Unresolved {
                    reason: "fatal: Needed a single revision".to_string(),
                },
            );
        });
        assert_eq!(
            message,
            "invalid graph g.txt at line 2: cannot resolve \"bbbb\" to a commit: fatal: Needed a single revision"
        );
    }

    #[test]
    fn a_hash_that_resolved_to_another_commit_is_refused() {
        let message = error_of("aaaa\nbbbb aaaa\n", |r| {
            r.insert("bbbb".to_string(), Resolution::Commit(hash('6')));
        });
        assert_eq!(
            message,
            "invalid graph g.txt at line 2: \"bbbb\" resolved to 666666666666, which does not start with it; \
             a tag or branch may have the same name"
        );
    }

    #[test]
    fn the_checks_run_in_their_order_over_all_lines() {
        // 3 行目の 3 の誤りより，4 行目の 2 の誤りを先に出す
        let message = error_of("aaaa\nbbbb\ncccc\ndddd\n", |r| {
            r.insert("cccc".to_string(), Resolution::Commit(hash('6')));
            r.insert(
                "dddd".to_string(),
                Resolution::Unresolved {
                    reason: "x".to_string(),
                },
            );
        });
        assert!(
            message.starts_with("invalid graph g.txt at line 4: cannot resolve"),
            "{message}"
        );
    }

    #[test]
    fn duplicates_and_unknown_parents_are_refused_at_their_lines() {
        // 短く書いた版と完全なハッシュで書いた版も，直した後で同じ版とみなす
        let message = error_of(&format!("aaaa\nbbbb aaaa\n{}\n", hash('a')), |_| {});
        assert_eq!(
            message,
            "invalid graph g.txt at line 3: version aaaaaaaaaaaa also appears at line 1"
        );
        let message = error_of(&format!("aaaa\nbbbb aaaa {}\n", hash('a')), |_| {});
        assert_eq!(
            message,
            "invalid graph g.txt at line 2: parent aaaaaaaaaaaa appears twice"
        );
        let message = error_of("aaaa\nbbbb cccc\n", |_| {});
        assert_eq!(
            message,
            "invalid graph g.txt at line 2: parent cccccccccccc is not a version in the graph"
        );
    }

    #[test]
    fn a_cycle_is_refused_with_the_line_of_a_version_on_it() {
        let message = error_of("aaaa\nbbbb aaaa cccc\ncccc bbbb\n", |_| {});
        assert!(
            message == "invalid graph g.txt: version bbbbbbbbbbbb at line 2 is on a cycle"
                || message == "invalid graph g.txt: version cccccccccccc at line 3 is on a cycle",
            "{message}"
        );
        let message = error_of("aaaa aaaa\n", |_| {});
        assert_eq!(
            message,
            "invalid graph g.txt: version aaaaaaaaaaaa at line 1 is on a cycle"
        );
    }

    #[test]
    fn a_valid_graph_keeps_the_line_order_and_the_parent_order() {
        let parsed = parse("g.txt", b"cccc bbbb aaaa\naaaa\nbbbb aaaa\n").unwrap();
        let graph = build("g.txt", &parsed, &resolve_all(&parsed)).unwrap();
        assert_eq!(graph.versions[0].commit, hash('c'));
        assert_eq!(graph.versions[0].parents, [VersionIdx(2), VersionIdx(1)]);
        let order: Vec<u32> = graph.order.iter().map(|v| v.0).collect();
        assert_eq!(order, [1, 2, 0]);
    }
}
