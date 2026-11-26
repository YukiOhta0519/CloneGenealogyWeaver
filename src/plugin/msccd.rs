use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config::{PluginSection, resolve_path, resolve_program};
use crate::core::model::{RawFragment, RawPair};
use crate::error::PluginError;
use crate::plugin::{
    CheckDir, Detector, Items, PluginCall, deadline, describe_end, program_failure, run_check,
};
use crate::process::{Interrupt, ProgramEnd, ProgramSpec, run_program};

const TOKENIZER_JAR: &str = "tokenizers/Java/Java_tokenizer.jar";
const ANTLR_JAR: &str = "lib/antlr-4.8-modified.jar";
const DETECTOR_JAR: &str = "modules/msccd_detector.jar";
const DEFAULT_KEYWORDS: &str = "grammarDefinations/java/Java.reserved";

/// 標準出力も探すのは，keywordsList が読めないことは標準出力にだけ出るからである．
const WATCH: [&str; 2] = ["java.lang.OutOfMemoryError", "Keywords filter unavaliable"];

const ITEMS: &[&str] = &[
    "detectionThreshold",
    "heap_detector",
    "heap_tokenizer",
    "java",
    "keywordsList",
    "languageExtensionName",
    "maxRound",
    "minTokens",
    "minTokensForBagGeneration",
    "msccd_dir",
    "threadNum_detection",
    "threadNum_tokenizer",
    "tokenizer",
];

/// i64 の最大にしないのは，2 つの jar が整数の引数を Integer.valueOf で読み，int を超えると全部の版で失敗するからである．
/// 2026-11-15 に javap で確かめた
const JAVA_INT_MAX: i64 = i32::MAX as i64;

pub struct Msccd {
    java: PathBuf,
    java_version: String,
    msccd_dir: PathBuf,
    keywords: PathBuf,
    sha256: BTreeMap<String, String>,
    heap_tokenizer: String,
    heap_detector: String,
    min_tokens: i64,
    min_tokens_for_bags: i64,
    threshold: f64,
    max_round: i64,
    extension: String,
    threads_tokenizer: i64,
    threads_detection: i64,
}

