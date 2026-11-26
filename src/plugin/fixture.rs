use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{PluginSection, resolve_path};
use crate::core::model::{Cell, Pair, RawFragment, RawPair, TreeEntry};
use crate::error::PluginError;
use crate::plugin::{
    Classifier, Detector, Items, PluginCall, PluginRow, ScopeResult, classifier_label,
    refuse_timeout,
};

const PLANNED: &str = "planned failure (fixture)";

/// JSON を設定を確かめるときに 1 回だけ読むのは，形の誤りで記録を始めないためである．
struct Source {
    path: PathBuf,
    sha256: String,
    bytes: Vec<u8>,
}

fn read_source(section: &PluginSection, base_dir: &Path, label: String) -> Result<Source> {
    refuse_timeout(&label, section)?;
    let items = Items::new(label, section, &["file"])?;
    let path = resolve_path(base_dir, items.required_string("file")?);
    let bytes =
        std::fs::read(&path).with_context(|| format!("cannot read fixture {}", path.display()))?;
    let path = std::fs::canonicalize(&path)
        .with_context(|| format!("cannot resolve fixture {}", path.display()))?;
    let sha256 = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(Source {
        path,
        sha256,
        bytes,
    })
}

fn effective(source: &Source) -> serde_json::Value {
    serde_json::json!({
        "file": source.path.to_string_lossy(),
        "sha256": source.sha256,
    })
}

/// 短いハッシュのキーを受け付けないのは，fixture は Git を呼ばず，版の完全なハッシュで引くからである
fn check_keys<'a>(source: &Source, keys: impl Iterator<Item = &'a String>) -> Result<()> {
    for key in keys {
        let hex = key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex || !(key.len() == 40 || key.len() == 64) {
            bail!(
                "fixture {}: version key {key:?} is not a full commit hash",
                source.path.display()
            );
        }
    }
    Ok(())
}

