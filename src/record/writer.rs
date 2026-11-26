use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, Transaction, params};

use crate::core::fragment_events::{ContentEvent, content_event};
use crate::core::fragment_mapping::{ChildKind, Mapping};
use crate::core::model::{Cell, Fragment, Pair, VersionState};
use crate::core::pair_events::{DisappearReason, PairEvent};
use crate::plugin::{Failure, PluginId, PluginRow};
use crate::record::edge::{EdgeResult, Issue};

pub struct EdgeWrite<'a> {
    pub parent_id: Option<i64>,
    pub parent: &'a VersionState,
    pub skipped: u32,
    pub result: EdgeResult,
}

pub enum Status<'a> {
    Broken {
        plugin: PluginId,
        message: &'a str,
    },
    Ok {
        state: &'a VersionState,
        scope_sha256: &'a str,
        root: Option<&'static str>,
        edges: Vec<EdgeWrite<'a>>,
    },
}

pub struct VersionWrite<'a> {
    pub version_id: i64,
    pub status: Status<'a>,
    pub failures: &'a [Failure],
    pub plugin_rows: &'a [PluginRow],
    /// 最後の版なら終わりの時刻．最後の版を書いたことと記録が終わったことを同じ時に確定するためである．
    pub finished_at: Option<&'a str>,
}

pub fn write_version(tx: &Transaction, labelers: &[&str], w: &VersionWrite) -> Result<()> {
    let plans: Vec<EdgePlan> = match &w.status {
        Status::Ok { state, edges, .. } => edges.iter().map(|e| plan(e, state)).collect(),
        Status::Broken { .. } => Vec::new(),
    };
    let roots: Vec<&Pair> = match &w.status {
        Status::Ok {
            state,
            root: Some("censored"),
            ..
        } => state.pairs.keys().collect(),
        _ => Vec::new(),
    };

    // 辞書を先に書くのは，外部キーが参照する行を先に要るからである．
    let mut paths: BTreeSet<String> = BTreeSet::new();
    let mut fragments: BTreeSet<Fragment> = BTreeSet::new();
    let mut pairs: BTreeSet<Pair> = BTreeSet::new();
    for plan in &plans {
        for issue in &plan.edge.result.issues {
            for path in [&issue.old_path, &issue.new_path].into_iter().flatten() {
                paths.insert(utf8(path)?);
            }
        }
        for m in &plan.mappings {
            fragments.insert(m.parent.clone());
            if let Some((child, _)) = &m.child {
                fragments.insert(child.clone());
            }
        }
        for entry in &plan.edge.result.entries {
            fragments.insert(entry.fragment.clone());
        }
        for row in &plan.rows {
            pairs.extend(row.parent.into_iter().chain(row.child).cloned());
        }
    }
    pairs.extend(roots.iter().map(|p| (*p).clone()));
    fragments.extend(pairs.iter().flat_map(|p| [p.a.clone(), p.b.clone()]));
    paths.extend(fragments.iter().map(|f| f.path.to_string()));
    let dict = Dictionary::fill(tx, &paths, &fragments, &pairs)?;

    match &w.status {
        Status::Broken { plugin, message } => {
            tx.execute(
                "UPDATE version SET status = 'broken', failed_plugin = ?1, failure_message = ?2 WHERE id = ?3",
                params![plugin, message, w.version_id],
            )?;
        }
        Status::Ok {
            state,
            scope_sha256,
            root,
            ..
        } => {
            tx.execute(
                "UPDATE version SET status = 'ok', root = ?1, pair_count = ?2, fragment_count = ?3,
                   scope_sha256 = ?4 WHERE id = ?5",
                params![
                    root,
                    state.pairs.len() as i64,
                    state.fragments.len() as i64,
                    scope_sha256,
                    w.version_id
                ],
            )?;
        }
    }
    for failure in w.failures {
        tx.execute(
            "INSERT INTO plugin_failure (version, plugin, attempt, message) VALUES (?1, ?2, ?3, ?4)",
            params![w.version_id, failure.plugin, failure.attempt as i64, failure.message],
        )?;
    }
    for row in w.plugin_rows {
        let marks: Vec<String> = (1..=row.values.len() + 1)
            .map(|i| format!("?{i}"))
            .collect();
        let sql = format!("INSERT INTO {} VALUES ({})", row.table, marks.join(", "));
        let mut values: Vec<rusqlite::types::Value> = vec![w.version_id.into()];
        values.extend(row.values.iter().map(|cell| match cell {
            Cell::Int(n) => rusqlite::types::Value::Integer(*n),
            Cell::Text(s) => rusqlite::types::Value::Text(s.clone()),
        }));
        tx.execute(&sql, rusqlite::params_from_iter(values))
            .with_context(|| format!("cannot write a row of {}", row.table))?;
    }

    for plan in &plans {
        write_edge(tx, labelers, w.version_id, plan, &dict)?;
    }

    if let Status::Ok { state, .. } = &w.status {
        for pair in &roots {
            let info = &state.pairs[*pair];
            tx.execute(
                "INSERT INTO root_pair (version, pair, similarity) VALUES (?1, ?2, ?3)",
                params![w.version_id, dict.pair(pair), info.similarity],
            )?;
            for (name, label) in labelers.iter().zip(&info.labels) {
                tx.execute(
                    &format!(
                        "INSERT INTO {name}_root_pair (version, pair, label) VALUES (?1, ?2, ?3)"
                    ),
                    params![w.version_id, dict.pair(pair), label],
                )?;
            }
        }
    }

    if let Some(finished_at) = w.finished_at {
        tx.execute(
            "UPDATE run SET status = 'finished', finished_at = ?1",
            params![finished_at],
        )?;
    }
    Ok(())
}