impl Msccd {
    pub fn new(section: &PluginSection, base_dir: &Path, interrupt: &Interrupt) -> Result<Msccd> {
        let items = Items::new("[detector]".to_string(), section, ITEMS)?;
        let label = items.label();

        let dir = resolve_path(base_dir, items.required_string("msccd_dir")?);
        let msccd_dir = std::fs::canonicalize(&dir)
            .with_context(|| format!("{label} cannot resolve msccd_dir {}", dir.display()))?;
        // 設定で止めるのは，-cp がパスを `:` で区切り，`:` のあるパスを渡せないからである
        if msccd_dir.as_os_str().as_bytes().contains(&b':') {
            bail!(
                "{label} msccd_dir {} must not contain `:`",
                msccd_dir.display()
            );
        }
        let mut sha256 = BTreeMap::new();
        for jar in [TOKENIZER_JAR, ANTLR_JAR, DETECTOR_JAR] {
            let path = msccd_dir.join(jar);
            let bytes = std::fs::read(&path)
                .with_context(|| format!("{label} cannot read {} of MSCCD", path.display()))?;
            sha256.insert(path.to_string_lossy().into_owned(), hex_sha256(&bytes));
        }

        // 既定を msccd_dir から読むのは，既定のファイルが MSCCD のリポジトリの中にあるからである．
        let keywords = match items.string("keywordsList")? {
            Some(value) => resolve_path(base_dir, value),
            None => msccd_dir.join(DEFAULT_KEYWORDS),
        };
        let keywords = std::fs::canonicalize(&keywords).with_context(|| {
            format!("{label} cannot resolve keywordsList {}", keywords.display())
        })?;
        // 読めないか空のファイルで止めるのは，tokenizer が止まらずに絞り込みを外し，結果が静かに変わるからである．
        let bytes = std::fs::read(&keywords)
            .with_context(|| format!("{label} cannot read keywordsList {}", keywords.display()))?;
        if bytes.is_empty() {
            bail!("{label} keywordsList {} is empty", keywords.display());
        }
        sha256.insert(keywords.to_string_lossy().into_owned(), hex_sha256(&bytes));

        let heap = |key: &str, default: &str| -> Result<String> {
            let value = items.string(key)?.unwrap_or(default);
            if !is_heap(value) {
                bail!(
                    "{label} `{key}` of msccd must be a size like `4800m` or `8g`, not {value:?}"
                );
            }
            Ok(value.to_string())
        };
        let heap_tokenizer = heap("heap_tokenizer", "4800m")?;
        let heap_detector = heap("heap_detector", "8g")?;
        let integer = |key: &str, min: i64, default: i64| -> Result<i64> {
            Ok(items.integer(key, min, JAVA_INT_MAX)?.unwrap_or(default))
        };
        let min_tokens = integer("minTokens", 0, 20)?;
        let min_tokens_for_bags = integer("minTokensForBagGeneration", 0, 20)?;
        let max_round = integer("maxRound", 0, 4)?;
        let threads_tokenizer = integer("threadNum_tokenizer", 1, 8)?;
        let threads_detection = integer("threadNum_detection", 1, 8)?;
        let threshold = items.number("detectionThreshold")?.unwrap_or(0.7);
        if !(threshold > 0.0 && threshold <= 1.0) {
            bail!("{label} `detectionThreshold` of msccd must be greater than 0 and at most 1");
        }
        let extension = items.string("languageExtensionName")?.unwrap_or(".java");
        // 設定で止めるのは，リンクの名前に拡張子を付け，fileList.txt にカンマと改行で区切って書くからである．
        if extension.is_empty() || extension.contains(['/', ',', '\n', '\r', '\0']) {
            bail!(
                "{label} `languageExtensionName` of msccd must be a nonempty name part \
                 without `/`, `,`, or line breaks, not {extension:?}"
            );
        }
        // Java だけにするのは，ほかの言語の tokenizer の jar の名前と呼び方を確かめていないからである．
        let tokenizer = items.string("tokenizer")?.unwrap_or("Java");
        if tokenizer != "Java" {
            bail!("{label} `tokenizer` of msccd supports only \"Java\", not {tokenizer:?}");
        }

        let java = resolve_program(base_dir, items.string("java")?.unwrap_or("java"))
            .with_context(|| format!("{label} `java` of msccd"))?;
        let java_version = read_java_version(&java, section, interrupt)
            .with_context(|| format!("{label} cannot use {}", java.display()))?;

        Ok(Msccd {
            java,
            java_version,
            msccd_dir,
            keywords,
            sha256,
            heap_tokenizer,
            heap_detector,
            min_tokens,
            min_tokens_for_bags,
            threshold,
            max_round,
            extension: extension.to_string(),
            threads_tokenizer,
            threads_detection,
        })
    }

    /// 終了コードと探す文字列の両方を見るのは，tokenizer が OOM でも，keywordsList が読めなくても，0 で終わりうるからである．
    fn run(&self, call: &PluginCall, jar: &str, args: &[OsString]) -> Result<(), PluginError> {
        let output = run_program(
            &ProgramSpec {
                program: &self.java,
                args,
                temp_dir: call.temp_dir,
                deadline: call.deadline,
                capture_stdout: false,
                watch: &WATCH,
            },
            call.interrupt,
        )?;
        if output.end != ProgramEnd::Exited(0) {
            return Err(program_failure(
                format!("the {jar} of MSCCD {}", describe_end(output.end)),
                &output,
            ));
        }
        if let Some((_, text)) = output.watched.iter().zip(WATCH).find(|(found, _)| **found) {
            return Err(program_failure(
                format!("the {jar} of MSCCD printed {text:?}"),
                &output,
            ));
        }
        Ok(())
    }
}

fn read_java_version(
    java: &Path,
    section: &PluginSection,
    interrupt: &Interrupt,
) -> Result<String> {
    let dir = CheckDir::new()?;
    let output = run_check(
        &ProgramSpec {
            program: java,
            args: &[OsString::from("-version")],
            temp_dir: dir.path(),
            deadline: deadline(section.timeout_secs),
            capture_stdout: true,
            watch: &[],
        },
        interrupt,
    )?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr_tail),
        String::from_utf8_lossy(&output.stdout)
    );
    let text = text.trim().to_string();
    let Some(major) = java_major(&text) else {
        bail!("cannot read the version from java -version: {text:?}");
    };
    // 設定で止めるのは，tokenizer の jar が Java 17 では起動せず，全部の版が壊れた版になるからである．
    if major < 18 {
        bail!("the tokenizer of MSCCD needs Java 18 or later, but java -version says {text:?}");
    }
    Ok(text)
}

