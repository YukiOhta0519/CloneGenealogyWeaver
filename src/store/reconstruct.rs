use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::core::model::{Fragment, Pair};
use crate::process::Interrupt;
use crate::store::classifier_tables;

/// ラベルを文字列で持つのは，読むコマンドがプラグインを作らず，ラベルの値を知らないからである
#[derive(Clone, PartialEq, Debug)]
pub struct RecordedPair {
    pub similarity: Option<f64>,
    pub labels: Vec<String>,
}

type EventRow = (i64, Option<i64>, Option<i64>, Option<f64>);

pub struct Reconstructed {
    pub labelers: Vec<String>,
    pub pairs: BTreeMap<Pair, RecordedPair>,
}

/// プラグインの値を作らずに plugin の表と組み込みの定義から引くのは，読むコマンドが設定ファイルを受け取らないからである．
pub fn labelers(conn: &Connection) -> Result<Vec<String>> {
    let names: Vec<String> = conn
        .prepare("SELECT name FROM plugin WHERE kind = 'classifier' ORDER BY id")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut labelers = Vec::new();
    for name in names {
        if classifier_tables(&name)?.is_some_and(|tables| tables.labels) {
            labelers.push(name);
        }
    }
    Ok(labelers)
}

/// 1 本の edge だけをたどってよいのは，書かないのが 3 つ組の変わらない Persist だけで，
/// どの edge から見ても子の版のペアの一覧が同じになるからである．
pub fn reconstruct(
    conn: &Connection,
    version: i64,
    interrupt: &Interrupt,
) -> Result<Reconstructed> {
    let labelers = labelers(conn)?;

    // 番号の一番小さい edge をたどるのは，組み立て方の決まりで，親の側の版がグラフの順で一番前の edge だからである．
    let mut path = Vec::new();
    let mut at = version;
    let origin = loop {
        let edge: Option<(i64, Option<i64>)> = conn
            .query_row(
                "SELECT id, parent FROM edge WHERE child = ?1 ORDER BY id LIMIT 1",
                params![at],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match edge {
            Some((id, Some(parent))) => {
                path.push(id);
                at = parent;
            }
            Some((id, None)) => {
                path.push(id);
                break None;
            }
            None => break Some(at),
        }
    };
    path.reverse();

    // 組み立ての間を辞書の番号で持つのは，3 つ組を作る手間を，最後に残ったペアの数だけにするためである．
    let mut pairs: HashMap<i64, RecordedPair> = HashMap::new();
    if let Some(root) = origin {
        let mut statement =
            conn.prepare("SELECT pair, similarity FROM root_pair WHERE version = ?1")?;
        let rows = statement.query_map(params![root], |row| {
            Ok((row.get::<_, i64>(0)?, row.get(1)?))
        })?;
        for row in rows {
            let (pair, similarity) = row?;
            pairs.insert(
                pair,
                RecordedPair {
                    similarity,
                    labels: Vec::with_capacity(labelers.len()),
                },
            );
        }
        for name in &labelers {
            let mut statement = conn.prepare(&format!(
                "SELECT pair, label FROM {name}_root_pair WHERE version = ?1"
            ))?;
            let rows = statement.query_map(params![root], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (pair, label) = row?;
                let Some(recorded) = pairs.get_mut(&pair) else {
                    bail!(
                        "the record is broken: {name}_root_pair has a pair that is not a root pair"
                    );
                };
                recorded.labels.push(label);
            }
        }
    }

    for edge in path {
        interrupt.check()?;
        let mut labels: HashMap<i64, Vec<String>> = HashMap::new();
        for name in &labelers {
            let mut statement = conn.prepare(&format!(
                "SELECT l.pair_event, l.label FROM {name}_pair_event l
                 JOIN pair_event e ON e.id = l.pair_event WHERE e.edge = ?1"
            ))?;
            let rows = statement.query_map(params![edge], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (event, label) = row?;
                labels.entry(event).or_default().push(label);
            }
        }
        let mut statement = conn.prepare(
            "SELECT id, parent_pair, child_pair, similarity FROM pair_event WHERE edge = ?1 ORDER BY id",
        )?;
        let events: Vec<EventRow> = statement
            .query_map(params![edge], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        // 親のペアを先に全部外すのは，同じ edge の中で外したペアと足したペアが同じ番号のことがあるからである
        for (_, parent, _, _) in &events {
            if let Some(parent) = parent {
                pairs.remove(parent);
            }
        }
        for (id, _, child, similarity) in events {
            if let Some(child) = child {
                pairs.insert(
                    child,
                    RecordedPair {
                        similarity,
                        labels: labels.remove(&id).unwrap_or_default(),
                    },
                );
            }
        }
    }

    // 辞書の片を全部読まないのは，辞書がほかの版の片も持ち，全部の量で数百万になりうるからである
    let mut paths: HashMap<String, Rc<str>> = HashMap::new();
    let mut intern = |path: String| {
        Rc::clone(
            paths
                .entry(path.clone())
                .or_insert_with(|| Rc::from(path.as_str())),
        )
    };
    let mut by_triple = BTreeMap::new();
    let mut lookup = conn.prepare_cached(
        "SELECT a.path, a.start_line, a.end_line, b.path, b.start_line, b.end_line FROM pair p
         JOIN fragment_view a ON a.id = p.fragment_a JOIN fragment_view b ON b.id = p.fragment_b
         WHERE p.id = ?1",
    )?;
    for (id, recorded) in pairs {
        if recorded.labels.len() != labelers.len() {
            bail!(
                "the record is broken: pair {id} has {} labels for {} plugins",
                recorded.labels.len(),
                labelers.len()
            );
        }
        let (pa, sa, ea, pb, sb, eb): (String, u32, u32, String, u32, u32) =
            lookup.query_row(params![id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?;
        let pair = Pair {
            a: Fragment {
                path: intern(pa),
                start: sa,
                end: ea,
            },
            b: Fragment {
                path: intern(pb),
                start: sb,
                end: eb,
            },
        };
        by_triple.insert(pair, recorded);
    }

    let (pair_count, fragment_count): (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT pair_count, fragment_count FROM version WHERE id = ?1",
            params![version],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .context("the version is not in the record")?;
    let distinct: HashSet<&Fragment> = by_triple.keys().flat_map(|p| [&p.a, &p.b]).collect();
    if pair_count != Some(by_triple.len() as i64) || fragment_count != Some(distinct.len() as i64) {
        bail!(
            "the record is broken: version {version} has {} pairs and {} fragments rebuilt from the events, \
             but {pair_count:?} and {fragment_count:?} written",
            by_triple.len(),
            distinct.len()
        );
    }
    Ok(Reconstructed {
        labelers,
        pairs: by_triple,
    })
}
