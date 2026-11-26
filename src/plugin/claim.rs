use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{PluginSection, resolve_program};
use crate::core::model::{Cell, Pair, TreeEntry};
use crate::error::PluginError;
use crate::plugin::{
    CheckDir, Classifier, Items, PluginCall, PluginRow, ScopeResult, check_error, classifier_label,
    deadline, describe_end, is_python_space, program_failure, run_check,
};
use crate::process::{Interrupt, ProgramEnd, ProgramSpec, run_program};

/// 実行ファイルに埋め込むのは，記録に使う CLAIM を，ツールの版で 1 つに決めるためである．
const CLAIM_PY: &[u8] = include_bytes!("../../plugins/claim/claim.py");
const ENTRY_PY: &[u8] = include_bytes!("../../plugins/claim/entry.py");

/// 固定した版．plugins/claim/requirements.txt と同じにする
const PACKAGES: [(&str, &str); 4] = [
    ("PyYAML", "6.0.3"),
    ("dockerfile", "3.4.0"),
    ("parameter-expansion-patched", "0.3.1"),
    ("python-dotenv", "1.2.2"),
];

pub struct Claim {
    python: PathBuf,
    claim_name: String,
    python_version: String,
    /// 直前の scope の版のサービスのディレクトリ．classify はこれでラベルを振る
    services: HashSet<Vec<u8>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Versions {
    python: String,
    packages: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Contexts {
    contexts: Vec<String>,
}

impl Claim {
    pub fn new(section: &PluginSection, base_dir: &Path, interrupt: &Interrupt) -> Result<Claim> {
        let items = Items::new(
            classifier_label(section),
            section,
            &["python", "claim_name"],
        )?;
        let label = items.label();
        let python = resolve_program(base_dir, items.required_string("python")?)
            .with_context(|| format!("{label} `python` of claim"))?;
        let claim_name = items.required_string("claim_name")?;
        // 版ごとに止めずに設定で止めるのは，claim.py が name.split('.')[1] を使い，
        // `.` が無いと全部の版が IndexError で壊れた版になるからである．
        if !claim_name
            .split_once('.')
            .is_some_and(|(owner, rest)| !owner.is_empty() && !rest.is_empty())
        {
            bail!(
                "{label} `claim_name` of claim must be like `owner.repository`, \
                 with text before and after the first `.`, not {claim_name:?}"
            );
        }

        let dir = CheckDir::new()?;
        let entry = write_scripts(dir.path()).map_err(check_error)?;
        let args = [
            OsString::from("-s"),
            OsString::from("-B"),
            entry.into(),
            "--versions".into(),
        ];
        let output = run_check(
            &ProgramSpec {
                program: &python,
                args: &args,
                temp_dir: dir.path(),
                deadline: deadline(section.timeout_secs),
                capture_stdout: true,
                watch: &[],
            },
            interrupt,
        )
        .with_context(|| format!("{label} cannot read the versions of the virtual environment"))?;
        let versions: Versions = serde_json::from_slice(&output.stdout).with_context(|| {
            format!("{label} entry.py --versions printed output that is not the expected JSON")
        })?;
        for (name, wanted) in PACKAGES {
            match versions.packages.get(name) {
                Some(found) if found == wanted => {}
                found => bail!(
                    "{label} {} has {name} {}, but claim needs {wanted}",
                    python.display(),
                    found.map_or("missing", String::as_str)
                ),
            }
        }
        Ok(Claim {
            python,
            claim_name: claim_name.to_string(),
            python_version: versions.python,
            services: HashSet::new(),
        })
    }
}

impl Classifier for Claim {
    fn name(&self) -> &'static str {
        "claim"
    }

    fn effective_config(&self) -> serde_json::Value {
        serde_json::json!({
            "claim_name": self.claim_name,
            "packages": BTreeMap::from(PACKAGES),
            "python": self.python.to_string_lossy(),
            "python_version": self.python_version,
        })
    }

    fn labels(&self) -> &'static [&'static str] {
        &["intra", "inter"]
    }