/// `1.8.0_292` のような古い書き方も読むのは，古い Java を版の読めない誤りでなく 18 より前として止めるためである
fn java_major(text: &str) -> Option<u32> {
    let after = text.split_once("version \"")?.1;
    let version = &after[..after.find('"')?];
    let mut parts = version.split(|c: char| !c.is_ascii_digit());
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

fn is_heap(value: &str) -> bool {
    value.strip_suffix(['m', 'g']).is_some_and(|digits| {
        !digits.is_empty() && !digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit())
    })
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Detector for Msccd {
    fn effective_config(&self) -> serde_json::Value {
        serde_json::json!({
            "detectionThreshold": self.threshold,
            "heap_detector": self.heap_detector,
            "heap_tokenizer": self.heap_tokenizer,
            "java": self.java.to_string_lossy(),
            "java_version": self.java_version,
            "keywordsList": self.keywords.to_string_lossy(),
            "languageExtensionName": self.extension,
            "maxRound": self.max_round,
            "minTokens": self.min_tokens,
            "minTokensForBagGeneration": self.min_tokens_for_bags,
            "msccd_dir": self.msccd_dir.to_string_lossy(),
            "sha256": self.sha256,
            "threadNum_detection": self.threads_detection,
            "threadNum_tokenizer": self.threads_tokenizer,
            "tokenizer": "Java",
        })
    }

    fn detect(
        &mut self,
        call: &PluginCall,
        scope: &[Vec<u8>],
    ) -> Result<Vec<RawPair>, PluginError> {
        let files: Vec<&[u8]> = scope
            .iter()
            .map(Vec::as_slice)
            .filter(|path| path.ends_with(self.extension.as_bytes()))
            .collect();
        if files.is_empty() {
            return Ok(Vec::new());
        }
        // 通し番号の名前のリンクにするのは，fileList.txt がカンマと改行で区切り，パスのカンマを書けないからである．
        let mut list = String::new();
        for (i, path) in files.iter().enumerate() {
            let name = format!("{:06}{}", i + 1, self.extension);
            let target = call.work_tree.join(OsStr::from_bytes(path));
            std::os::unix::fs::symlink(&target, call.temp_dir.join(&name)).map_err(failed(
                &format!("cannot link {name} to {}", target.display()),
            ))?;
            list.push_str(&format!("0,{name}\n"));
        }
        write(call.temp_dir, "fileList.txt", list.as_bytes())?;

        let dir = &self.msccd_dir;
        let mut classpath = dir.join(TOKENIZER_JAR).into_os_string();
        classpath.push(":");
        classpath.push(dir.join(ANTLR_JAR));
        let tokenizer_args: Vec<OsString> = vec![
            format!("-Xmx{}", self.heap_tokenizer).into(),
            "-XX:+ExitOnOutOfMemoryError".into(),
            "-cp".into(),
            classpath,
            "org.nagoya_u.ertl.sa.App".into(),
            "1".into(),
            self.min_tokens_for_bags.to_string().into(),
            "fileList.txt".into(),
            self.keywords.clone().into(),
            "tokenBags".into(),
            self.threads_tokenizer.to_string().into(),
        ];
        self.run(call, "tokenizer", &tokenizer_args)?;
        let bags = read(call.temp_dir, "tokenBags", "the tokenizer")?;
        let (sorted, bags) = sort_bags(&bags, files.len()).map_err(bad_output("tokenBags"))?;
        // 並べ替えるのは，tokenizer がスレッドの終わった順に書き，そのままではスレッド数と実行ごとにペアが変わるからである．
        write(call.temp_dir, "tokenBags", &sorted)?;

        let detector_args: Vec<OsString> = vec![
            format!("-Xmx{}", self.heap_detector).into(),
            "-XX:+ExitOnOutOfMemoryError".into(),
            "-jar".into(),
            dir.join(DETECTOR_JAR).into(),
            ".".into(),
            "tokenBags".into(),
            self.min_tokens.to_string().into(),
            self.threshold.to_string().into(),
            self.threads_detection.to_string().into(),
            self.max_round.to_string().into(),
        ];
        self.run(call, "detector", &detector_args)?;
        let pairs = read(call.temp_dir, "pairs.file", "the detector")?;
        let pairs = read_pairs(&pairs, &bags).map_err(bad_output("pairs.file"))?;

        let mut counts: HashMap<usize, LineCount> = HashMap::new();
        let mut raw = Vec::new();
        let mut dropped = 0;
        for (x, y) in pairs {
            let mut fragment = |bag: Bag| -> Result<RawFragment, PluginError> {
                let count = match counts.get(&bag.file) {
                    Some(count) => *count,
                    None => {
                        let path = call.work_tree.join(OsStr::from_bytes(files[bag.file]));
                        let bytes = std::fs::read(&path)
                            .map_err(failed(&format!("cannot read {}", path.display())))?;
                        *counts.entry(bag.file).or_insert(LineCount::of(&bytes))
                    }
                };
                Ok(RawFragment {
                    path: files[bag.file].to_vec(),
                    start: bag.start,
                    end: count.fix_end(bag.end),
                })
            };
            let (a, b) = (fragment(x)?, fragment(y)?);
            // 落とすのは，1 行に収まった同じ粒度の兄弟の bag から出て，ツールが同じ片どうしのペアとして止めるからである．
            if a == b {
                dropped += 1;
                continue;
            }
            raw.push(RawPair {
                a,
                b,
                similarity: None,
            });
        }
        if dropped > 0 {
            log::debug!(
                "MSCCD paired a fragment with itself {dropped} times in version {}; dropped them",
                call.commit
            );
        }
        Ok(raw)
    }
}

fn failed(message: &str) -> impl Fn(std::io::Error) -> PluginError + '_ {
    move |err| PluginError::Failed {
        reason: format!("{message}: {err}"),
    }
}