struct PairRow<'a> {
    event: &'static str,
    parent: Option<&'a Pair>,
    child: Option<&'a Pair>,
    reason: Option<&'static str>,
    entered_scope: Option<bool>,
    labels_changed: Option<&'a [bool]>,
}

struct EdgePlan<'a> {
    edge: &'a EdgeWrite<'a>,
    child: &'a VersionState,
    mappings: Vec<&'a Mapping>,
    rows: Vec<PairRow<'a>>,
    implicit_pairs: i64,
    implicit_fragments: i64,
}

/// 書かない 2 つのイベントは書き出しの条件で決める．core が全部のイベントを返し，ここで数えるのは，
/// 判定の規則と書くかどうかの規則を分け，規則ごとのテストで書かないイベントも確かめるためである．
fn plan<'a>(edge: &'a EdgeWrite<'a>, child: &'a VersionState) -> EdgePlan<'a> {
    let result = &edge.result;
    let events: HashMap<&Fragment, (&Mapping, ContentEvent)> = result
        .mappings
        .iter()
        .map(|m| (&m.parent, (m, content_event(m))))
        .collect();
    let implicit_mapping = |m: &Mapping| {
        matches!(&m.child, Some((g, _)) if *g == m.parent)
            && m.counts.taken_in_lines == 0
            && content_event(m) == ContentEvent::Stable
    };
    let stable = |f: &Fragment| events[f].1 == ContentEvent::Stable;
    let mappings: Vec<&Mapping> = result
        .mappings
        .iter()
        .filter(|m| !implicit_mapping(m))
        .collect();
    let implicit_fragments = (result.mappings.len() - mappings.len()) as i64;

    let mut by_parent: BTreeMap<&Pair, PairRow> = BTreeMap::new();
    let mut appears = Vec::new();
    let mut implicit_pairs = 0;
    for event in &result.events {
        match event {
            PairEvent::Disappear { parent, reason } => {
                by_parent.insert(
                    parent,
                    PairRow {
                        event: "Disappear",
                        parent: Some(parent),
                        child: None,
                        reason: Some(reason_text(*reason)),
                        entered_scope: None,
                        labels_changed: None,
                    },
                );
            }
            PairEvent::Persist {
                parent,
                child: continued,
                labels_changed,
            } => {
                let unchanged = parent == continued
                    && stable(&parent.a)
                    && stable(&parent.b)
                    && labels_changed.iter().all(|changed| !changed)
                    && edge.parent.pairs[parent].similarity == child.pairs[continued].similarity;
                if unchanged {
                    implicit_pairs += 1;
                    continue;
                }
                by_parent.insert(
                    parent,
                    PairRow {
                        event: "Persist",
                        parent: Some(parent),
                        child: Some(continued),
                        reason: None,
                        entered_scope: None,
                        labels_changed: Some(labels_changed),
                    },
                );
            }
            // Merge を親のペアごとに 1 行にし，各親のペアの位置に入れるのは，pair_event の行をこの順に決めたからである
            PairEvent::Merge {
                parents,
                child: continued,
            } => {
                for parent in parents {
                    by_parent.insert(
                        parent,
                        PairRow {
                            event: "Merge",
                            parent: Some(parent),
                            child: Some(continued),
                            reason: None,
                            entered_scope: None,
                            labels_changed: None,
                        },
                    );
                }
            }
            PairEvent::Appear {
                child: appeared,
                entered_scope,
            } => appears.push(PairRow {
                event: "Appear",
                parent: None,
                child: Some(appeared),
                reason: None,
                entered_scope: Some(*entered_scope),
                labels_changed: None,
            }),
        }
    }
    let mut rows: Vec<PairRow> = by_parent.into_values().collect();
    rows.extend(appears);
    EdgePlan {
        edge,
        child,
        mappings,
        rows,
        implicit_pairs,
        implicit_fragments,
    }
}

