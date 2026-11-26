pub mod claim;
pub mod fixture;
pub mod generated;
pub mod msccd;
pub mod test_path;
pub mod vendor;

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::PluginSection;
use crate::core::model::{Cell, Pair, RawPair, TreeEntry};
use crate::error::PluginError;
use crate::process::{Interrupt, ProgramEnd, ProgramOutput, ProgramSpec, run_program};

/// 2 つのパスを絶対パスにするのは，作業ディレクトリを一時ファイルのディレクトリにした外部のプログラムに，
/// そのまま渡せるようにするためである．
pub struct PluginCall<'a> {
    pub commit: &'a str,
    pub work_tree: &'a Path,
    pub temp_dir: &'a Path,
    pub deadline: Option<Instant>,
    pub interrupt: &'a Interrupt,
}

/// 表せない締め切りを上限なしにするのは，そこまで待つことが起こらず，上限なしと結果が変わらないからである．
pub fn deadline(timeout_secs: Option<NonZeroU64>) -> Option<Instant> {
    timeout_secs.and_then(|secs| Instant::now().checked_add(Duration::from_secs(secs.get())))
}

pub trait Detector {
    fn effective_config(&self) -> serde_json::Value;
    fn detect(&mut self, call: &PluginCall, scope: &[Vec<u8>])
    -> Result<Vec<RawPair>, PluginError>;
}

pub struct ScopeResult {
    pub keep: Option<BTreeSet<Vec<u8>>>,
    pub rows: Vec<PluginRow>,
}

/// プラグインが表に書かずにツールに渡すのは，版のトランザクションを戻したときにプラグインの表も一緒に戻すためである．
pub struct PluginRow {
    pub table: &'static str,
    pub values: Vec<Cell>,
}

pub trait Classifier {
    fn name(&self) -> &'static str;
    fn effective_config(&self) -> serde_json::Value;
    fn labels(&self) -> &'static [&'static str];
    fn scope(&mut self, call: &PluginCall, tree: &[TreeEntry]) -> Result<ScopeResult, PluginError>;
    /// 版を受け取らないのは，直前の scope の版でラベルを振るからである．
    fn classify(&mut self, pairs: &[Pair]) -> Result<Vec<&'static str>, PluginError>;
}

/// interrupt を受け取るのは，設定を確かめるときに Java や Python を動かすからである．
pub fn detector(
    section: &PluginSection,
    base_dir: &Path,
    interrupt: &Interrupt,
) -> Result<Box<dyn Detector>> {
    let detector: Box<dyn Detector> = match section.name.as_str() {
        "fixture" => Box::new(fixture::FixtureDetector::new(section, base_dir)?),
        "msccd" => Box::new(msccd::Msccd::new(section, base_dir, interrupt)?),
        name => unreachable!("config.rs accepts only the detectors in version.rs, not {name}"),
    };
    Ok(detector)
}

pub fn classifiers(
    sections: &BTreeMap<String, PluginSection>,
    base_dir: &Path,
    interrupt: &Interrupt,
) -> Result<Vec<Box<dyn Classifier>>> {
    sections
        .values()
        .map(|section| {
            let classifier: Box<dyn Classifier> = match section.name.as_str() {
                "claim" => Box::new(claim::Claim::new(section, base_dir, interrupt)?),
                "fixture" => Box::new(fixture::FixtureClassifier::new(section, base_dir)?),
                "generated" => Box::new(generated::Generated::new(section)?),
                "test_path" => Box::new(test_path::TestPath::new(section)?),
                "vendor" => Box::new(vendor::Vendor::new(section)?),
                name => {
                    unreachable!("config.rs accepts only the classifiers in version.rs, not {name}")
                }
            };
            Ok(classifier)
        })
        .collect()
}

/// 節の項目を取り出す．知らない項目を作るときに止めるのは，書き間違えた項目が既定の値で黙って動かないようにするためである．
pub struct Items<'a> {
    label: String,
    plugin: &'a str,
    rest: &'a toml::Table,
}

