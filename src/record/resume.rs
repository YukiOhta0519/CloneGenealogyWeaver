use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::cli::RecordArgs;
use crate::config;
use crate::core::detection::scope_hash;
use crate::core::model::{PairInfo, VersionState};
use crate::git::{Git, revs, worktree};
use crate::graph::edges::EdgeBuilder;
use crate::graph::{Graph, VersionIdx};
use crate::plugin::{self, TempDirs};
use crate::process::Interrupt;
use crate::record::state::States;
use crate::record::{CallContext, Recorder, conclude, hex};
use crate::store::reconstruct::reconstruct;
use crate::store::{self, classifier_tables};
use crate::version::TOOL_VERSION;

struct PreviousRun {
    run_id: String,
    tool_version: String,
    git_version: String,
    config: String,
    graph_sha256: String,
}

pub fn run(args: RecordArgs, interrupt: &Interrupt) -> Result<()> {
    let rev = args.resume.clone().expect("resume is given");
    // ロックの前にも確かめるのは，終わった記録やイベントのデータベースでないファイルで，ロックのファイルを作らずに止めるためである．
    store::read_snapshot(&args.db, refuse_finished)?;
    let lock = store::lock(&args.db)?;
    let mut conn = reopen_after_lock(&args.db)?;

    let previous: PreviousRun = conn.query_row(
        "SELECT run_id, tool_version, git_version, config, graph_sha256 FROM run",
        [],
        |row| {
            Ok(PreviousRun {
                run_id: row.get(0)?,
                tool_version: row.get(1)?,
                git_version: row.get(2)?,
                config: row.get(3)?,
                graph_sha256: row.get(4)?,
            })
        },
    )?;
    let git = Git::new();
    let git_version = revs::git_version(&git, Path::new("."), interrupt)?;
    let graph_bytes = std::fs::read(&args.graph)
        .with_context(|| format!("cannot read graph {}", args.graph.display()))?;
    let config_bytes = std::fs::read(&args.config)
        .with_context(|| format!("cannot read config {}", args.config.display()))?;
    let mut differ = Vec::new();
    if hex(&Sha256::digest(&graph_bytes)) != previous.graph_sha256 {
        differ.push("the graph file".to_string());
    }
    if TOOL_VERSION != previous.tool_version {
        differ.push(format!(
            "the tool version ({TOOL_VERSION} now, {} before)",
            previous.tool_version
        ));
    }
    if git_version != previous.git_version {
        differ.push(format!(
            "the Git version ({git_version} now, {} before)",
            previous.git_version
        ));
    }
    if config_bytes != previous.config.as_bytes() {
        differ.push("the config file".to_string());
    }
    if !differ.is_empty() {
        bail!(
            "cannot resume: {} differ from the previous run",
            differ.join(", ")
        );
    }

    let config = config::load(&args.config)?;
    let invalid_config = || format!("invalid config {}", args.config.display());
    let detector = plugin::detector(&config.detector, &config.base_dir, interrupt)
        .with_context(invalid_config)?;
    let classifiers = plugin::classifiers(&config.classifiers, &config.base_dir, interrupt)
        .with_context(invalid_config)?;
    let repository = revs::check_repository(&git, &args.repo, interrupt)?;
    revs::check_not_shallow(&git, &repository, interrupt)?;
    let Some(repository_text) = repository.to_str() else {
        bail!("--repo {} is not UTF-8", repository.display());
    };

    let (graph, statuses, scope_hashes) = graph_from_record(&conn)?;
    let start = resume_position(&graph, &rev)?;
    for &v in &graph.order[..start] {
        if statuses[v.index()] == "pending" {
            bail!(
                "cannot resume from {}: version {} before it is not recorded",
                graph.versions[graph.order[start].index()].commit,
                graph.versions[v.index()].commit
            );
        }
    }
    let from = graph.order[start];
    clear_from(
        &mut conn,
        i64::from(graph.position[from.index()]) + 1,
        i64::from(from.0) + 1,
    )?;
    log::info!("resuming from {}", graph.versions[from.index()].commit);

    let clone = super::clone_dir(&args)?;
    let temp = TempDirs::new(&args.db, &previous.run_id)?;
    let mut recorder = Recorder {
        git: &git,
        interrupt,
        config: &config,
        graph: &graph,
        repository: &repository,
        clone: &clone,
        run_id: &previous.run_id,
        detector,
        classifiers,
        temp,
    };
    let result = prepare(
        &mut recorder,
        &conn,
        &graph,
        start,
        &statuses,
        &scope_hashes,
    )
    .and_then(|(builder, states)| recorder.record(&mut conn, start, builder, states));
    let temp = recorder.temp;
    conclude(
        result,
        conn,
        &args,
        repository_text,
        &clone,
        &previous.run_id,
        &temp,
        lock,
    )
}

