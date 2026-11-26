use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

use crate::core::lines::BlobLines;
use crate::core::model::{Fragment, Pair, RawFragment, RawPair, Scope, TreeEntry};

#[derive(Debug, PartialEq)]
pub struct Detected {
    pub pairs: BTreeMap<Pair, Option<f64>>,
}

impl Detected {
    pub fn fragments(&self) -> BTreeSet<&Fragment> {
        self.pairs
            .keys()
            .flat_map(|pair| [&pair.a, &pair.b])
            .collect()
    }
}

/// 行の範囲をここで確かめないのは，片のファイルの blob が要り，blob を読むのは record の役目だからである．
pub fn normalize(commit: &str, raw: Vec<RawPair>, scope: &Scope) -> Result<Detected> {
    // 同じパスの文字列を 1 つにまとめるのは，25 万ペアの版でもパスを片の数だけ写さないためである．
    let mut paths: HashMap<Vec<u8>, Rc<str>> = HashMap::new();
    let mut pairs: BTreeMap<Pair, Option<f64>> = BTreeMap::new();
    for raw_pair in raw {
        let a = fragment(commit, raw_pair.a, scope, &mut paths)?;
        let b = fragment(commit, raw_pair.b, scope, &mut paths)?;
        if let Some(similarity) = raw_pair.similarity
            && !(0.0..=1.0).contains(&similarity)
        {
            bail!(
                "invalid detector output for version {commit}: the pair of {a} and {b} has \
                 similarity {similarity}, which is not between 0 and 1"
            );
        }
        if a == b {
            bail!("invalid detector output for version {commit}: a pair of the same fragment {a}");
        }
        let pair = Pair::new(a, b)?;
        // 大きい方を残すのは，類似度をイベントの判定に使わず，どちらを残しても系譜が変わらないからである
        match pairs.entry(pair) {
            Entry::Vacant(entry) => {
                entry.insert(raw_pair.similarity);
            }
            Entry::Occupied(mut entry) => {
                let kept = entry.get_mut();
                *kept = match (*kept, raw_pair.similarity) {
                    (Some(x), Some(y)) => Some(x.max(y)),
                    (x, y) => x.or(y),
                };
            }
        }
    }
    Ok(Detected { pairs })
}

fn fragment(
    commit: &str,
    raw: RawFragment,
    scope: &Scope,
    paths: &mut HashMap<Vec<u8>, Rc<str>>,
) -> Result<Fragment> {
    let shown = format!(
        "{}:{}-{}",
        String::from_utf8_lossy(&raw.path),
        raw.start,
        raw.end
    );
    let invalid = |what: &str| {
        anyhow::anyhow!("invalid detector output for version {commit}: fragment {shown} {what}")
    };
    let Ok(text) = std::str::from_utf8(&raw.path) else {
        return Err(invalid("has a path that is not UTF-8"));
    };
    if !is_tree_path(&raw.path) {
        return Err(invalid("has a path not written as in the Git tree"));
    }
    if !scope.contains(&raw.path) {
        return Err(invalid("is out of the detection scope"));
    }
    // 終了行が u32 に収まらなければ，どのファイルの行の数も超えるので，ここで止めてよい．
    let (Ok(start), Ok(end)) = (u32::try_from(raw.start), u32::try_from(raw.end)) else {
        return Err(invalid("has an invalid line range"));
    };
    if start == 0 || start > end {
        return Err(invalid("has an invalid line range"));
    }
    let path = match paths.get(&raw.path) {
        Some(path) => Rc::clone(path),
        None => {
            let path: Rc<str> = Rc::from(text);
            paths.insert(raw.path, Rc::clone(&path));
            path
        }
    };
    Ok(Fragment { path, start, end })
}

/// 範囲の中かの確かめより先に見るのは，`./a` のような書き方の違いを，範囲の外とは別の誤りとして出すためである
fn is_tree_path(path: &[u8]) -> bool {
    !path.is_empty()
        && path
            .split(|&b| b == b'/')
            .all(|part| !part.is_empty() && part != b"." && part != b"..")
}

pub fn check_lines(
    commit: &str,
    detected: &Detected,
    lines: &HashMap<Rc<str>, Rc<BlobLines>>,
) -> Result<()> {
    for fragment in detected.fragments() {
        let blob = lines
            .get(&fragment.path)
            .unwrap_or_else(|| panic!("the lines of {} were not read", fragment.path));
        if fragment.end > blob.len() {
            bail!(
                "invalid detector output for version {commit}: fragment {fragment} ends after \
                 the last line {} of the file",
                blob.len()
            );
        }
        if fragment.lines().all(|i| blob.is_blank(i)) {
            bail!(
                "invalid detector output for version {commit}: fragment {fragment} has only blank lines"
            );
        }
    }
    Ok(())
}

pub fn scope_candidates(tree: &[TreeEntry]) -> BTreeSet<Vec<u8>> {
    tree.iter()
        .filter(|entry| entry.is_regular_file())
        .map(|entry| entry.path.clone())
        .collect()
}