/// 形が読めないことを止める誤りにしないのは，外部のプログラムの出力で，ディスクがいっぱいのような一時的な理由でも起きうるからである．
fn bad_output(file: &'static str) -> impl Fn(String) -> PluginError {
    move |problem| PluginError::Failed {
        reason: format!("cannot read {file} of MSCCD: {problem}"),
    }
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), PluginError> {
    let path = dir.join(name);
    std::fs::write(&path, bytes).map_err(failed(&format!("cannot write {}", path.display())))
}

/// 無いことを失敗にするのは，detector が tokenBags の無いときも 0 で終わるからである．
fn read(dir: &Path, name: &str, writer: &str) -> Result<Vec<u8>, PluginError> {
    let path = dir.join(name);
    std::fs::read(&path).map_err(|err| PluginError::Failed {
        reason: if err.kind() == std::io::ErrorKind::NotFound {
            format!("{writer} of MSCCD wrote no {name}")
        } else {
            format!("cannot read {}: {err}", path.display())
        },
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Bag {
    file: usize,
    start: i64,
    end: i64,
}

type BagKey = (usize, u64);

const BAG_SEPARATOR: &[u8] = b"@ @";
const RANGE_SEPARATOR: &[u8] = b": :";
const PROJECT_FIELD: usize = 0;
const FILE_FIELD: usize = 1;
const BAG_FIELD: usize = 2;
const RANGE_FIELD: usize = 7;

fn sort_bags(bytes: &[u8], files: usize) -> Result<(Vec<u8>, HashMap<BagKey, Bag>), String> {
    let mut rows: Vec<(BagKey, &[u8])> = Vec::new();
    let mut bags = HashMap::new();
    for line in bytes.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        let fields = first_fields(line, BAG_SEPARATOR, RANGE_FIELD + 1);
        let bad = || {
            format!(
                "unexpected line {:?}",
                String::from_utf8_lossy(&line[..line.len().min(200)])
            )
        };
        if fields.len() <= RANGE_FIELD || number::<u64>(fields[PROJECT_FIELD]) != Some(0) {
            return Err(bad());
        }
        let (Some(file), Some(bag)) = (
            number::<usize>(fields[FILE_FIELD]),
            number::<u64>(fields[BAG_FIELD]),
        ) else {
            return Err(bad());
        };
        let range = first_fields(fields[RANGE_FIELD], RANGE_SEPARATOR, 3);
        let (Some(start), Some(end)) = (
            range.first().and_then(|x| number::<i64>(x)),
            range.get(1).and_then(|x| number::<i64>(x)),
        ) else {
            return Err(bad());
        };
        if range.len() != 2 || file >= files {
            return Err(bad());
        }
        if bags.insert((file, bag), Bag { file, start, end }).is_some() {
            return Err(format!("bag {bag} of file {file} appears twice"));
        }
        rows.push(((file, bag), line));
    }
    rows.sort_by_key(|(key, _)| *key);
    let mut sorted = Vec::with_capacity(bytes.len() + 1);
    for (_, line) in rows {
        sorted.extend_from_slice(line);
        sorted.push(b'\n');
    }
    Ok((sorted, bags))
}

/// 行の全部を分けないのは，9 番目からがトークンの一覧で，長い行では数千の項目になるからである
fn first_fields<'a>(line: &'a [u8], separator: &[u8], limit: usize) -> Vec<&'a [u8]> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while fields.len() + 1 < limit && i + separator.len() <= line.len() {
        if &line[i..i + separator.len()] == separator {
            fields.push(&line[start..i]);
            i += separator.len();
            start = i;
        } else {
            i += 1;
        }
    }
    let rest = &line[start..];
    let end = (0..=rest.len().saturating_sub(separator.len()))
        .find(|&j| rest[j..].starts_with(separator))
        .unwrap_or(rest.len());
    fields.push(&rest[..end]);
    fields
}