/// ロックを取った後に読み直すのは，前の確かめとロックの間に，前の record が記録を終えることがあるからである．
///
/// 状態を読み直すまで open_write を呼ばないのは，ジャーナルを WAL に切り替え，終わった記録を書き換えるからである
fn reopen_after_lock(db: &Path) -> Result<Connection> {
    store::read_snapshot(db, refuse_finished)?;
    store::open_write(db)
}

fn refuse_finished(tx: &rusqlite::Transaction) -> Result<()> {
    let status: String = tx.query_row("SELECT status FROM run", [], |row| row.get(0))?;
    if status == "finished" {
        bail!("the record is finished; there is nothing to resume");
    }
    Ok(())
}

/// 入力したグラフを読み直さずに表から作るのは，前の実行の後にコミットが増えて短いハッシュが曖昧になっても
/// 再開できるようにするためである．グラフの順は求め直し，記録と違えば止める．
fn graph_from_record(conn: &Connection) -> Result<(Graph, Vec<String>, Vec<Option<String>>)> {
    let rows: Vec<(String, i64, String, Option<String>)> = conn
        .prepare("SELECT commit_id, topo_order, status, scope_sha256 FROM version ORDER BY id")?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut parents: Vec<Vec<VersionIdx>> = vec![Vec::new(); rows.len()];
    let mut statement =
        conn.prepare("SELECT version, parent FROM input_parent ORDER BY version, position")?;
    for row in statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))? {
        let (version, parent) = row?;
        parents[(version - 1) as usize].push(VersionIdx((parent - 1) as u32));
    }
    let versions = rows
        .iter()
        .zip(parents)
        .map(|(row, parents)| (row.0.clone(), parents))
        .collect();
    let graph = Graph::new(versions).map_err(|v| {
        anyhow::anyhow!(
            "the record is broken: version {} is on a cycle",
            rows[v.index()].0
        )
    })?;
    for (i, row) in rows.iter().enumerate() {
        if i64::from(graph.position[i]) + 1 != row.1 {
            bail!(
                "the record is broken: the graph order of version {} differs from the record",
                row.0
            );
        }
    }
    let statuses = rows.iter().map(|row| row.2.clone()).collect();
    let hashes = rows.into_iter().map(|row| row.3).collect();
    Ok((graph, statuses, hashes))
}

/// Git でなくグラフの版の中から探すのは，グラフの中で 1 つに決まれば受け付けるからである．
fn resume_position(graph: &Graph, rev: &str) -> Result<usize> {
    let hex = rev
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !hex || !(4..=64).contains(&rev.len()) {
        bail!("--resume {rev:?} is not a commit hash");
    }
    let found: Vec<usize> = graph
        .order
        .iter()
        .enumerate()
        .filter(|(_, v)| graph.versions[v.index()].commit.starts_with(rev))
        .map(|(k, _)| k)
        .collect();
    match found.as_slice() {
        [k] => Ok(*k),
        [] => bail!("--resume {rev}: no version in the graph starts with it"),
        _ => bail!(
            "--resume {rev} is ambiguous; {} versions in the graph start with it",
            found.len()
        ),
    }
}

/// 消すことと run を running に戻すことを 1 つのトランザクションで行うのは，途中で止まっても，
/// 消しかけの記録を残さないためである．
fn clear_from(conn: &mut Connection, topo_order: i64, version_id: i64) -> Result<()> {
    let classifiers: Vec<String> = conn
        .prepare("SELECT name FROM plugin WHERE kind = 'classifier' ORDER BY id")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let later = "SELECT id FROM version WHERE topo_order >= ?1";
    let edges = format!("SELECT id FROM edge WHERE child IN ({later})");
    let events = format!("SELECT id FROM pair_event WHERE edge IN ({edges})");
    // 参照する側から消すのは，外部キーに CASCADE が無いからである．
    for name in &classifiers {
        let Some(tables) = classifier_tables(name)? else {
            continue;
        };
        if tables.labels {
            tx.execute(
                &format!("DELETE FROM {name}_pair_event WHERE pair_event IN ({events})"),
                params![topo_order],
            )?;
            tx.execute(
                &format!("DELETE FROM {name}_root_pair WHERE version IN ({later})"),
                params![topo_order],
            )?;
        }
        for table in tables.per_version {
            tx.execute(
                &format!("DELETE FROM {table} WHERE version IN ({later})"),
                params![topo_order],
            )?;
        }
    }
    for table in [
        "pair_event",
        "fragment_entry",
        "fragment_mapping",
        "edge_file_issue",
    ] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE edge IN ({edges})"),
            params![topo_order],
        )?;
    }
    tx.execute(
        &format!("DELETE FROM edge WHERE child IN ({later})"),
        params![topo_order],
    )?;
    tx.execute(
        &format!("DELETE FROM root_pair WHERE version IN ({later})"),
        params![topo_order],
    )?;
    tx.execute(
        &format!("DELETE FROM plugin_failure WHERE version IN ({later})"),
        params![topo_order],
    )?;
    tx.execute(
        "UPDATE version SET status = 'pending', failed_plugin = NULL, failure_message = NULL, root = NULL,
           pair_count = NULL, fragment_count = NULL, scope_sha256 = NULL
         WHERE topo_order >= ?1",
        params![topo_order],
    )?;
    tx.execute(
        "UPDATE run SET status = 'running', stop_message = NULL, finished_at = NULL",
        [],
    )?;
    tx.execute(
        "INSERT INTO run_resume (resumed_at, from_version) VALUES (?1, ?2)",
        params![store::now(), version_id],
    )?;
    tx.commit()?;
    Ok(())
}