fn parse<T: for<'de> Deserialize<'de>>(source: &Source) -> Result<T> {
    serde_json::from_slice(&source.bytes)
        .with_context(|| format!("invalid fixture {}", source.path.display()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DetectorFile {
    versions: BTreeMap<String, DetectorVersion>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DetectorVersion {
    #[serde(default)]
    pairs: Vec<JsonPair>,
    #[serde(default)]
    fail: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonPair {
    a: JsonFragment,
    b: JsonFragment,
    #[serde(default)]
    similarity: Option<f64>,
    /// 受け付けて読み飛ばすのは，pairs のコマンドの出力をそのまま fixture に使えるようにするためである．
    #[serde(default)]
    #[allow(dead_code)]
    labels: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonFragment {
    path: String,
    start: i64,
    end: i64,
}

impl JsonFragment {
    fn raw(&self) -> RawFragment {
        RawFragment {
            path: self.path.as_bytes().to_vec(),
            start: self.start,
            end: self.end,
        }
    }
}

pub struct FixtureDetector {
    source: Source,
    versions: BTreeMap<String, DetectorVersion>,
    /// 何回目かを自分で数えるのは，本物のプラグインと同じ trait で呼び，trait に番号を渡さないからである．
    calls: HashMap<String, u64>,
}

impl FixtureDetector {
    pub fn new(section: &PluginSection, base_dir: &Path) -> Result<FixtureDetector> {
        let source = read_source(section, base_dir, "[detector]".to_string())?;
        let file: DetectorFile = parse(&source)?;
        check_keys(&source, file.versions.keys())?;
        Ok(FixtureDetector {
            source,
            versions: file.versions,
            calls: HashMap::new(),
        })
    }
}

impl Detector for FixtureDetector {
    fn effective_config(&self) -> serde_json::Value {
        effective(&self.source)
    }

    /// 範囲で絞らずに返すのは，範囲の外の片を返したアダプタの誤りを，ツールが止めるかを確かめられるようにするためである
    fn detect(
        &mut self,
        call: &PluginCall,
        _scope: &[Vec<u8>],
    ) -> Result<Vec<RawPair>, PluginError> {
        let calls = self.calls.entry(call.commit.to_string()).or_insert(0);
        *calls += 1;
        let Some(version) = self.versions.get(call.commit) else {
            return Ok(Vec::new());
        };
        if *calls <= version.fail {
            return Err(PluginError::Failed {
                reason: PLANNED.to_string(),
            });
        }
        Ok(version
            .pairs
            .iter()
            .map(|pair| RawPair {
                a: pair.a.raw(),
                b: pair.b.raw(),
                similarity: pair.similarity,
            })
            .collect())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassifierFile {
    versions: BTreeMap<String, ClassifierVersion>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassifierVersion {
    #[serde(default)]
    services: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    dropped: Vec<String>,
    #[serde(default)]
    fail_scope: u64,
    #[serde(default)]
    fail_classify: u64,
}

pub struct FixtureClassifier {
    source: Source,
    versions: BTreeMap<String, ClassifierVersion>,
    /// 版ごとに前もって作るのは，2 つのサービスに出るファイルを，設定を確かめる時点で止めるためである．
    service_of: HashMap<String, HashMap<Vec<u8>, String>>,
    /// scope と classify で回数を分けて数えるのは，plugin_failure の通しの番号で数えると，
    /// scope が成功した 1 回が classify の予定の失敗を 1 つ消すからである．
    scope_calls: HashMap<String, u64>,
    classify_calls: HashMap<String, u64>,
    current: Option<String>,
}

impl FixtureClassifier {
    pub fn new(section: &PluginSection, base_dir: &Path) -> Result<FixtureClassifier> {
        let source = read_source(section, base_dir, classifier_label(section))?;
        let file: ClassifierFile = parse(&source)?;
        check_keys(&source, file.versions.keys())?;
        let mut service_of = HashMap::new();
        for (commit, version) in &file.versions {
            let mut map: HashMap<Vec<u8>, String> = HashMap::new();
            for (service, files) in &version.services {
                for file in files {
                    if let Some(other) = map.insert(file.as_bytes().to_vec(), service.clone()) {
                        bail!(
                            "fixture {}: file {file:?} of version {commit} belongs to both {other} and {service}",
                            source.path.display()
                        );
                    }
                }
            }
            service_of.insert(commit.clone(), map);
        }
        Ok(FixtureClassifier {
            source,
            versions: file.versions,
            service_of,
            scope_calls: HashMap::new(),
            classify_calls: HashMap::new(),
            current: None,
        })
    }
}

impl Classifier for FixtureClassifier {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn effective_config(&self) -> serde_json::Value {
        effective(&self.source)
    }

    fn labels(&self) -> &'static [&'static str] {
        &["intra", "inter"]
    }

    fn scope(
        &mut self,
        call: &PluginCall,
        _tree: &[TreeEntry],
    ) -> Result<ScopeResult, PluginError> {
        self.current = Some(call.commit.to_string());
        let calls = self.scope_calls.entry(call.commit.to_string()).or_insert(0);
        *calls += 1;
        let version = self.versions.get(call.commit);
        if *calls <= version.map_or(0, |v| v.fail_scope) {
            return Err(PluginError::Failed {
                reason: PLANNED.to_string(),
            });
        }
        // サービスの無い版を失敗にするのは，CLAIM が context を返さない版と同じ扱いにするためである．
        let Some(version) = version.filter(|v| !v.services.is_empty()) else {
            return Err(PluginError::Failed {
                reason: format!("no services for version {} (fixture)", call.commit),
            });
        };
        let keep: BTreeSet<Vec<u8>> = version
            .services
            .values()
            .flatten()
            .map(|file| file.as_bytes().to_vec())
            .collect();
        let rows = version
            .dropped
            .iter()
            .map(|context| PluginRow {
                table: "fixture_context_issue",
                values: vec![Cell::Text(context.clone())],
            })
            .collect();
        Ok(ScopeResult {
            keep: Some(keep),
            rows,
        })
    }

    fn classify(&mut self, pairs: &[Pair]) -> Result<Vec<&'static str>, PluginError> {
        let commit = self
            .current
            .clone()
            .expect("classify is called after scope of the same version");
        let calls = self.classify_calls.entry(commit.clone()).or_insert(0);
        *calls += 1;
        let version = &self.versions[&commit];
        if *calls <= version.fail_classify {
            return Err(PluginError::Failed {
                reason: PLANNED.to_string(),
            });
        }
        let services = &self.service_of[&commit];
        let service = |path: &str| {
            services.get(path.as_bytes()).ok_or_else(|| {
                PluginError::Abort(anyhow::anyhow!(
                    "fixture has no service for {path} in version {commit}"
                ))
            })
        };
        pairs
            .iter()
            .map(|pair| {
                Ok(if service(&pair.a.path)? == service(&pair.b.path)? {
                    "intra"
                } else {
                    "inter"
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::Fragment;
    use crate::plugin::{Failure, with_retries};
    use crate::process::Interrupt;
    use std::num::NonZeroU64;
    use std::rc::Rc;

    const V1: &str = "6eebcac8871ae63d7f616c97e44bef71a2183272";

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let path =
                std::env::temp_dir().join(format!("cgweave-fixture-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn section(name: &str, items: &str, timeout: Option<u64>) -> PluginSection {
        PluginSection {
            name: name.to_string(),
            timeout_secs: timeout.and_then(NonZeroU64::new),
            rest: toml::from_str(items).unwrap(),
        }
    }

    fn call<'a>(commit: &'a str, dir: &'a Path, interrupt: &'a Interrupt) -> PluginCall<'a> {
        PluginCall {
            commit,
            work_tree: dir,
            temp_dir: dir,
            deadline: None,
            interrupt,
        }
    }

    fn detector(dir: &Dir, json: &str) -> Result<FixtureDetector> {
        std::fs::write(dir.0.join("pairs.json"), json).unwrap();
        FixtureDetector::new(&section("fixture", "file = \"pairs.json\"", None), &dir.0)
    }

    fn classifier(dir: &Dir, json: &str) -> Result<FixtureClassifier> {
        std::fs::write(dir.0.join("services.json"), json).unwrap();
        FixtureClassifier::new(
            &section("fixture", "file = \"services.json\"", None),
            &dir.0,
        )
    }

    fn pair(a: &str, b: &str) -> Pair {
        let f = |p: &str| Fragment {
            path: Rc::from(p),
            start: 1,
            end: 2,
        };
        Pair::new(f(a), f(b)).unwrap()
    }

    #[test]
    fn the_detector_returns_the_pairs_after_the_planned_failures() {
        let dir = Dir::new("detect");
        let json = format!(
            r#"{{"versions": {{"{V1}": {{"pairs": [{{"a": {{"path": "A.java", "start": 3, "end": 10}},
                "b": {{"path": "B.java", "start": 5, "end": 12}}, "similarity": 0.8,
                "labels": {{"claim": "intra"}}}}], "fail": 1}}}}}}"#
        );
        let mut detector = detector(&dir, &json).unwrap();
        let interrupt = Interrupt::default();
        let c = call(V1, &dir.0, &interrupt);
        assert!(
            matches!(detector.detect(&c, &[]), Err(PluginError::Failed { reason }) if reason == PLANNED)
        );
        let pairs = detector.detect(&c, &[]).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a.path, b"A.java");
        assert_eq!(pairs[0].similarity, Some(0.8));
        // JSON に無い版は，ペアの無い版である
        let other = "a".repeat(40);
        assert!(
            detector
                .detect(&call(&other, &dir.0, &interrupt), &[])
                .unwrap()
                .is_empty()
        );
        let config = detector.effective_config();
        assert_eq!(config["sha256"].as_str().unwrap().len(), 64);
        assert!(config["file"].as_str().unwrap().ends_with("/pairs.json"));
    }

    #[test]
    fn bad_fixtures_stop_when_the_config_is_checked() {
        let dir = Dir::new("bad");
        for json in [
            "{",
            r#"{"versions": {}, "extra": 1}"#,
            r#"{"versions": {"6eebcac": {}}}"#,
            r#"{"versions": {"6EEBCAC8871AE63D7F616C97E44BEF71A2183272": {}}}"#,
            &format!(
                r#"{{"versions": {{"{V1}": {{"pairs": [{{"a": {{"path": "A", "start": 1}}}}]}}}}}}"#
            ),
            &format!(r#"{{"versions": {{"{V1}": {{"fails": 1}}}}}}"#),
        ] {
            assert!(detector(&dir, json).is_err(), "{json}");
        }
        std::fs::write(dir.0.join("pairs.json"), r#"{"versions": {}}"#).unwrap();
        let err = FixtureDetector::new(
            &section("fixture", "file = \"pairs.json\"", Some(60)),
            &dir.0,
        )
        .err()
        .unwrap();
        assert!(
            err.to_string().contains("timeout_secs is not allowed"),
            "{err}"
        );
        let err = FixtureDetector::new(
            &section("fixture", "file = \"pairs.json\"\nfiles = 1", None),
            &dir.0,
        )
        .err()
        .unwrap();
        assert_eq!(
            err.to_string(),
            "[detector] unknown item `files` for fixture"
        );
        let err = FixtureDetector::new(&section("fixture", "", None), &dir.0)
            .err()
            .unwrap();
        assert!(err.to_string().contains("needs `file`"), "{err}");
    }

    #[test]
    fn a_file_in_two_services_stops_when_the_config_is_checked() {
        let dir = Dir::new("twice");
        let json = format!(
            r#"{{"versions": {{"{V1}": {{"services": {{"a": ["x.java"], "b": ["x.java"]}}}}}}}}"#
        );
        let err = classifier(&dir, &json).err().unwrap();
        assert!(err.to_string().contains("belongs to both a and b"), "{err}");
    }

    #[test]
    fn the_classifier_keeps_service_files_and_labels_pairs_by_service() {
        let dir = Dir::new("classify");
        let json = format!(
            r#"{{"versions": {{"{V1}": {{"services": {{"order": ["o/A.java", "o/B.java"], "pay": ["p/C.java"]}},
                "dropped": ["deploy/old"]}}}}}}"#
        );
        let mut classifier = classifier(&dir, &json).unwrap();
        let interrupt = Interrupt::default();
        let result = classifier
            .scope(&call(V1, &dir.0, &interrupt), &[])
            .unwrap();
        let keep: Vec<&[u8]> = result
            .keep
            .as_ref()
            .unwrap()
            .iter()
            .map(Vec::as_slice)
            .collect();
        assert_eq!(keep, [&b"o/A.java"[..], b"o/B.java", b"p/C.java"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].table, "fixture_context_issue");
        assert_eq!(
            result.rows[0].values,
            [Cell::Text("deploy/old".to_string())]
        );
        let labels = classifier
            .classify(&[pair("o/A.java", "o/B.java"), pair("o/A.java", "p/C.java")])
            .unwrap();
        assert_eq!(labels, ["intra", "inter"]);
        // サービスの無い版は，範囲を決められなかった失敗である
        let other = "b".repeat(40);
        assert!(matches!(
            classifier.scope(&call(&other, &dir.0, &interrupt), &[]),
            Err(PluginError::Failed { .. })
        ));
    }

    #[test]
    fn a_classify_failure_after_a_successful_scope_has_attempt_number_2() {
        let dir = Dir::new("numbers");
        let json = format!(
            r#"{{"versions": {{"{V1}": {{"services": {{"s": ["A.java", "B.java"]}}, "fail_scope": 0, "fail_classify": 1}}}}}}"#
        );
        let mut classifier = classifier(&dir, &json).unwrap();
        let interrupt = Interrupt::default();
        let (mut number, mut failures) = (1, Vec::new());
        let scoped = with_retries(2, 2, &mut number, &mut failures, |_| {
            classifier.scope(&call(V1, &dir.0, &interrupt), &[])
        })
        .unwrap();
        assert!(scoped.is_some());
        let labels = with_retries(2, 2, &mut number, &mut failures, |_| {
            classifier
                .classify(&[pair("A.java", "B.java")])
                .map_err(|err| match err {
                    PluginError::Failed { reason } => PluginError::Failed {
                        reason: format!("classify: {reason}"),
                    },
                    other => other,
                })
        })
        .unwrap();
        assert_eq!(labels, Some(vec!["intra"]));
        assert_eq!(
            failures,
            [Failure {
                plugin: 2,
                attempt: 2,
                message: "classify: planned failure (fixture)".to_string()
            }]
        );
    }
}
