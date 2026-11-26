pub mod edge;
pub mod line_cache;
pub mod resume;
pub mod state;
pub mod writer;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::cli::RecordArgs;
use crate::config::{Config, PluginSection};
use crate::core::detection::{
    check_labels, check_lines, narrow_scope, normalize, scope_candidates, scope_hash,
};
use crate::core::lines::BlobLines;
use crate::core::model::{Pair, PairInfo, Scope, TreeEntry, VersionState};
use crate::error::{Interrupted, PluginError, one_line};
use crate::git::hunks::BlobReader;
use crate::git::{Git, revs, worktree};
use crate::graph::Graph;
use crate::graph::edges::EdgeBuilder;
use crate::plugin::{Classifier, Detector, Failure, PluginCall, PluginRow, TempDirs, with_retries};
use crate::process::Interrupt;
use crate::record::edge::Tools;
use crate::record::line_cache::LineCache;
use crate::record::state::States;
use crate::record::writer::{EdgeWrite, Status, VersionWrite};
use crate::version::{EVENTS_SCHEMA_VERSION, TOOL_VERSION};
use crate::{config, graph, plugin, store};

pub fn run(args: RecordArgs, interrupt: &Interrupt) -> Result<()> {
    // 設定とグラフより後に見ないのは，Git でハッシュを直す前に止めるためである．
    // 確かめられないときに無いとみなさないのは，あるかもしれないファイルに記録を始めないためである
    let exists = args.db.try_exists().unwrap_or(true);
    match (&args.resume, exists) {
        (None, true) => bail!(
            "database {} already exists; pass --resume to continue it",
            args.db.display()
        ),
        (Some(_), false) => bail!(
            "database {} does not exist; nothing to resume",
            args.db.display()
        ),
        (Some(_), true) => return resume::run(args, interrupt),
        (None, false) => {}
    }

    let config = config::load(&args.config)?;
    let invalid_config = || format!("invalid config {}", args.config.display());
    let detector = plugin::detector(&config.detector, &config.base_dir, interrupt)
        .with_context(invalid_config)?;
    let classifiers = plugin::classifiers(&config.classifiers, &config.base_dir, interrupt)
        .with_context(invalid_config)?;
    let git = Git::new();
    let git_version = revs::git_version(&git, Path::new("."), interrupt)?;
    let repository = revs::check_repository(&git, &args.repo, interrupt)?;
    revs::check_not_shallow(&git, &repository, interrupt)?;
    let Some(repository_text) = repository.to_str() else {
        bail!("--repo {} is not UTF-8", repository.display());
    };

    let name = args.graph.display().to_string();
    let bytes = std::fs::read(&args.graph).with_context(|| format!("cannot read graph {name}"))?;
    let parsed = graph::input::parse(&name, &bytes)?;
    let mut resolved = HashMap::new();
    for rev in parsed.revisions() {
        // Git の失敗だけで割り込みを見ないのは，ツールだけに SIGTERM が来たときは Git が成功し続けるからである
        interrupt.check()?;
        resolved.insert(
            rev.to_string(),
            revs::resolve_commit(&git, &repository, rev, interrupt)?,
        );
    }
    let graph = graph::input::build(&name, &parsed, &resolved)?;
    let commits: Vec<String> = graph.versions.iter().map(|v| v.commit.clone()).collect();
    let times = revs::committer_times(&git, &repository, &commits, interrupt)?
        .into_iter()
        .zip(&commits)
        .map(|(time, commit)| {
            store::format_time(time)
                .with_context(|| format!("cannot use the committer time of {commit}"))
        })
        .collect::<Result<Vec<String>>>()?;

    let lock = store::lock(&args.db)?;
    let mut conn = store::create(&args.db)?;
    let run_id = store::new_run_id();
    let graph_sha256 = hex(&Sha256::digest(&bytes));
    let start = Start {
        run_id: &run_id,
        git_version: &git_version,
        repository: repository_text,
        config: &config,
        graph_sha256: &graph_sha256,
        graph: &graph,
        times: &times,
        detector: detector.as_ref(),
        classifiers: &classifiers,
    };
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let names: Vec<&str> = classifiers.iter().map(|c| c.name()).collect();
    store::create_schema(&tx, &names)?;
    write_start(&tx, &start)?;
    tx.commit()?;

    let clone = clone_dir(&args)?;
    let temp = TempDirs::new(&args.db, &run_id)?;
    let mut recorder = Recorder {
        git: &git,
        interrupt,
        config: &config,
        graph: &graph,
        repository: &repository,
        clone: &clone,
        run_id: &run_id,
        detector,
        classifiers,
        temp,
    };
    let result = worktree::create(&git, &repository, &clone, &run_id, interrupt)
        .and_then(|()| recorder.record(&mut conn, 0, EdgeBuilder::new(&graph), States::default()));
    let temp = recorder.temp;
    conclude(
        result,
        conn,
        &args,
        repository_text,
        &clone,
        &run_id,
        &temp,
        lock,
    )
}