fn prepare<'g>(
    recorder: &mut Recorder,
    conn: &Connection,
    graph: &'g Graph,
    start: usize,
    statuses: &[String],
    scope_hashes: &[Option<String>],
) -> Result<(EdgeBuilder<'g>, States)> {
    recorder.temp.clear()?;
    worktree::reuse_or_create(
        recorder.git,
        recorder.repository,
        recorder.clone,
        recorder.run_id,
        recorder.interrupt,
    )?;
    // プラグインも Git も動かさずに呼び直すのは，捨てる時期が記録の状態だけで決まるからである．
    let mut builder = EdgeBuilder::new(graph);
    for &v in &graph.order[..start] {
        builder.finish(v, statuses[v.index()] == "broken");
    }
    let labeling: Vec<&'static [&'static str]> = recorder
        .classifiers
        .iter()
        .filter(|c| !c.labels().is_empty())
        .map(|c| c.labels())
        .collect();
    let mut states = States::default();
    for v in builder.held() {
        recorder.interrupt.check()?;
        let commit = graph.versions[v.index()].commit.as_str();
        let rebuilt = reconstruct(conn, i64::from(v.0) + 1, recorder.interrupt)?;
        let pairs = rebuilt
            .pairs
            .into_iter()
            .map(|(pair, recorded)| {
                let labels = labeling
                    .iter()
                    .zip(&recorded.labels)
                    .map(|(allowed, label)| {
                        allowed.iter().copied().find(|known| known == label).with_context(|| {
                            format!("the record has label {label:?}, which the classifier does not know")
                        })
                    })
                    .collect::<Result<Vec<&'static str>>>()?;
                Ok((pair, PairInfo { similarity: recorded.similarity, labels }))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        // 範囲を求め直すのは，次の edge の Appear に範囲に入ったの印を付けるのに，親の版の範囲が要るからである．
        // 版ごとの行と途中の失敗を書かないのは，その版の記録は前の実行で終わっていて，書くと止めずに動かした記録と違うからである
        worktree::checkout(recorder.git, recorder.clone, commit, recorder.interrupt)?;
        let tree = revs::tree(recorder.git, recorder.clone, commit, recorder.interrupt)?;
        let ctx = CallContext {
            git: recorder.git,
            clone: recorder.clone,
            commit,
            interrupt: recorder.interrupt,
        };
        let mut numbers = vec![1u64; recorder.classifiers.len() + 1];
        let scope = match recorder.scope(
            &ctx,
            &tree,
            &mut numbers,
            &mut Vec::new(),
            &mut Vec::new(),
        )? {
            Ok(scope) => scope,
            Err(plugin) => bail!(
                "cannot find the detection scope of {commit} again: plugin {plugin} kept failing"
            ),
        };
        if Some(scope_hash(&scope)) != scope_hashes[v.index()] {
            bail!("the detection scope of {commit} differs from the record");
        }
        states.insert(v, VersionState::new(pairs, scope));
    }
    Ok((builder, states))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// run の行だけを書いた記録を作り，ジャーナルを DELETE に戻して閉じる
    fn record_with_status(dir: &Path, status: &str) -> std::path::PathBuf {
        let db = dir.join("e.db");
        let mut conn = store::create(&db).unwrap();
        let tx = conn.transaction().unwrap();
        store::create_schema(&tx, &[]).unwrap();
        let finished_at = (status == "finished").then_some("2026-01-01T00:00:01Z");
        tx.execute(
            "INSERT INTO run VALUES ('r', ?1, 't', 'g', '/repo', '', 'h', '2026-01-01T00:00:00Z', ?2, ?3, NULL)",
            rusqlite::params![crate::version::EVENTS_SCHEMA_VERSION, finished_at, status],
        )
        .unwrap();
        tx.commit().unwrap();
        conn.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))
            .unwrap();
        drop(conn);
        db
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cgweave-resume-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_record_finished_before_the_lock_is_refused_without_touching_the_journal() {
        let dir = temp_dir("finished");
        let db = record_with_status(&dir, "finished");
        let before = std::fs::read(&db).unwrap();
        let err = reopen_after_lock(&db).err().unwrap();
        assert!(err.to_string().contains("the record is finished"), "{err}");
        assert!(!store::is_wal(&db).unwrap());
        assert_eq!(std::fs::read(&db).unwrap(), before);
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["e.db"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_record_still_running_after_the_lock_is_opened_for_writing() {
        let dir = temp_dir("running");
        let db = record_with_status(&dir, "running");
        let conn = reopen_after_lock(&db).unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