fn number<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn read_pairs(bytes: &[u8], bags: &HashMap<BagKey, Bag>) -> Result<Vec<(Bag, Bag)>, String> {
    let mut pairs = Vec::new();
    for line in bytes
        .split(|&b| b == b'\n')
        .filter(|line| !line.trim_ascii().is_empty())
    {
        let bad = || format!("unexpected line {:?}", String::from_utf8_lossy(line));
        let [x, y]: [[u64; 3]; 2] = serde_json::from_slice(line).map_err(|_| bad())?;
        let bag = |[project, file, bag]: [u64; 3]| {
            let file = usize::try_from(file).map_err(|_| bad())?;
            match bags.get(&(file, bag)) {
                Some(found) if project == 0 => Ok(*found),
                _ => Err(format!(
                    "a pair has bag {bag} of file {file}, which is not in tokenBags"
                )),
            }
        };
        pairs.push((bag(x)?, bag(y)?));
    }
    Ok(pairs)
}

#[derive(Clone, Copy, Debug)]
struct LineCount {
    lf: i64,
    ends_with_lf: bool,
}

impl LineCount {
    fn of(bytes: &[u8]) -> LineCount {
        LineCount {
            lf: bytes.iter().filter(|&&b| b == b'\n').count() as i64,
            ends_with_lf: bytes.last() == Some(&b'\n'),
        }
    }