/// 絶対パスにするのは，プラグインに作業ツリーを絶対パスで渡すためである．
pub(crate) fn clone_dir(args: &RecordArgs) -> Result<std::path::PathBuf> {
    let clone = args
        .clone_dir
        .clone()
        .unwrap_or_else(|| store::side_path(&args.db, ".clone"));
    std::path::absolute(&clone)
        .with_context(|| format!("cannot resolve the clone directory {}", clone.display()))
}

/// 誤りで止まったときに run を failed にし，割り込みのときに running のまま残すのは，
/// 再開で区別せず，status で止めた理由を出せるようにするためである．
#[allow(clippy::too_many_arguments)]
fn conclude(
    result: Result<()>,
    conn: Connection,
    args: &RecordArgs,
    repository: &str,
    clone: &Path,
    run_id: &str,
    temp: &TempDirs,
    lock: store::DbLock,
) -> Result<()> {
    match result {
        Ok(()) => finish(conn, clone, run_id, temp, &args.db, lock),
        Err(err) => {
            let interrupted = err.chain().any(|cause| cause.is::<Interrupted>());
            if !interrupted {
                let message = one_line(&format!("{err:#}"));
                if let Err(write_err) = conn.execute(
                    "UPDATE run SET status = 'failed', stop_message = ?1",
                    params![message],
                ) {
                    log::error!("cannot mark the run as failed: {write_err}");
                }
            }
            report_next(&conn, args, repository);
            Err(err)
        }
    }
}

struct Start<'a> {
    run_id: &'a str,
    git_version: &'a str,
    repository: &'a str,
    config: &'a Config,
    graph_sha256: &'a str,
    graph: &'a Graph,
    times: &'a [String],
    detector: &'a dyn Detector,
    classifiers: &'a [Box<dyn Classifier>],
}

/// 表の作成と同じトランザクションで書くのは，途中で止まったときに中身の無いファイルだけが残るようにするためである．
fn write_start(tx: &Transaction, s: &Start) -> Result<()> {
    tx.execute(
        "INSERT INTO run (run_id, schema_version, tool_version, git_version, repository, config,
           graph_sha256, started_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'running')",
        params![
            s.run_id,
            EVENTS_SCHEMA_VERSION,
            TOOL_VERSION,
            s.git_version,
            s.repository,
            s.config.raw,
            s.graph_sha256,
            store::now()
        ],
    )?;
    tx.execute(
        "INSERT INTO plugin (id, kind, name, config) VALUES (1, 'detector', ?1, ?2)",
        params![
            s.config.detector.name,
            s.detector.effective_config().to_string()
        ],
    )?;
    for (i, classifier) in s.classifiers.iter().enumerate() {
        tx.execute(
            "INSERT INTO plugin (id, kind, name, config) VALUES (?1, 'classifier', ?2, ?3)",
            params![
                i as i64 + 2,
                classifier.name(),
                classifier.effective_config().to_string()
            ],
        )?;
    }
    for (i, version) in s.graph.versions.iter().enumerate() {
        tx.execute(
            "INSERT INTO version (id, commit_id, topo_order, committed_at, status)
             VALUES (?1, ?2, ?3, ?4, 'pending')",
            params![
                i as i64 + 1,
                version.commit,
                i64::from(s.graph.position[i]) + 1,
                s.times[i]
            ],
        )?;
    }
    for (i, version) in s.graph.versions.iter().enumerate() {
        for (j, parent) in version.parents.iter().enumerate() {
            tx.execute(
                "INSERT INTO input_parent (version, position, parent) VALUES (?1, ?2, ?3)",
                params![i as i64 + 1, j as i64 + 1, i64::from(parent.0) + 1],
            )?;
        }
    }
    Ok(())
}