    fn scope(&mut self, call: &PluginCall, tree: &[TreeEntry]) -> Result<ScopeResult, PluginError> {
        let entry = write_scripts(call.temp_dir)?;
        let args = [
            OsString::from("-s"),
            OsString::from("-B"),
            entry.into(),
            OsString::from(&self.claim_name),
            call.work_tree.into(),
        ];
        let output = run_program(
            &ProgramSpec {
                program: &self.python,
                args: &args,
                temp_dir: call.temp_dir,
                deadline: call.deadline,
                capture_stdout: true,
                watch: &[],
            },
            call.interrupt,
        )?;
        if output.end != ProgramEnd::Exited(0) {
            return Err(program_failure(
                format!("entry.py {}", describe_end(output.end)),
                &output,
            ));
        }
        let contexts: Contexts = serde_json::from_slice(&output.stdout).map_err(|err| {
            program_failure(
                format!("entry.py printed output that is not the expected JSON: {err}"),
                &output,
            )
        })?;
        if contexts.contexts.is_empty() {
            return Err(PluginError::Failed {
                reason: "CLAIM found no service built from a context".to_string(),
            });
        }

        let root = std::fs::canonicalize(call.work_tree).map_err(|err| PluginError::Failed {
            reason: format!(
                "cannot resolve the work tree {}: {err}",
                call.work_tree.display()
            ),
        })?;
        let mut services = HashSet::new();
        let mut dropped = BTreeSet::new();
        for context in &contexts.contexts {
            let normalized = normalize(context);
            match resolve(call.work_tree, &root, &normalized)? {
                Some(dir) => {
                    services.insert(dir.into_bytes());
                }
                None => {
                    dropped.insert(normalized);
                }
            }
        }
        if services.is_empty() {
            return Err(PluginError::Failed {
                reason: format!(
                    "no build context from CLAIM is a directory of the work tree: {}",
                    dropped.into_iter().collect::<Vec<_>>().join(", ")
                ),
            });
        }
        let keep = tree
            .iter()
            .filter(|entry| entry.is_regular_file() && service_of(&services, &entry.path).is_some())
            .map(|entry| entry.path.clone())
            .collect();
        self.services = services;
        Ok(ScopeResult {
            keep: Some(keep),
            rows: dropped
                .into_iter()
                .map(|context| PluginRow {
                    table: "claim_context_issue",
                    values: vec![Cell::Text(context)],
                })
                .collect(),
        })
    }