/// 候補に無いファイルを黙って捨てないのは，プラグインの誤りを見逃さないためである．
pub fn narrow_scope(
    commit: &str,
    candidates: BTreeSet<Vec<u8>>,
    keeps: &[(&str, &BTreeSet<Vec<u8>>)],
) -> Result<Scope> {
    for (plugin, keep) in keeps {
        if let Some(path) = keep.iter().find(|path| !candidates.contains(*path)) {
            bail!(
                "invalid output of classifier {plugin} for version {commit}: it kept {:?}, \
                 which is not a regular file of the version",
                String::from_utf8_lossy(path)
            );
        }
    }
    let scope = candidates
        .into_iter()
        .filter(|path| keeps.iter().all(|(_, keep)| keep.contains(path)))
        .collect();
    Ok(Scope(scope))
}

pub fn scope_hash(scope: &Scope) -> String {
    let mut hasher = Sha256::new();
    for path in &scope.0 {
        hasher.update(path);
        hasher.update([0u8]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 表の CHECK に任せないのは，どのプラグインの誤りかを出してから止めるためである．
pub fn check_labels(
    plugin: &str,
    commit: &str,
    labels: &[&str],
    allowed: &[&str],
    pairs: usize,
) -> Result<()> {
    if labels.len() != pairs {
        bail!(
            "invalid output of classifier {plugin} for version {commit}: {} labels for {pairs} pairs",
            labels.len()
        );
    }
    if let Some(label) = labels.iter().find(|label| !allowed.contains(label)) {
        bail!(
            "invalid output of classifier {plugin} for version {commit}: unknown label {label:?}; \
             the labels are {}",
            allowed.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::ObjectKind;

    const COMMIT: &str = "3f2a9c1b7d4e3f2a9c1b7d4e3f2a9c1b7d4e3f2a";

    fn raw(path: &str, start: i64, end: i64) -> RawFragment {
        RawFragment {
            path: path.as_bytes().to_vec(),
            start,
            end,
        }
    }

    fn pair(a: RawFragment, b: RawFragment, similarity: Option<f64>) -> RawPair {
        RawPair { a, b, similarity }
    }

    fn scope(paths: &[&str]) -> Scope {
        Scope(paths.iter().map(|p| p.as_bytes().to_vec()).collect())
    }

    fn error(raw: Vec<RawPair>) -> String {
        normalize(COMMIT, raw, &scope(&["a.java", "b.java", "d/c.java"]))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn pairs_are_ordered_and_duplicates_keep_the_larger_similarity() {
        let detected = normalize(
            COMMIT,
            vec![
                pair(raw("b.java", 1, 5), raw("a.java", 3, 9), Some(0.7)),
                pair(raw("a.java", 3, 9), raw("b.java", 1, 5), Some(0.9)),
                pair(raw("a.java", 3, 9), raw("d/c.java", 2, 2), None),
                pair(raw("d/c.java", 2, 2), raw("a.java", 3, 9), Some(0.4)),
                pair(raw("a.java", 1, 2), raw("a.java", 3, 9), None),
            ],
            &scope(&["a.java", "b.java", "d/c.java"]),
        )
        .unwrap();
        let got: Vec<(String, String, Option<f64>)> = detected
            .pairs
            .iter()
            .map(|(p, s)| (p.a.to_string(), p.b.to_string(), *s))
            .collect();
        assert_eq!(
            got,
            [
                ("a.java:1-2".to_string(), "a.java:3-9".to_string(), None),
                (
                    "a.java:3-9".to_string(),
                    "b.java:1-5".to_string(),
                    Some(0.9)
                ),
                (
                    "a.java:3-9".to_string(),
                    "d/c.java:2-2".to_string(),
                    Some(0.4)
                ),
            ]
        );
        let fragments = detected.fragments();
        assert_eq!(fragments.len(), 4);
        // 同じパスの片は 1 つの文字列を指す
        let a: Vec<&Fragment> = fragments
            .iter()
            .copied()
            .filter(|f| &*f.path == "a.java")
            .collect();
        assert!(Rc::ptr_eq(&a[0].path, &a[1].path));
    }

    #[test]
    fn each_check_stops_with_the_version_and_the_fragment() {
        let ok = || raw("a.java", 1, 2);
        let mut not_utf8 = raw("a.java", 1, 2);
        not_utf8.path = b"\xffa.java".to_vec();
        for (bad, message) in [
            (
                not_utf8,
                "fragment \u{fffd}a.java:1-2 has a path that is not UTF-8",
            ),
            (
                raw("./a.java", 1, 2),
                "fragment ./a.java:1-2 has a path not written as in the Git tree",
            ),
            (
                raw("d//c.java", 1, 2),
                "has a path not written as in the Git tree",
            ),
            (
                raw("/a.java", 1, 2),
                "has a path not written as in the Git tree",
            ),
            (
                raw("e.java", 1, 2),
                "fragment e.java:1-2 is out of the detection scope",
            ),
            (
                raw("a.java", 0, 2),
                "fragment a.java:0-2 has an invalid line range",
            ),
            (raw("a.java", 3, 2), "has an invalid line range"),
            (raw("a.java", -1, 2), "has an invalid line range"),
            (raw("a.java", 1, 4_294_967_296), "has an invalid line range"),
        ] {
            let message_got = error(vec![pair(ok(), bad, None)]);
            assert!(
                message_got.starts_with(&format!("invalid detector output for version {COMMIT}: "))
            );
            assert!(message_got.ends_with(message), "{message_got}");
        }
        assert!(error(vec![pair(ok(), raw("b.java", 1, 2), Some(1.5))]).ends_with(
            "the pair of a.java:1-2 and b.java:1-2 has similarity 1.5, which is not between 0 and 1"
        ));
        assert!(
            error(vec![pair(ok(), raw("b.java", 1, 2), Some(f64::NAN))])
                .contains("has similarity NaN")
        );
        assert!(
            error(vec![pair(ok(), ok(), None)]).ends_with("a pair of the same fragment a.java:1-2")
        );
        assert!(
            normalize(
                COMMIT,
                vec![pair(
                    ok(),
                    raw("b.java", 4_294_967_295, 4_294_967_295),
                    None
                )],
                &scope(&["a.java", "b.java"])
            )
            .is_ok()
        );
    }

    #[test]
    fn line_ranges_are_checked_against_the_child_lines() {
        let detected = normalize(
            COMMIT,
            vec![pair(raw("a.java", 1, 3), raw("b.java", 2, 2), None)],
            &scope(&["a.java", "b.java"]),
        )
        .unwrap();
        let blob = |text: &str| Rc::new(BlobLines::new(text.as_bytes().to_vec()));
        let lines = |a: &str, b: &str| -> HashMap<Rc<str>, Rc<BlobLines>> {
            [(Rc::from("a.java"), blob(a)), (Rc::from("b.java"), blob(b))]
                .into_iter()
                .collect()
        };
        check_lines(COMMIT, &detected, &lines("x\ny\nz\n", "x\ny\n")).unwrap();
        let err = check_lines(COMMIT, &detected, &lines("x\ny\n", "x\ny\n")).unwrap_err();
        assert!(
            err.to_string()
                .ends_with("fragment a.java:1-3 ends after the last line 2 of the file"),
            "{err}"
        );
        let err = check_lines(COMMIT, &detected, &lines("x\ny\nz\n", "x\n \t\r\n")).unwrap_err();
        assert!(
            err.to_string()
                .ends_with("fragment b.java:2-2 has only blank lines"),
            "{err}"
        );
    }

    fn entry(mode: u32, kind: ObjectKind, path: &str) -> TreeEntry {
        TreeEntry {
            mode,
            kind,
            oid: "a".repeat(40),
            path: path.as_bytes().to_vec(),
        }
    }

    #[test]
    fn the_scope_is_the_regular_files_that_every_classifier_kept() {
        let tree = [
            entry(0o100644, ObjectKind::Blob, "a.java"),
            entry(0o100755, ObjectKind::Blob, "run.sh"),
            entry(0o120000, ObjectKind::Blob, "link"),
            entry(0o160000, ObjectKind::Commit, "sub"),
            entry(0o100644, ObjectKind::Blob, "test/T.java"),
        ];
        let candidates = scope_candidates(&tree);
        assert_eq!(candidates, scope(&["a.java", "run.sh", "test/T.java"]).0);
        assert_eq!(
            narrow_scope(COMMIT, candidates.clone(), &[]).unwrap().0,
            candidates
        );
        let no_tests = scope(&["a.java", "run.sh"]).0;
        let no_scripts = scope(&["a.java", "test/T.java"]).0;
        let narrowed = narrow_scope(
            COMMIT,
            candidates.clone(),
            &[("vendor", &no_tests), ("generated", &no_scripts)],
        )
        .unwrap();
        assert_eq!(narrowed, scope(&["a.java"]));
        let wrong = scope(&["a.java", "link"]).0;
        let err = narrow_scope(COMMIT, candidates, &[("vendor", &wrong)]).unwrap_err();
        assert!(err.to_string().contains("classifier vendor"), "{err}");
        assert!(
            err.to_string()
                .contains("it kept \"link\", which is not a regular file"),
            "{err}"
        );
    }

    #[test]
    fn the_scope_hash_is_the_sha256_of_paths_each_followed_by_nul() {
        // printf 'a.java\0b/c.java\0' | sha256sum
        assert_eq!(
            scope_hash(&scope(&["b/c.java", "a.java"])),
            "af151a52d180cb6686c56b7b67e57e3bb570f540124d69a67ba28c6e8aacf7b0"
        );
        // 空の範囲は空のバイト列の SHA-256
        assert_eq!(
            scope_hash(&Scope::default()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn labels_must_be_one_per_pair_and_known() {
        let allowed = ["intra", "inter"];
        check_labels("claim", COMMIT, &["intra", "inter"], &allowed, 2).unwrap();
        let err = check_labels("claim", COMMIT, &["intra"], &allowed, 2).unwrap_err();
        assert!(err.to_string().ends_with("1 labels for 2 pairs"), "{err}");
        let err = check_labels("claim", COMMIT, &["intra", "cross"], &allowed, 2).unwrap_err();
        assert!(err.to_string().contains("unknown label \"cross\""), "{err}");
    }
}