struct Recorder<'a> {
    git: &'a Git,
    interrupt: &'a Interrupt,
    config: &'a Config,
    graph: &'a Graph,
    repository: &'a Path,
    clone: &'a Path,
    run_id: &'a str,
    detector: Box<dyn Detector>,
    classifiers: Vec<Box<dyn Classifier>>,
    temp: TempDirs,
}

enum Detection {
    Broken(i64, String),
    Ok(VersionState, String),
}

impl Recorder<'_> {
    /// start は，グラフの順で最初に処理する版の位置である．再開では，その前の版の状態を組み立ててから渡す
    fn record(
        &mut self,
        conn: &mut Connection,
        start: usize,
        mut builder: EdgeBuilder,
        mut states: States,
    ) -> Result<()> {
        let mut blobs = BlobReader::start(self.git, self.clone)?;
        let mut cache = LineCache::new(self.config.line_cache_mib.get());
        let labelers: Vec<&'static str> = self
            .classifiers
            .iter()
            .filter(|c| !c.labels().is_empty())
            .map(|c| c.name())
            .collect();
        let empty_state = VersionState::empty();
        let mut empty_tree: Option<String> = None;
        let total = self.graph.order.len();

        for (k, &v) in self.graph.order.iter().enumerate().skip(start) {
            self.interrupt.check()?;
            let started = Instant::now();
            let commit = self.graph.versions[v.index()].commit.as_str();
            worktree::checkout(self.git, self.clone, commit, self.interrupt)?;
            let tree = revs::tree(self.git, self.clone, commit, self.interrupt)?;
            let tree_oids: HashMap<Vec<u8>, String> = tree
                .iter()
                .map(|e| (e.path.clone(), e.oid.clone()))
                .collect();
            let mut failures = Vec::new();
            let mut plugin_rows = Vec::new();
            let detection = self.detect(
                commit,
                &tree,
                &tree_oids,
                &mut blobs,
                &mut cache,
                &mut failures,
                &mut plugin_rows,
            )?;

            let mut edges = Vec::new();
            let mut root = None;
            if let Detection::Ok(state, _) = &detection {
                let mut tools = Tools {
                    git: self.git,
                    clone: self.clone,
                    interrupt: self.interrupt,
                    blobs: &mut blobs,
                    cache: &mut cache,
                };
                let incoming = builder.incoming(v);
                for e in &incoming {
                    self.interrupt.check()?;
                    let parent_commit = self.graph.versions[e.parent.index()].commit.as_str();
                    let parent = states.get(e.parent);
                    let result = edge::process(
                        &mut tools,
                        parent_commit,
                        Some(parent_commit),
                        parent,
                        commit,
                        state,
                        &tree_oids,
                    )
                    .with_context(|| {
                        format!("cannot process the edge from {parent_commit} to {commit}")
                    })?;
                    edges.push(EdgeWrite {
                        parent_id: Some(i64::from(e.parent.0) + 1),
                        parent,
                        skipped: e.skipped,
                        result,
                    });
                }
                if incoming.is_empty() {
                    if revs::has_git_parents(self.git, self.clone, commit, self.interrupt)? {
                        root = Some("censored");
                    } else {
                        let empty = match &empty_tree {
                            Some(oid) => oid.clone(),
                            None => {
                                let oid = revs::empty_tree(self.git, self.clone, self.interrupt)?;
                                empty_tree = Some(oid.clone());
                                oid
                            }
                        };
                        let result = edge::process(
                            &mut tools,
                            &empty,
                            None,
                            &empty_state,
                            commit,
                            state,
                            &tree_oids,
                        )
                        .with_context(|| {
                            format!("cannot process the edge from the empty tree to {commit}")
                        })?;
                        // 飛ばした版の数を 0 にするのは，空のツリーと版の間に飛ばした版が無いからである．
                        edges.push(EdgeWrite {
                            parent_id: None,
                            parent: &empty_state,
                            skipped: 0,
                            result,
                        });
                        root = Some("empty_tree");
                    }
                }
            }

            self.interrupt.check()?;
            let finished_at = (k + 1 == total).then(store::now);
            let pairs = match &detection {
                Detection::Ok(state, _) => state.pairs.len(),
                Detection::Broken(..) => 0,
            };
            let status = match &detection {
                Detection::Broken(plugin, message) => Status::Broken {
                    plugin: *plugin,
                    message,
                },
                Detection::Ok(state, scope_sha256) => Status::Ok {
                    state,
                    scope_sha256,
                    root,
                    edges,
                },
            };
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            writer::write_version(
                &tx,
                &labelers,
                &VersionWrite {
                    version_id: i64::from(v.0) + 1,
                    status,
                    failures: &failures,
                    plugin_rows: &plugin_rows,
                    finished_at: finished_at.as_deref(),
                },
            )
            .with_context(|| format!("cannot write version {commit}"))?;
            tx.commit()?;

            let broken = matches!(detection, Detection::Broken(..));
            if let Detection::Ok(state, _) = detection {
                states.insert(v, state);
            }
            states.drop_versions(&builder.finish(v, broken));
            log::info!(
                "[{}/{total}] {} {} pairs={pairs} {:.1}s",
                k + 1,
                &commit[..12],
                if broken { "broken" } else { "ok" },
                started.elapsed().as_secs_f64()
            );
        }
        blobs.finish()
    }

    /// 範囲を検出より先に決めるのは，検出器に範囲だけを解析させるためである．
    #[allow(clippy::too_many_arguments)]
    fn detect(
        &mut self,
        commit: &str,
        tree: &[TreeEntry],
        tree_oids: &HashMap<Vec<u8>, String>,
        blobs: &mut BlobReader,
        cache: &mut LineCache,
        failures: &mut Vec<Failure>,
        plugin_rows: &mut Vec<PluginRow>,
    ) -> Result<Detection> {
        let retries = self.config.retries;
        // 番号を版とプラグインごとに持つのは，scope と classify の失敗を通しの番号で記録するためである．
        let mut numbers = vec![1u64; self.classifiers.len() + 1];
        let ctx = CallContext {
            git: self.git,
            clone: self.clone,
            commit,
            interrupt: self.interrupt,
        };
        let scope = match self.scope(&ctx, tree, &mut numbers, failures, plugin_rows)? {
            Ok(scope) => scope,
            Err(plugin) => return Ok(broken(plugin, failures)),
        };
        let scope_paths: Vec<Vec<u8>> = scope.0.iter().cloned().collect();

        let detector = &mut self.detector;
        let temp = &mut self.temp;
        let section = &self.config.detector;
        let raw = with_retries(retries, 1, &mut numbers[0], failures, |nth| {
            ctx.call(nth, temp, section, |call| {
                detector.detect(call, &scope_paths)
            })
        })?;
        let Some(raw) = raw else {
            return Ok(broken(1, failures));
        };
        let detected = normalize(commit, raw, &scope)?;
        let mut lines: HashMap<Rc<str>, Rc<BlobLines>> = HashMap::new();
        for fragment in detected.fragments() {
            if lines.contains_key(&fragment.path) {
                continue;
            }
            let oid = &tree_oids[fragment.path.as_bytes()];
            let blob = cache.get(oid, || blobs.read(oid, self.interrupt))?;
            lines.insert(Rc::clone(&fragment.path), blob);
        }
        check_lines(commit, &detected, &lines)?;

        let pairs: Vec<Pair> = detected.pairs.keys().cloned().collect();
        let mut labels: Vec<Vec<&'static str>> = Vec::new();
        for (i, classifier) in self.classifiers.iter_mut().enumerate() {
            if classifier.labels().is_empty() {
                continue;
            }
            let classified = with_retries(
                retries,
                i as i64 + 2,
                &mut numbers[i + 1],
                failures,
                |nth| {
                    if nth > 1 {
                        worktree::checkout(ctx.git, ctx.clone, commit, ctx.interrupt)
                            .map_err(PluginError::Abort)?;
                    }
                    classifier.classify(&pairs).map_err(prefixed("classify"))
                },
            )?;
            let Some(classified) = classified else {
                return Ok(broken(i as i64 + 2, failures));
            };
            check_labels(
                classifier.name(),
                commit,
                &classified,
                classifier.labels(),
                pairs.len(),
            )?;
            labels.push(classified);
        }

        let pairs: BTreeMap<Pair, PairInfo> = detected
            .pairs
            .into_iter()
            .enumerate()
            .map(|(j, (pair, similarity))| {
                let labels = labels.iter().map(|per_plugin| per_plugin[j]).collect();
                (pair, PairInfo { similarity, labels })
            })
            .collect();
        let hash = scope_hash(&scope);
        Ok(Detection::Ok(VersionState::new(pairs, scope), hash))
    }

    /// 分類のプラグインの scope を名前の順に呼び，範囲を作る．回数を使い切ったプラグインがあれば，その番号を返す
    fn scope(
        &mut self,
        ctx: &CallContext,
        tree: &[TreeEntry],
        numbers: &mut [u64],
        failures: &mut Vec<Failure>,
        plugin_rows: &mut Vec<PluginRow>,
    ) -> Result<std::result::Result<Scope, i64>> {
        let retries = self.config.retries;
        let mut keeps: Vec<(&'static str, BTreeSet<Vec<u8>>)> = Vec::new();
        for (i, classifier) in self.classifiers.iter_mut().enumerate() {
            let section = &self.config.classifiers[classifier.name()];
            let temp = &mut self.temp;
            let scoped = with_retries(
                retries,
                i as i64 + 2,
                &mut numbers[i + 1],
                failures,
                |nth| {
                    ctx.call(nth, temp, section, |call| {
                        classifier.scope(call, tree).map_err(prefixed("scope"))
                    })
                },
            )?;
            let Some(scoped) = scoped else {
                return Ok(Err(i as i64 + 2));
            };
            // 後で壊れた版になっても書くのは，scope が成功した事実が変わらないからである．
            plugin_rows.extend(scoped.rows);
            if let Some(keep) = scoped.keep {
                keeps.push((classifier.name(), keep));
            }
        }
        let keep_refs: Vec<(&str, &BTreeSet<Vec<u8>>)> =
            keeps.iter().map(|(n, k)| (*n, k)).collect();
        Ok(Ok(narrow_scope(
            ctx.commit,
            scope_candidates(tree),
            &keep_refs,
        )?))
    }
}

