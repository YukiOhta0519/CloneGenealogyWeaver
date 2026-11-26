// ファイルを読み直さないのは，検査した中身と run.config に書く中身を同じものにするためである．
// プラグインごとの項目を検査しないのは，プラグインを作るときにプラグインが検査するからである．

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::version::{CLASSIFIERS, DETECTORS};

pub struct Config {
    pub raw: String,
    pub base_dir: PathBuf,
    pub retries: u32,
    pub line_cache_mib: NonZeroU64,
    pub detector: PluginSection,
    /// HashMap にしないのは，表を作る順とラベルの列の順を，節の名前の順に決めるためである．
    pub classifiers: BTreeMap<String, PluginSection>,
}

pub struct PluginSection {
    pub name: String,
    pub timeout_secs: Option<NonZeroU64>,
    /// ここで検査しないのは，項目がプラグインごとに違い，プラグインが自分で検査するからである．
    pub rest: toml::Table,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default = "default_retries")]
    retries: u32,
    #[serde(default = "default_line_cache_mib")]
    line_cache_mib: NonZeroU64,
    detector: DetectorSection,
    #[serde(default)]
    classifiers: BTreeMap<String, ClassifierSection>,
}

#[derive(Deserialize)]
struct DetectorSection {
    name: String,
    timeout_secs: Option<NonZeroU64>,
    #[serde(flatten)]
    rest: toml::Table,
}

#[derive(Deserialize)]
struct ClassifierSection {
    timeout_secs: Option<NonZeroU64>,
    #[serde(flatten)]
    rest: toml::Table,
}

fn default_retries() -> u32 {
    2
}

fn default_line_cache_mib() -> NonZeroU64 {
    NonZeroU64::new(1024).expect("1024 is not zero")
}

pub fn load(path: &Path) -> Result<Config> {
    let bytes =
        std::fs::read(path).with_context(|| format!("cannot read config {}", path.display()))?;
    let raw = String::from_utf8(bytes)
        .map_err(|_| anyhow!("invalid config {}: the file is not UTF-8", path.display()))?;
    let base_dir = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve config path {}", path.display()))?
        .parent()
        .expect("a canonical file path has a parent")
        .to_path_buf();
    parse(path, raw, base_dir)
}

/// load に含めないのは，ファイルを読まずにテストで試せるようにするためである．
fn parse(path: &Path, raw: String, base_dir: PathBuf) -> Result<Config> {
    let file: ConfigFile = toml::from_str(&raw).map_err(|err| toml_error(path, &raw, &err))?;

    if !DETECTORS.contains(&file.detector.name.as_str()) {
        bail!(
            "invalid config {}: unknown detector `{}`; known detectors are {}",
            path.display(),
            file.detector.name,
            DETECTORS.join(", ")
        );
    }
    for name in file.classifiers.keys() {
        if !CLASSIFIERS.contains(&name.as_str()) {
            bail!(
                "invalid config {}: unknown classifier `[classifiers.{name}]`; known classifiers are {}",
                path.display(),
                CLASSIFIERS.join(", ")
            );
        }
    }

    let detector = PluginSection {
        name: file.detector.name,
        timeout_secs: file.detector.timeout_secs,
        rest: file.detector.rest,
    };
    let classifiers = file
        .classifiers
        .into_iter()
        .map(|(name, section)| {
            let plugin = PluginSection {
                name: name.clone(),
                timeout_secs: section.timeout_secs,
                rest: section.rest,
            };
            (name, plugin)
        })
        .collect();

    Ok(Config {
        raw,
        base_dir,
        retries: file.retries,
        line_cache_mib: file.line_cache_mib,
        detector,
        classifiers,
    })
}

/// `~` を展開しないのは，シェルを通さずに渡したパスと同じに扱うためである．
pub fn resolve_path(base_dir: &Path, value: &str) -> PathBuf {
    base_dir.join(value)
}

/// 名前だけなら PATH から探すのは，シェルでその名前を打ったときと同じ実行ファイルを使うためである．
///
/// リンクをたどった先に直さないのは，仮想環境の python が，呼ばれたパスから仮想環境を見つけるからである
pub fn resolve_program(base_dir: &Path, value: &str) -> Result<PathBuf> {
    if value.is_empty() {
        bail!("the program name is empty");
    }
    let path = if value.contains('/') {
        resolve_path(base_dir, value)
    } else {
        let found = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(value))
                .find(|candidate| is_executable(candidate))
        });
        match found {
            Some(path) => path,
            None => bail!("cannot find `{value}` in PATH"),
        }
    };
    if !is_executable(&path) {
        bail!("{} is not an executable file", path.display());
    }
    std::path::absolute(&path).with_context(|| format!("cannot resolve {}", path.display()))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn toml_error(path: &Path, raw: &str, err: &toml::de::Error) -> anyhow::Error {
    // toml の文の改行を残さないのは，誤りを 1 行で出すためである．
    let message = err
        .message()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    match err.span() {
        Some(span) => {
            let (line, column) = line_and_column(raw, span.start);
            anyhow!(
                "invalid config {} at line {line} column {column}: {message}",
                path.display()
            )
        }
        None => anyhow!("invalid config {}: {message}", path.display()),
    }
}

fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(raw: &str) -> Result<Config> {
        parse(Path::new("c.toml"), raw.to_string(), PathBuf::from("/base"))
    }

    fn error_of(raw: &str) -> String {
        match check(raw) {
            Ok(_) => panic!("accepted: {raw}"),
            Err(err) => format!("{err:#}"),
        }
    }

    #[test]
    fn defaults_fill_the_tool_items() {
        let config = check("[detector]\nname = \"msccd\"\nmsccd_dir = \"/x\"\n").unwrap();
        assert_eq!(config.retries, 2);
        assert_eq!(config.line_cache_mib.get(), 1024);
        assert_eq!(config.detector.name, "msccd");
        assert!(config.detector.timeout_secs.is_none());
        assert_eq!(config.detector.rest["msccd_dir"].as_str(), Some("/x"));
        assert!(config.classifiers.is_empty());
        assert_eq!(config.base_dir, PathBuf::from("/base"));
    }

    #[test]
    fn classifiers_are_named_by_their_sections_in_name_order() {
        let config = check(
            "[detector]\nname = \"fixture\"\nfile = \"p.json\"\n\
             [classifiers.vendor]\n\
             [classifiers.claim]\ntimeout_secs = 600\nclaim_name = \"a.b\"\n",
        )
        .unwrap();
        let names: Vec<_> = config.classifiers.keys().cloned().collect();
        assert_eq!(names, ["claim", "vendor"]);
        let claim = &config.classifiers["claim"];
        assert_eq!(claim.name, "claim");
        assert_eq!(claim.timeout_secs.map(NonZeroU64::get), Some(600));
        assert_eq!(claim.rest["claim_name"].as_str(), Some("a.b"));
        assert!(!claim.rest.contains_key("timeout_secs"));
    }

    #[test]
    fn integer_items_reject_bad_values() {
        for item in [
            "retries = -1",
            "retries = 1.5",
            "retries = 2.0",
            "retries = \"2\"",
            "line_cache_mib = 0",
            "line_cache_mib = 0.5",
        ] {
            let raw = format!("{item}\n[detector]\nname = \"msccd\"\n");
            error_of(&raw);
        }
        for value in ["0", "-1", "1.5", "2.0"] {
            error_of(&format!(
                "[detector]\nname = \"msccd\"\ntimeout_secs = {value}\n"
            ));
            error_of(&format!(
                "[detector]\nname = \"msccd\"\n[classifiers.claim]\ntimeout_secs = {value}\n"
            ));
        }
    }

    #[test]
    fn structure_errors_are_rejected() {
        assert!(error_of("retry = 2\n[detector]\nname = \"msccd\"\n").contains("retry"));
        error_of("retries = 1\nretries = 2\n[detector]\nname = \"msccd\"\n");
        assert!(error_of("retries = 1\n").contains("detector"));
        assert!(error_of("[detector]\nmsccd_dir = \"/x\"\n").contains("name"));
        error_of("[detector]\nname = \"msccd\"\n[classifiers]\nclaim = 1\n");
        // 2026-11-12 より前の形の節は，知らない項目として拒む
        assert!(
            error_of("[detector]\nname = \"msccd\"\n[classifier]\nname = \"claim\"\n")
                .contains("classifier")
        );
    }

    #[test]
    fn unknown_plugin_names_are_rejected() {
        assert!(
            error_of("[detector]\nname = \"ccfinder\"\n").contains("unknown detector `ccfinder`")
        );
        assert!(
            error_of("[detector]\nname = \"msccd\"\n[classifiers.service]\n")
                .contains("unknown classifier `[classifiers.service]`")
        );
    }

    #[test]
    fn toml_errors_show_line_and_column_in_one_line() {
        let message = error_of("retries = 2\n[detector]\nname = msccd\n");
        assert!(
            message.starts_with("invalid config c.toml at line 3 column 8: "),
            "{message}"
        );
        assert!(!message.contains('\n'), "{message}");
    }

    #[test]
    fn programs_are_found_in_path_or_beside_the_config() {
        let sh = resolve_program(Path::new("/nowhere"), "sh").unwrap();
        assert!(sh.is_absolute() && sh.ends_with("sh"), "{}", sh.display());
        assert_eq!(
            resolve_program(Path::new("/"), "bin/sh").unwrap(),
            PathBuf::from("/bin/sh")
        );
        assert_eq!(
            resolve_program(Path::new("/base"), "/bin/sh").unwrap(),
            PathBuf::from("/bin/sh")
        );
        let message = format!(
            "{:#}",
            resolve_program(Path::new("/"), "cgweave-no-such-program").unwrap_err()
        );
        assert!(
            message.contains("cannot find `cgweave-no-such-program` in PATH"),
            "{message}"
        );
        let message = format!(
            "{:#}",
            resolve_program(Path::new("/"), "etc/hostname").unwrap_err()
        );
        assert!(
            message.contains("/etc/hostname is not an executable file"),
            "{message}"
        );
        assert!(resolve_program(Path::new("/"), "").is_err());
    }

    #[test]
    fn line_and_column_count_characters() {
        assert_eq!(line_and_column("ab\ncdé\nf", 0), (1, 1));
        assert_eq!(line_and_column("ab\ncdé\nf", 3), (2, 1));
        assert_eq!(line_and_column("ab\ncdé\nf", 8), (3, 1));
    }
}