fn write_edge(
    tx: &Transaction,
    labelers: &[&str],
    version_id: i64,
    plan: &EdgePlan,
    dict: &Dictionary,
) -> Result<()> {
    let edge = plan.edge;
    tx.execute(
        "INSERT INTO edge (parent, child, is_ancestor, skipped_versions, implicit_pairs, implicit_fragments)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            edge.parent_id,
            version_id,
            edge.result.is_ancestor,
            edge.skipped,
            plan.implicit_pairs,
            plan.implicit_fragments
        ],
    )?;
    let edge_id = tx.last_insert_rowid();
    for issue in &edge.result.issues {
        let path_id = |path: &Option<Vec<u8>>| -> Result<Option<i64>> {
            path.as_ref().map(|p| Ok(dict.path(&utf8(p)?))).transpose()
        };
        let (kind, hunks) = match issue.issue {
            Issue::DiffFailed => ("diff_failed", None),
            Issue::HunkTooLarge(n) => ("hunk_too_large", Some(n)),
        };
        tx.execute(
            "INSERT INTO edge_file_issue (edge, old_path, new_path, issue, hunks) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![edge_id, path_id(&issue.old_path)?, path_id(&issue.new_path)?, kind, hunks],
        )?;
    }
    let mut mapping_statement = tx.prepare_cached(
        "INSERT INTO fragment_mapping (edge, parent_fragment, child_fragment, child_kind, renamed, event,
           similarity_cut, file_deleted, parent_lines, parent_symbol_lines, parent_words, retained_lines,
           retained_symbol_lines, retained_words, common_words, modified_lines, whitespace_lines,
           taken_in_lines, taken_in_modified_lines, added_lines, child_words, text_similarity)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
    )?;
    for m in &plan.mappings {
        let event = content_event(m);
        let c = &m.counts;
        let (child, kind) = match &m.child {
            Some((g, ChildKind::Detected)) => (Some(dict.fragment(g)), Some("detected")),
            Some((g, ChildKind::Projected)) => (Some(dict.fragment(g)), Some("projected")),
            None => (None, None),
        };
        let event_text = match event {
            ContentEvent::Stable => "Stable",
            ContentEvent::Modified => "Modified",
            ContentEvent::Deleted => "Deleted",
        };
        let file_deleted = (event == ContentEvent::Deleted).then_some(m.file_deleted);
        mapping_statement.execute(params![
            edge_id,
            dict.fragment(&m.parent),
            child,
            kind,
            m.renamed,
            event_text,
            m.similarity_cut,
            file_deleted,
            c.parent_lines,
            c.parent_symbol_lines,
            c.parent_words,
            c.retained_lines,
            c.retained_symbol_lines,
            c.retained_words,
            c.common_words,
            c.modified_lines,
            c.whitespace_lines,
            c.taken_in_lines,
            c.taken_in_modified_lines,
            c.added_lines,
            c.child_words,
            c.text_similarity(),
        ])?;
    }
    for entry in &edge.result.entries {
        tx.execute(
            "INSERT INTO fragment_entry (edge, fragment, added, file_added) VALUES (?1, ?2, ?3, ?4)",
            params![edge_id, dict.fragment(&entry.fragment), entry.added, entry.file_added],
        )?;
    }
    let child = |pair: &Pair| &plan.child.pairs[pair];
    for row in &plan.rows {
        let similarity = match row.child {
            Some(pair) => child(pair).similarity,
            None => None,
        };
        tx.execute(
            "INSERT INTO pair_event (edge, event, parent_pair, child_pair, reason, entered_scope, similarity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                edge_id,
                row.event,
                row.parent.map(|p| dict.pair(p)),
                row.child.map(|p| dict.pair(p)),
                row.reason,
                row.entered_scope,
                similarity
            ],
        )?;
        let event_id = tx.last_insert_rowid();
        // Disappear に書かないのは，子のペアが無く，ラベルが無いからである．
        if let Some(pair) = row.child {
            let labels = &child(pair).labels;
            for (i, (name, label)) in labelers.iter().zip(labels).enumerate() {
                let changed = row.labels_changed.map(|changed| changed[i]);
                tx.execute(
                    &format!("INSERT INTO {name}_pair_event (pair_event, label, changed) VALUES (?1, ?2, ?3)"),
                    params![event_id, label, changed],
                )?;
            }
        }
    }
    Ok(())
}