fn broken(plugin: i64, failures: &[Failure]) -> Detection {
    let message = failures
        .last()
        .map(|f| f.message.clone())
        .unwrap_or_default();
    Detection::Broken(plugin, message)
}

/// 理由の先頭に付けるのは，1 つの分類のプラグインの scope と classify の失敗を見分けるためである．
fn prefixed(what: &'static str) -> impl Fn(PluginError) -> PluginError {
    move |err| match err {
        PluginError::Failed { reason } => PluginError::Failed {
            reason: format!("{what}: {reason}"),
        },
        other => other,
    }
}

struct CallContext<'a> {
    git: &'a Git,
    clone: &'a Path,
    commit: &'a str,
    interrupt: &'a Interrupt,
}

impl CallContext<'_> {
    /// 呼ぶたびに作業ツリーを確かめるのは，プラグインが作業ツリーを変えたまま次へ進まないためである．
    fn call<T>(
        &self,
        nth: u64,
        temp: &mut TempDirs,
        section: &PluginSection,
        f: impl FnOnce(&PluginCall) -> Result<T, PluginError>,
    ) -> Result<T, PluginError> {
        if nth > 1 {
            worktree::checkout(self.git, self.clone, self.commit, self.interrupt)
                .map_err(PluginError::Abort)?;
        }
        let dir = temp.create().map_err(PluginError::Abort)?;
        let call = PluginCall {
            commit: self.commit,
            work_tree: self.clone,
            temp_dir: &dir,
            deadline: plugin::deadline(section.timeout_secs),
            interrupt: self.interrupt,
        };
        let result = f(&call);
        temp.remove(&dir).map_err(PluginError::Abort)?;
        if let Err(PluginError::Abort(err)) = result {
            return Err(PluginError::Abort(err));
        }
        worktree::check_unchanged(self.git, self.clone, self.interrupt)
            .map_err(PluginError::Abort)?;
        result
    }
}