    /// LF で終わるファイルの行数は LF の数なので，終了行がそれより 1 大きいときだけ直す．
    /// ほかの食い違いを直さないのは，観測していない形を，ツールの確かめで止めて知らせるためである．
    fn fix_end(self, end: i64) -> i64 {
        if self.ends_with_lf && end == self.lf + 1 {
            self.lf
        } else {
            end
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_versions_are_read_in_both_styles() {
        let text = "openjdk version \"21.0.13.0.3\" 2026-10-24\nOpenJDK Runtime Environment";
        assert_eq!(java_major(text), Some(21));
        assert_eq!(
            java_major("openjdk version \"17.0.20\" 2026-07-15"),
            Some(17)
        );
        assert_eq!(java_major("openjdk version \"18-ea\" 2022-03-22"), Some(18));
        assert_eq!(java_major("java version \"1.8.0_292\""), Some(8));
        assert_eq!(java_major("openjdk version \"25\" 2025-09-16"), Some(25));
        assert_eq!(java_major("no version here"), None);
        assert_eq!(java_major("version \"x\""), None);
    }

    #[test]
    fn heaps_are_megabytes_or_gigabytes() {
        for value in ["4800m", "8g", "1m"] {
            assert!(is_heap(value), "{value}");
        }
        for value in ["", "m", "8", "8G", "8k", "08g", "-1g", "1.5g", "8gb"] {
            assert!(!is_heap(value), "{value}");
        }
    }

    fn bag_line(file: usize, bag: u64, range: &str) -> String {
        format!("0@ @{file}@ @{bag}@ @0@ @4@ @25@ @7@ @{range}@ @a: :1_ _b@ @c: :2_ _")
    }

    #[test]
    fn bags_are_sorted_by_file_and_bag_number() {
        let lines = [
            bag_line(1, 10, "3: :9"),
            bag_line(0, 2, "1: :5"),
            bag_line(1, 9, "2: :4"),
            bag_line(0, 11, "6: :8"),
        ];
        let input = format!("{}\n", lines.join("\n"));
        let (sorted, bags) = sort_bags(input.as_bytes(), 2).unwrap();
        let expected = format!("{}\n{}\n{}\n{}\n", lines[1], lines[3], lines[2], lines[0]);
        assert_eq!(String::from_utf8(sorted).unwrap(), expected);
        assert_eq!(
            bags[&(1, 10)],
            Bag {
                file: 1,
                start: 3,
                end: 9
            }
        );
        assert_eq!(bags.len(), 4);
        let (sorted, bags) = sort_bags(b"", 2).unwrap();
        assert!(sorted.is_empty() && bags.is_empty());
    }

    #[test]
    fn broken_bags_are_refused() {
        for line in [
            "0@ @0@ @1".to_string(),
            bag_line(2, 0, "1: :2"),
            bag_line(0, 0, "1-2"),
            bag_line(0, 0, "1: :2: :3"),
            bag_line(0, 0, "x: :2"),
            bag_line(0, 0, "1: :2").replacen('0', "1", 1),
        ] {
            assert!(sort_bags(line.as_bytes(), 2).is_err(), "{line}");
        }
        let twice = format!("{}\n{}\n", bag_line(0, 1, "1: :2"), bag_line(0, 1, "3: :4"));
        assert_eq!(
            sort_bags(twice.as_bytes(), 1).unwrap_err(),
            "bag 1 of file 0 appears twice"
        );
    }

    #[test]
    fn fields_stop_at_the_limit() {
        assert_eq!(first_fields(b"a@ @b@ @c@ @d", b"@ @", 2), [&b"a"[..], b"b"]);
        assert_eq!(first_fields(b"a@ @b", b"@ @", 8), [&b"a"[..], b"b"]);
        assert_eq!(first_fields(b"3: :9", b": :", 3), [&b"3"[..], b"9"]);
        assert_eq!(first_fields(b"", b"@ @", 3), [&b""[..]]);
    }

    #[test]
    fn pairs_are_looked_up_in_the_bags() {
        let bags = HashMap::from([
            (
                (0, 0),
                Bag {
                    file: 0,
                    start: 1,
                    end: 5,
                },
            ),
            (
                (1, 3),
                Bag {
                    file: 1,
                    start: 2,
                    end: 4,
                },
            ),
        ]);
        let pairs =
            read_pairs(b"[[0, 0, 0], [0, 1, 3]]\n\n[[0, 1, 3], [0, 0, 0]]\n", &bags).unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(
            pairs[0].1,
            Bag {
                file: 1,
                start: 2,
                end: 4
            }
        );
        assert!(read_pairs(b"[[0, 0, 0], [0, 1, 4]]", &bags).is_err());
        assert!(read_pairs(b"[[1, 0, 0], [0, 1, 3]]", &bags).is_err());
        assert!(read_pairs(b"[[0, 0, 0]]", &bags).is_err());
        assert!(read_pairs(b"[[0, 0, 0], [0, 1, 3]", &bags).is_err());
    }

    #[test]
    fn only_the_eof_line_after_a_final_lf_is_fixed() {
        let lf = LineCount::of(b"a\nb\nc\n");
        assert_eq!(lf.fix_end(4), 3);
        assert_eq!(lf.fix_end(3), 3);
        assert_eq!(lf.fix_end(5), 5);
        let no_lf = LineCount::of(b"a\nb\nc");
        assert_eq!(no_lf.fix_end(3), 3);
        assert_eq!(no_lf.fix_end(4), 4);
        let empty = LineCount::of(b"");
        assert_eq!(empty.fix_end(1), 1);
    }
}