fn reason_text(reason: DisappearReason) -> &'static str {
    match reason {
        DisappearReason::FragmentDeleted => "fragment_deleted",
        DisappearReason::FragmentsMerged => "fragments_merged",
        DisappearReason::OutOfScope => "out_of_scope",
        DisappearReason::UndetectedFragment => "undetected_fragment",
        DisappearReason::NotPaired => "not_paired",
    }
}

/// 止めるのは，path の表がパスを UTF-8 の文字列で持つからである．
fn utf8(path: &[u8]) -> Result<String> {
    match std::str::from_utf8(path) {
        Ok(path) => Ok(path.to_string()),
        Err(_) => bail!(
            "cannot write path {:?}, which is not UTF-8",
            String::from_utf8_lossy(path)
        ),
    }
}

/// 辞書をメモリに全部持たないのは，全部の量で辞書のペアが 100 万を超えうるからである．
struct Dictionary {
    paths: HashMap<String, i64>,
    fragments: HashMap<Fragment, i64>,
    pairs: HashMap<Pair, i64>,
}

impl Dictionary {
    /// 並べてから入れるのは，1 つの版の中では，パスはバイト列の順に，片とペアは 3 つ組の順に番号を振るからである
    fn fill(
        tx: &Transaction,
        paths: &BTreeSet<String>,
        fragments: &BTreeSet<Fragment>,
        pairs: &BTreeSet<Pair>,
    ) -> Result<Dictionary> {
        let mut dict = Dictionary {
            paths: HashMap::new(),
            fragments: HashMap::new(),
            pairs: HashMap::new(),
        };
        for path in paths {
            let id = find_or_insert(
                tx,
                "SELECT id FROM path WHERE path = ?1",
                "INSERT INTO path (path) VALUES (?1)",
                params![path],
            )?;
            dict.paths.insert(path.clone(), id);
        }
        for fragment in fragments {
            let path = dict.paths[&*fragment.path];
            let id = find_or_insert(
                tx,
                "SELECT id FROM fragment WHERE path = ?1 AND start_line = ?2 AND end_line = ?3",
                "INSERT INTO fragment (path, start_line, end_line) VALUES (?1, ?2, ?3)",
                params![path, fragment.start, fragment.end],
            )?;
            dict.fragments.insert(fragment.clone(), id);
        }
        for pair in pairs {
            let (a, b) = (dict.fragments[&pair.a], dict.fragments[&pair.b]);
            let id = find_or_insert(
                tx,
                "SELECT id FROM pair WHERE fragment_a = ?1 AND fragment_b = ?2",
                "INSERT INTO pair (fragment_a, fragment_b) VALUES (?1, ?2)",
                params![a, b],
            )?;
            dict.pairs.insert(pair.clone(), id);
        }
        Ok(dict)
    }

    fn path(&self, path: &str) -> i64 {
        self.paths[path]
    }

    fn fragment(&self, fragment: &Fragment) -> i64 {
        self.fragments[fragment]
    }

    fn pair(&self, pair: &Pair) -> i64 {
        self.pairs[pair]
    }
}

fn find_or_insert(
    tx: &Transaction,
    find: &str,
    insert: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<i64> {
    if let Some(id) = tx
        .prepare_cached(find)?
        .query_row(values, |row| row.get(0))
        .optional()?
    {
        return Ok(id);
    }
    tx.prepare_cached(insert)?.execute(values)?;
    Ok(tx.last_insert_rowid())
}
