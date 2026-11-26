use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::rc::Rc;

use anyhow::{Context, Result};

use crate::core::file_mapping::{EdgeLines, FileLines, FileMap, NewFile, OldFile};
use crate::core::fragment_events::{Entry, entries};
use crate::core::fragment_mapping::{Mapping, map_fragments};
use crate::core::line_events::{Diff, line_events};
use crate::core::lines::BlobLines;
use crate::core::model::VersionState;
use crate::core::pair_events::{PairEvent, pair_events};
use crate::git::Git;
use crate::git::hunks::{BlobDiff, BlobReader, diff_blobs};
use crate::git::raw::diff_raw;
use crate::git::revs::is_ancestor;
use crate::process::Interrupt;
use crate::record::line_cache::LineCache;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Issue {
    DiffFailed,
    HunkTooLarge(u32),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileIssue {
    pub old_path: Option<Vec<u8>>,
    pub new_path: Option<Vec<u8>>,
    pub issue: Issue,
}

pub struct EdgeResult {
    pub is_ancestor: Option<bool>,
    pub issues: Vec<FileIssue>,
    pub mappings: Vec<Mapping>,
    pub entries: Vec<Entry>,
    pub events: Vec<PairEvent>,
}

pub struct Tools<'a> {
    pub git: &'a Git,
    pub clone: &'a Path,
    pub interrupt: &'a Interrupt,
    pub blobs: &'a mut BlobReader,
    pub cache: &'a mut LineCache,
}

/// parent_rev と parent_commit を分けるのは，空のツリーとの edge では diff だけを取り，祖先を調べないからである
pub fn process(
    tools: &mut Tools,
    parent_rev: &str,
    parent_commit: Option<&str>,
    parent: &VersionState,
    child_commit: &str,
    child: &VersionState,
    child_tree: &HashMap<Vec<u8>, String>,
) -> Result<EdgeResult> {
    // 空のツリーとの edge で祖先を調べないのは，空のツリーがコミットでないからである．
    let is_ancestor = match parent_commit {
        Some(commit) => Some(is_ancestor(
            tools.git,
            tools.clone,
            commit,
            child_commit,
            tools.interrupt,
        )?),
        None => None,
    };
    let raw = diff_raw(
        tools.git,
        tools.clone,
        parent_rev,
        child_commit,
        tools.interrupt,
    )?;
    let files = FileMap::new(&raw);

    let mut lines = EdgeLines::default();
    let mut issues = Vec::new();
    for pair in file_pairs(parent, child, &files, child_tree) {
        tools.interrupt.check()?;
        let file = match &pair {
            FilePair::Deleted { old_oid, .. } => FileLines::deleted(read(tools, old_oid)?),
            FilePair::Kept {
                old_path,
                new_path,
                old_oid,
                new_oid,
            } => {
                let old = read(tools, old_oid)?;
                let new = read(tools, new_oid)?;
                let diff = if old_oid == new_oid {
                    BlobDiff::Hunks(Vec::new())
                } else {
                    diff_blobs(tools.git, tools.clone, old_oid, new_oid, tools.interrupt)?
                };
                let events = match &diff {
                    _ if old_oid == new_oid => line_events(&old, &new, Diff::SameBlob),
                    BlobDiff::Hunks(hunks) => line_events(&old, &new, Diff::Hunks(hunks)),
                    BlobDiff::Failed => line_events(&old, &new, Diff::Failed),
                }
                .with_context(|| {
                    format!(
                        "cannot find the line events from {} to {}",
                        String::from_utf8_lossy(old_path),
                        String::from_utf8_lossy(new_path)
                    )
                })?;
                let issue = match diff {
                    BlobDiff::Failed => Some(Issue::DiffFailed),
                    BlobDiff::Hunks(_) if events.skipped_hunks > 0 => {
                        Some(Issue::HunkTooLarge(events.skipped_hunks))
                    }
                    BlobDiff::Hunks(_) => None,
                };
                if let Some(issue) = issue {
                    issues.push(FileIssue {
                        old_path: Some(old_path.clone()),
                        new_path: Some(new_path.clone()),
                        issue,
                    });
                }
                FileLines { old, new, events }
            }
        };
        let (old_path, new_path) = pair.paths();
        lines.insert(old_path, new_path, file);
    }

    let mappings = map_fragments(parent, child, &files, &lines)?;
    let entries = entries(child, &mappings, &files, &lines);
    let events = pair_events(parent, child, &mappings, &files);
    // 無い側を先にして旧のパス，新のパスの順に並べるのは，edge_file_issue の行をこの順に決めたからである
    issues.sort_by(|a, b| (&a.old_path, &a.new_path).cmp(&(&b.old_path, &b.new_path)));
    Ok(EdgeResult {
        is_ancestor,
        issues,
        mappings,
        entries,
        events,
    })
}

fn read(tools: &mut Tools, oid: &str) -> Result<Rc<BlobLines>> {
    let blobs = &mut *tools.blobs;
    let interrupt = tools.interrupt;
    tools.cache.get(oid, || blobs.read(oid, interrupt))
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum FilePair {
    Deleted {
        old_path: Vec<u8>,
        old_oid: String,
    },
    Kept {
        old_path: Vec<u8>,
        new_path: Vec<u8>,
        old_oid: String,
        new_oid: String,
    },
}

impl FilePair {
    fn paths(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        match self {
            FilePair::Deleted { old_path, .. } => (Some(old_path.clone()), None),
            FilePair::Kept {
                old_path, new_path, ..
            } => (Some(old_path.clone()), Some(new_path.clone())),
        }
    }
}

/// 足されたファイルの組を作らないのは，片側の行しか要らず，Added は file_mapping の表から決まるからである．
/// 同じファイルの blob のハッシュを子の版の tree から引くのは，ファイルの対応の出力に無いファイルの blob が親と子で同じだからである
fn file_pairs(
    parent: &VersionState,
    child: &VersionState,
    files: &FileMap,
    child_tree: &HashMap<Vec<u8>, String>,
) -> BTreeSet<FilePair> {
    let same = |path: &[u8]| {
        let oid = child_tree
            .get(path)
            .unwrap_or_else(|| {
                panic!(
                    "{} is in the tree of the child",
                    String::from_utf8_lossy(path)
                )
            })
            .clone();
        FilePair::Kept {
            old_path: path.to_vec(),
            new_path: path.to_vec(),
            old_oid: oid.clone(),
            new_oid: oid,
        }
    };
    let mut pairs = BTreeSet::new();
    for path in parent.index.keys() {
        let path = path.as_bytes();
        pairs.insert(match files.old_file(path) {
            OldFile::Same => same(path),
            OldFile::Changed {
                new_path,
                old_oid,
                new_oid,
                ..
            } => FilePair::Kept {
                old_path: path.to_vec(),
                new_path: new_path.clone(),
                old_oid: old_oid.clone(),
                new_oid: new_oid.clone(),
            },
            OldFile::Deleted { old_oid } => FilePair::Deleted {
                old_path: path.to_vec(),
                old_oid: old_oid.clone(),
            },
        });
    }
    for path in child.index.keys() {
        let path = path.as_bytes();
        match files.new_file(path) {
            NewFile::Same => {
                pairs.insert(same(path));
            }
            NewFile::Changed {
                old_path,
                old_oid,
                new_oid,
                ..
            } => {
                pairs.insert(FilePair::Kept {
                    old_path: old_path.clone(),
                    new_path: path.to_vec(),
                    old_oid: old_oid.clone(),
                    new_oid: new_oid.clone(),
                });
            }
            NewFile::Added { .. } => {}
        }
    }
    pairs
}