impl<'a> Items<'a> {
    /// label は，誤りの文で節を指す `[detector]` や `[classifiers.claim]` である
    pub fn new(label: String, section: &'a PluginSection, known: &[&str]) -> Result<Items<'a>> {
        if let Some(unknown) = section
            .rest
            .keys()
            .find(|key| !known.contains(&key.as_str()))
        {
            bail!("{label} unknown item `{unknown}` for {}", section.name);
        }
        Ok(Items {
            label,
            plugin: &section.name,
            rest: &section.rest,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn string(&self, key: &str) -> Result<Option<&'a str>> {
        match self.rest.get(key) {
            None => Ok(None),
            Some(value) => match value.as_str() {
                Some(text) => Ok(Some(text)),
                None => bail!("{} `{key}` of {} must be a string", self.label, self.plugin),
            },
        }
    }

    pub fn required_string(&self, key: &str) -> Result<&'a str> {
        match self.string(key)? {
            Some(text) => Ok(text),
            None => bail!("{} {} needs `{key}`", self.label, self.plugin),
        }
    }

    pub fn integer(&self, key: &str, min: i64, max: i64) -> Result<Option<i64>> {
        match self.rest.get(key) {
            None => Ok(None),
            Some(value) => match value.as_integer() {
                Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
                _ => bail!(
                    "{} `{key}` of {} must be an integer from {min} to {max}",
                    self.label,
                    self.plugin
                ),
            },
        }
    }

    /// 整数も受け付けるのは，TOML では `1` が整数になり，`1.0` と書かせないためである
    pub fn number(&self, key: &str) -> Result<Option<f64>> {
        match self.rest.get(key) {
            None => Ok(None),
            Some(toml::Value::Float(x)) => Ok(Some(*x)),
            Some(toml::Value::Integer(n)) => Ok(Some(*n as f64)),
            Some(_) => bail!("{} `{key}` of {} must be a number", self.label, self.plugin),
        }
    }
}

/// 外部のプログラムを動かさないプラグインに上限を書かせないのは，効かない項目で上限が当たると思わせないためである．
pub fn refuse_timeout(label: &str, section: &PluginSection) -> Result<()> {
    if section.timeout_secs.is_some() {
        bail!(
            "{label} {} does not run external programs, so timeout_secs is not allowed",
            section.name
        );
    }
    Ok(())
}

pub fn classifier_label(section: &PluginSection) -> String {
    format!("[classifiers.{}]", section.name)
}

pub fn describe_end(end: ProgramEnd) -> String {
    match end {
        ProgramEnd::Exited(code) => format!("exited with code {code}"),
        ProgramEnd::Signaled(signal) => format!("was killed by signal {signal}"),
        ProgramEnd::TimedOut => "ran past timeout_secs".to_string(),
    }
}

/// 標準エラーの末尾を足すのは，記録だけを見て，外部のプログラムが失敗した訳を追えるようにするためである．
pub fn program_failure(message: impl Into<String>, output: &ProgramOutput) -> PluginError {
    PluginError::Failed {
        reason: with_stderr(message.into(), output),
    }
}

fn with_stderr(mut message: String, output: &ProgramOutput) -> String {
    let tail = String::from_utf8_lossy(&output.stderr_tail);
    let tail = tail.trim();
    if !tail.is_empty() {
        message.push_str("; stderr: ");
        message.push_str(tail);
    }
    message
}

pub fn check_error(err: PluginError) -> anyhow::Error {
    match err {
        PluginError::Failed { reason } => anyhow::anyhow!(reason),
        PluginError::Abort(err) => err,
    }
}

/// 設定を確かめるときに外部のプログラムを動かす．失敗をやり直さずに止めるのは，記録を始める前で，
/// 動かないことが分かっている設定で全部の版を壊れた版にしないためである．
pub fn run_check(spec: &ProgramSpec, interrupt: &Interrupt) -> Result<ProgramOutput> {
    let output = run_program(spec, interrupt).map_err(check_error)?;
    if output.end != ProgramEnd::Exited(0) {
        bail!(
            "{}",
            with_stderr(
                format!("{} {}", spec.program.display(), describe_end(output.end)),
                &output
            )
        );
    }
    Ok(output)
}

/// Python の str.strip と str.split が区切る空白．Rust の is_whitespace に 4 つを足す．
/// 2026-11-15 に，Python の isspace が真で Rust の is_whitespace が偽の文字が，この 4 つだけであることを確かめた
pub fn is_python_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// 設定を確かめるときに外部のプログラムを動かす空のディレクトリ．
///
/// `<db>.tmp` に作らないのは，設定を確かめる時点ではデータベースを作っておらず，設定の誤りで止まったときに何も残さないためである．
pub struct CheckDir(PathBuf);

impl CheckDir {
    pub fn new() -> Result<CheckDir> {
        let base = std::path::absolute(std::env::temp_dir())
            .context("cannot resolve the temporary directory")?;
        let dir = base.join(format!("cgweave-check-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)
            .with_context(|| format!("cannot create the temporary directory {}", dir.display()))?;
        Ok(CheckDir(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CheckDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub type PluginId = i64;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Failure {
    pub plugin: PluginId,
    pub attempt: u64,
    pub message: String,
}

/// number を引数で受け取って持ち回すのは，1 つの分類のプラグインの scope と classify で，
/// plugin_failure の番号を版とプラグインごとに通しで振るためである．
///
/// attempt の引数を number と別にするのは，2 回目からの checkout を，この呼び出しの中で何回目かで決めるためである．
/// u64 で数えるのは，scope と classify を合わせて retries の 2 倍に 2 を足した回まで呼ばれ，u32 では溢れうるからである
pub fn with_retries<T>(
    retries: u32,
    plugin: PluginId,
    number: &mut u64,
    failures: &mut Vec<Failure>,
    mut attempt: impl FnMut(u64) -> Result<T, PluginError>,
) -> Result<Option<T>> {
    for nth in 1..=u64::from(retries) + 1 {
        let this = *number;
        *number += 1;
        match attempt(nth) {
            Ok(value) => return Ok(Some(value)),
            Err(PluginError::Failed { reason }) => failures.push(Failure {
                plugin,
                attempt: this,
                message: reason,
            }),
            Err(PluginError::Abort(err)) => return Err(err),
        }
    }
    Ok(None)
}

/// 呼ぶたびに空のディレクトリを渡すのは，作業ツリーの外に一時ファイルを置かせ，前の呼び出しの残りを見せないためである．
pub struct TempDirs {
    root: PathBuf,
    next: u64,
}

impl TempDirs {
    pub fn new(db: &Path, run_id: &str) -> Result<TempDirs> {
        let root = std::path::absolute(crate::store::side_path(db, ".tmp")).with_context(|| {
            format!("cannot resolve the temporary directory of {}", db.display())
        })?;
        Ok(TempDirs {
            root: root.join(run_id),
            next: 1,
        })
    }

    pub fn create(&mut self) -> Result<PathBuf> {
        let dir = self.root.join(self.next.to_string());
        self.next += 1;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create the temporary directory {}", dir.display()))?;
        Ok(dir)
    }

    pub fn remove(&self, dir: &Path) -> Result<()> {
        std::fs::remove_dir_all(dir)
            .with_context(|| format!("cannot delete the temporary directory {}", dir.display()))
    }

    /// 実行の UUID の名前のディレクトリだけを消すのは，同じ場所にある利用者のファイルを消さないためである．
    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_dir_all(&self.root) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(err).with_context(|| format!("cannot delete {}", self.root.display()))
            }
            _ => Ok(()),
        }
    }

    /// 親の `<db>.tmp` を空のときだけ消すのは，ほかの実行のディレクトリを残すためである
    pub fn finish(&self) -> Result<()> {
        self.clear()?;
        if let Some(parent) = self.root.parent()
            && std::fs::read_dir(parent).is_ok_and(|mut entries| entries.next().is_none())
        {
            std::fs::remove_dir(parent)
                .with_context(|| format!("cannot delete {}", parent.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn failed(reason: &str) -> PluginError {
        PluginError::Failed {
            reason: reason.to_string(),
        }
    }

    #[test]
    fn the_first_success_ends_the_retries() {
        let (mut number, mut failures) = (1, Vec::new());
        let got = with_retries(2, 3, &mut number, &mut failures, |nth| {
            Ok::<_, PluginError>(nth)
        })
        .unwrap();
        assert_eq!(got, Some(1));
        assert_eq!(number, 2);
        assert!(failures.is_empty());
    }

    #[test]
    fn failures_are_recorded_with_running_numbers_until_a_success() {
        let (mut number, mut failures) = (1, Vec::new());
        let got = with_retries(2, 3, &mut number, &mut failures, |nth| {
            if nth < 3 {
                Err(failed(&format!("try {nth}")))
            } else {
                Ok(nth)
            }
        })
        .unwrap();
        assert_eq!(got, Some(3));
        assert_eq!(
            failures,
            [
                Failure {
                    plugin: 3,
                    attempt: 1,
                    message: "try 1".to_string()
                },
                Failure {
                    plugin: 3,
                    attempt: 2,
                    message: "try 2".to_string()
                },
            ]
        );
        // 同じプラグインの次の呼び出しは，番号を続け，attempt の引数は 1 から始める
        let mut seen = Vec::new();
        let got = with_retries(2, 3, &mut number, &mut failures, |nth| {
            seen.push(nth);
            if nth == 1 {
                Err(failed("classify: once"))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(got, Some(()));
        assert_eq!(seen, [1, 2]);
        assert_eq!(failures.last().unwrap().attempt, 4);
        assert_eq!(number, 6);
    }

    #[test]
    fn using_up_the_retries_gives_none() {
        let (mut number, mut failures) = (1, Vec::new());
        let got = with_retries(2, 1, &mut number, &mut failures, |_| {
            Err::<(), _>(failed("no"))
        })
        .unwrap();
        assert_eq!(got, None);
        assert_eq!(failures.len(), 3);
        let (mut number, mut failures) = (1, Vec::new());
        let got = with_retries(0, 1, &mut number, &mut failures, |_| {
            Err::<(), _>(failed("no"))
        })
        .unwrap();
        assert_eq!(got, None);
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn an_error_that_stops_is_returned_without_retrying() {
        let (mut number, mut failures) = (1, Vec::new());
        let mut calls = 0;
        let err = with_retries(5, 1, &mut number, &mut failures, |_| {
            calls += 1;
            Err::<(), _>(PluginError::Abort(anyhow!("adapter bug")))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "adapter bug");
        assert_eq!(calls, 1);
        assert!(failures.is_empty());
    }

    #[test]
    fn temporary_directories_are_numbered_and_the_parent_goes_when_empty() {
        let base = std::env::temp_dir().join(format!("cgweave-tmpdirs-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let db = base.join("e.db");
        let mut dirs = TempDirs::new(&db, "run-1").unwrap();
        let first = dirs.create().unwrap();
        let second = dirs.create().unwrap();
        assert_eq!(first, base.join("e.db.tmp/run-1/1"));
        assert_eq!(second, base.join("e.db.tmp/run-1/2"));
        std::fs::write(first.join("x"), "x").unwrap();
        dirs.remove(&first).unwrap();
        assert!(!first.exists());
        // ほかの実行のディレクトリがあれば，<db>.tmp は残す
        std::fs::create_dir_all(base.join("e.db.tmp/run-0")).unwrap();
        dirs.finish().unwrap();
        assert!(!base.join("e.db.tmp/run-1").exists());
        assert!(base.join("e.db.tmp/run-0").exists());
        TempDirs::new(&db, "run-0").unwrap().finish().unwrap();
        assert!(!base.join("e.db.tmp").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