    fn classify(&mut self, pairs: &[Pair]) -> Result<Vec<&'static str>, PluginError> {
        let service = |path: &str| {
            service_of(&self.services, path.as_bytes()).ok_or_else(|| {
                PluginError::Abort(anyhow::anyhow!(
                    "claim has no service for {path}, which is in the scope it returned"
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

/// 書けないことを止める誤りにしないのは，ディスクがいっぱいのような一時的な理由でも起き，やり直しで通りうるからである．
fn write_scripts(dir: &Path) -> Result<PathBuf, PluginError> {
    let write = |name: &str, bytes: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).map_err(|err| PluginError::Failed {
            reason: format!("cannot write {}: {err}", path.display()),
        })?;
        Ok(path)
    };
    write("claim.py", CLAIM_PY)?;
    write("entry.py", ENTRY_PY)
}

/// 空の文字列はリポジトリの根を表す．
fn normalize(context: &str) -> String {
    let replaced = context.replace('\\', "/");
    let trimmed = replaced.trim_matches(is_python_space);
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if trimmed == "." {
        return String::new();
    }
    trimmed.trim_matches('/').to_string()
}

/// 先頭の要素を 1 つずつ落とした候補のうち，作業ツリーの中のディレクトリになる最初のもの．
fn resolve(work_tree: &Path, root: &Path, normalized: &str) -> Result<Option<String>, PluginError> {
    let elements: Vec<&str> = if normalized.is_empty() {
        Vec::new()
    } else {
        normalized.split('/').collect()
    };
    for start in 0..elements.len().max(1) {
        let rest = &elements[start.min(elements.len())..];
        // 最初の候補も確かめるのは，`../svc` のような context で作業ツリーの外を見ないためである．
        if rest
            .iter()
            .any(|element| matches!(*element, "" | "." | ".."))
        {
            continue;
        }
        let candidate = rest.join("/");
        if candidate.contains('\0') {
            continue;
        }
        match is_inner_dir(&work_tree.join(&candidate), root) {
            Ok(true) => return Ok(Some(candidate)),
            Ok(false) => {}
            Err(err) if is_missing(&err) => {}
            Err(err) => {
                return Err(PluginError::Failed {
                    reason: format!("cannot look up the build context {candidate:?}: {err}"),
                });
            }
        }
    }
    Ok(None)
}

/// 中か外かをリンクを全部たどった先で比べるのは，作業ツリーの中から外を指すリンクを，無いものとして扱うためである．
fn is_inner_dir(path: &Path, root: &Path) -> std::io::Result<bool> {
    let resolved = std::fs::canonicalize(path)?;
    Ok(resolved.starts_with(root) && std::fs::metadata(&resolved)?.is_dir())
}

fn is_missing(err: &std::io::Error) -> bool {
    use rustix::io::Errno;
    matches!(
        Errno::from_io_error(err),
        Some(Errno::NOENT | Errno::NOTDIR | Errno::LOOP | Errno::NAMETOOLONG)
    )
}

/// 深い方から祖先を見るのは，入れ子の context で，ファイルを一番深いサービスに属させるためである．
fn service_of<'s>(services: &'s HashSet<Vec<u8>>, path: &[u8]) -> Option<&'s [u8]> {
    let mut end = path.len();
    while let Some(slash) = path[..end].iter().rposition(|&b| b == b'/') {
        if let Some(service) = services.get(&path[..slash]) {
            return Some(service);
        }
        end = slash;
    }
    services.get(&b""[..]).map(Vec::as_slice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn contexts_are_normalized_like_the_previous_analysis() {
        for (context, expected) in [
            ("svc", "svc"),
            ("./svc/", "svc"),
            ("  .\\svc\\a  ", "svc/a"),
            ("\u{1c}svc\u{1f}", "svc"),
            ("/abs/svc", "abs/svc"),
            (".", ""),
            ("./", ""),
            ("", ""),
            ("  ", ""),
            ("///", ""),
            ("././svc", "./svc"),
            ("a//b", "a//b"),
            ("../svc", "../svc"),
        ] {
            assert_eq!(normalize(context), expected, "{context:?}");
        }
    }

    struct Tree {
        base: PathBuf,
        work: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Tree {
            let base =
                std::env::temp_dir().join(format!("cgweave-claim-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let work = base.join("work");
            for dir in ["work/svc", "work/a/b", "work/deploy", "svc", "outside"] {
                std::fs::create_dir_all(base.join(dir)).unwrap();
            }
            std::fs::write(work.join("file"), "x").unwrap();
            symlink(base.join("outside"), work.join("out")).unwrap();
            symlink("a/b", work.join("in")).unwrap();
            symlink("loop", work.join("loop")).unwrap();
            Tree { base, work }
        }

        fn resolve(&self, context: &str) -> Option<String> {
            let root = std::fs::canonicalize(&self.work).unwrap();
            resolve(&self.work, &root, &normalize(context)).unwrap()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn contexts_map_to_the_longest_existing_suffix_inside_the_work_tree() {
        let tree = Tree::new("map");
        let cases = [
            // 外にも中にもある ../svc は，外を見ずに中の svc にする
            ("../svc", Some("svc")),
            ("deploy/svc", Some("svc")),
            ("x/y/a/b", Some("a/b")),
            ("a/b", Some("a/b")),
            ("out", None),
            ("x/out", None),
            ("in", Some("in")),
            ("loop", None),
            ("file", None),
            ("file/x", None),
            ("a/./b", None),
            ("a//b", None),
            ("missing", None),
            (".", Some("")),
            ("", Some("")),
        ];
        for (context, expected) in cases {
            assert_eq!(tree.resolve(context).as_deref(), expected, "{context:?}");
        }
        std::fs::create_dir_all(tree.work.join("b")).unwrap();
        assert_eq!(tree.resolve("a/./b").as_deref(), Some("b"));
        assert_eq!(tree.resolve("a//b").as_deref(), Some("b"));
        let long = "x".repeat(300);
        assert_eq!(tree.resolve(&long), None);
        assert_eq!(tree.resolve("svc\0x"), None);
    }

    #[test]
    fn files_belong_to_the_deepest_service() {
        let services: HashSet<Vec<u8>> = ["a", "a/b", ""]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let of = |path: &str| {
            service_of(&services, path.as_bytes()).map(|s| String::from_utf8(s.to_vec()).unwrap())
        };
        assert_eq!(of("a/b/C.java").as_deref(), Some("a/b"));
        assert_eq!(of("a/bc/C.java").as_deref(), Some("a"));
        assert_eq!(of("a/C.java").as_deref(), Some("a"));
        assert_eq!(of("C.java").as_deref(), Some(""));
        assert_eq!(of("x/a/C.java").as_deref(), Some(""));
        let services: HashSet<Vec<u8>> = [b"a".to_vec()].into_iter().collect();
        assert_eq!(service_of(&services, b"b/C.java"), None);
        assert_eq!(service_of(&services, b"a"), None);
    }

    fn section(rest: &str) -> PluginSection {
        PluginSection {
            name: "claim".to_string(),
            timeout_secs: None,
            rest: toml::from_str(rest).unwrap(),
        }
    }

    fn error_of(rest: &str) -> String {
        match Claim::new(&section(rest), Path::new("/"), &Interrupt::default()) {
            Ok(_) => panic!("accepted: {rest}"),
            Err(err) => format!("{err:#}"),
        }
    }

    #[test]
    fn items_are_checked_before_running_python() {
        assert!(error_of("claim_name = \"a.b\"").contains("claim needs `python`"));
        assert!(error_of("python = \"/bin/sh\"").contains("claim needs `claim_name`"));
        for name in ["ab", ".b", "a.", ""] {
            let message = error_of(&format!("python = \"/bin/sh\"\nclaim_name = {name:?}"));
            assert!(
                message.contains("must be like `owner.repository`"),
                "{message}"
            );
        }
        let message = error_of("python = \"no/python\"\nclaim_name = \"a.b\"");
        assert!(
            message.contains("`python` of claim: /no/python is not an executable file"),
            "{message}"
        );
        assert!(
            error_of("python = \"/bin/sh\"\nclaim_name = \"a.b\"\nname = 1")
                .contains("unknown item `name`")
        );
    }

    #[test]
    fn packages_match_requirements_txt() {
        let requirements = include_str!("../../plugins/claim/requirements.txt");
        let mut listed: Vec<(&str, &str)> = requirements
            .lines()
            .filter_map(|line| line.split_once("=="))
            .collect();
        listed.sort();
        assert_eq!(listed, PACKAGES);
    }
}