/// 片づけの 1 つが失敗しても残りを続けるのは，記録はもう終わっていて，残るものを少なくするためである．
fn finish(
    conn: Connection,
    clone: &Path,
    run_id: &str,
    temp: &TempDirs,
    db: &Path,
    lock: store::DbLock,
) -> Result<()> {
    let mut problems = Vec::new();
    if let Err(err) = switch_journal(&conn) {
        problems.push(format!(
            "{err:#}; keep {0}, {0}-wal and {0}-shm together, and run PRAGMA journal_mode=DELETE \
             on it with SQLite after other processes close it",
            db.display()
        ));
    }
    drop(conn);
    if let Err(err) = worktree::remove(clone, run_id) {
        problems.push(format!("{err:#}"));
    }
    if let Err(err) = temp.finish() {
        problems.push(format!("{err:#}"));
    }
    let lock_path = store::side_path(db, ".lock");
    if let Err(err) = std::fs::remove_file(&lock_path) {
        problems.push(format!("cannot delete {}: {err}", lock_path.display()));
    }
    drop(lock);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "the record finished, but cleaning up failed: {}",
            problems.join("; ")
        ))
    }
}

/// 待つ時間の上限に任せずにやり直すのは，1 回読んだ接続が開いている間は待たずに失敗するからである．
/// 割り込みを見ないのは，記録はもう終わっていて，片づけを最後まで済ませる方が残るものが少ないからである．
fn switch_journal(conn: &Connection) -> Result<()> {
    let started = Instant::now();
    loop {
        match conn.query_row("PRAGMA journal_mode = DELETE", [], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(mode) if mode.eq_ignore_ascii_case("delete") => return Ok(()),
            Ok(mode) => bail!("SQLite kept the journal mode {mode}"),
            Err(err) if started.elapsed() >= Duration::from_secs(5) => {
                return Err(err).context("cannot switch the journal back to DELETE");
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// 次に渡す版をグラフの順で最初の pending の版にするのは，書き終えた版がどれもその前にあるからである．
fn report_next(conn: &Connection, args: &RecordArgs, repository: &str) {
    let next: Option<String> = conn
        .query_row(
            "SELECT commit_id FROM version WHERE status = 'pending' ORDER BY topo_order LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();
    let Some(next) = next else {
        return;
    };
    let clone_dir = args
        .clone_dir
        .as_ref()
        .map(|dir| format!(" --clone-dir {}", dir.display()))
        .unwrap_or_default();
    eprintln!("next: {next}");
    eprintln!(
        "resume: cgweave record --repo {repository} --graph {} --config {} --db {}{clone_dir} --resume {next}",
        args.graph.display(),
        args.config.display(),
        args.db.display()
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
