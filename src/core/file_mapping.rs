use std::collections::HashMap;
use std::rc::Rc;

use crate::core::line_events::{LineEvents, OldLine};
use crate::core::lines::BlobLines;
use crate::core::model::{RawEntry, RawStatus, Side};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OldFile {
    Same,
    Changed {
        new_path: Vec<u8>,
        renamed: bool,
        old_oid: String,
        new_oid: String,
    },
    Deleted {
        old_oid: String,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NewFile {
    Same,
    Added {
        new_oid: String,
    },
    Changed {
        old_path: Vec<u8>,
        renamed: bool,
        old_oid: String,
        new_oid: String,
    },
}

static SAME_OLD: OldFile = OldFile::Same;
static SAME_NEW: NewFile = NewFile::Same;

/// 出力に無いパスを表に入れないのは，ツリーの全部のパスを入れずに済ませるためである．
#[derive(Default, Debug)]
pub struct FileMap {
    old: HashMap<Vec<u8>, OldFile>,
    new: HashMap<Vec<u8>, NewFile>,
}

impl FileMap {
    /// 普通のファイルでない側を捨てるのは，範囲が普通のファイルだけで，片がそこにしか無いからである．
    /// T を変更として扱わないのは，種類の入れ替わりを，消えたファイルと足されたファイルに分ける決まりだからである
    pub fn new(entries: &[RawEntry]) -> FileMap {
        let mut map = FileMap::default();
        for entry in entries {
            let old = entry.old.as_ref().filter(|side| is_regular(side.mode));
            let new = entry.new.as_ref().filter(|side| is_regular(side.mode));
            let split = matches!(entry.status, RawStatus::TypeChanged);
            match (old, new) {
                (Some(old), Some(new)) if !split => {
                    let renamed = matches!(entry.status, RawStatus::Renamed { .. });
                    map.changed(old, new, renamed);
                }
                (old, new) => {
                    if let Some(old) = old {
                        map.old.insert(
                            old.path.clone(),
                            OldFile::Deleted {
                                old_oid: old.oid.clone(),
                            },
                        );
                    }
                    if let Some(new) = new {
                        map.new.insert(
                            new.path.clone(),
                            NewFile::Added {
                                new_oid: new.oid.clone(),
                            },
                        );
                    }
                }
            }
        }
        map
    }

    fn changed(&mut self, old: &Side, new: &Side, renamed: bool) {
        self.old.insert(
            old.path.clone(),
            OldFile::Changed {
                new_path: new.path.clone(),
                renamed,
                old_oid: old.oid.clone(),
                new_oid: new.oid.clone(),
            },
        );
        self.new.insert(
            new.path.clone(),
            NewFile::Changed {
                old_path: old.path.clone(),
                renamed,
                old_oid: old.oid.clone(),
                new_oid: new.oid.clone(),
            },
        );
    }

    pub fn old_file(&self, path: &[u8]) -> &OldFile {
        self.old.get(path).unwrap_or(&SAME_OLD)
    }

    pub fn new_file(&self, path: &[u8]) -> &NewFile {
        self.new.get(path).unwrap_or(&SAME_NEW)
    }

    pub fn source<'a>(&'a self, path: &'a [u8]) -> Option<&'a [u8]> {
        match self.new_file(path) {
            NewFile::Same => Some(path),
            NewFile::Added { .. } => None,
            NewFile::Changed { old_path, .. } => Some(old_path),
        }
    }
}

fn is_regular(mode: u32) -> bool {
    matches!(mode, 0o100644 | 0o100755)
}

pub struct FileLines {
    pub old: Rc<BlobLines>,
    pub new: Rc<BlobLines>,
    pub events: LineEvents,
}

impl FileLines {
    /// 消えたファイルの行を全部削除にするのをここで行うのは，片側の行しか無く，line_events に渡さないからである
    pub fn deleted(old: Rc<BlobLines>) -> FileLines {
        let events = LineEvents {
            old: vec![OldLine::Deleted; old.len() as usize],
            new: Vec::new(),
            skipped_hunks: 0,
        };
        FileLines {
            old,
            new: Rc::new(BlobLines::new(Vec::new())),
            events,
        }
    }

    /// テストのときだけ作るのは，record が足されたファイルの行の情報を使わないからである
    #[cfg(test)]
    pub fn added(new: Rc<BlobLines>) -> FileLines {
        let events = LineEvents {
            old: Vec::new(),
            new: vec![crate::core::line_events::NewLine::Added; new.len() as usize],
            skipped_hunks: 0,
        };
        FileLines {
            old: Rc::new(BlobLines::new(Vec::new())),
            new,
            events,
        }
    }
}

#[derive(Default)]
pub struct EdgeLines {
    by_old: HashMap<Vec<u8>, Rc<FileLines>>,
    by_new: HashMap<Vec<u8>, Rc<FileLines>>,
}

impl EdgeLines {
    pub fn insert(
        &mut self,
        old_path: Option<Vec<u8>>,
        new_path: Option<Vec<u8>>,
        lines: FileLines,
    ) {
        let lines = Rc::new(lines);
        if let Some(path) = old_path {
            self.by_old.insert(path, Rc::clone(&lines));
        }
        if let Some(path) = new_path {
            self.by_new.insert(path, lines);
        }
    }

    pub fn by_old(&self, path: &[u8]) -> Option<&FileLines> {
        self.by_old.get(path).map(|lines| &**lines)
    }

    pub fn by_new(&self, path: &[u8]) -> Option<&FileLines> {
        self.by_new.get(path).map(|lines| &**lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::line_events::NewLine;

    fn side(mode: u32, oid: char, path: &str) -> Option<Side> {
        Some(Side {
            mode,
            oid: oid.to_string().repeat(40),
            path: path.as_bytes().to_vec(),
        })
    }

    fn entry(status: RawStatus, old: Option<Side>, new: Option<Side>) -> RawEntry {
        RawEntry { status, old, new }
    }

    #[test]
    fn paths_not_in_the_output_are_the_same() {
        let map = FileMap::new(&[]);
        assert_eq!(map.old_file(b"a.java"), &OldFile::Same);
        assert_eq!(map.new_file(b"a.java"), &NewFile::Same);
        assert_eq!(map.source(b"a.java"), Some(&b"a.java"[..]));
    }

    #[test]
    fn modified_renamed_added_and_deleted_files() {
        let map = FileMap::new(&[
            entry(
                RawStatus::Modified,
                side(0o100644, '1', "m"),
                side(0o100755, '2', "m"),
            ),
            entry(
                RawStatus::Renamed { score: 90 },
                side(0o100644, '3', "from"),
                side(0o100644, '4', "to"),
            ),
            entry(RawStatus::Added, None, side(0o100644, '5', "new")),
            entry(RawStatus::Deleted, side(0o100644, '6', "gone"), None),
        ]);
        assert!(
            matches!(map.old_file(b"m"), OldFile::Changed { new_path, renamed: false, .. } if new_path == b"m")
        );
        assert!(
            matches!(map.old_file(b"from"), OldFile::Changed { new_path, renamed: true, .. } if new_path == b"to")
        );
        assert!(
            matches!(map.new_file(b"to"), NewFile::Changed { old_path, renamed: true, .. } if old_path == b"from")
        );
        assert_eq!(map.source(b"to"), Some(&b"from"[..]));
        assert!(matches!(map.new_file(b"new"), NewFile::Added { .. }));
        assert_eq!(map.source(b"new"), None);
        assert!(matches!(map.old_file(b"gone"), OldFile::Deleted { .. }));
        // 改名の前のパスは，子の版では出どころの無いパスとして引かれない
        assert_eq!(map.new_file(b"from"), &NewFile::Same);
    }

    #[test]
    fn type_changes_are_split_and_the_non_regular_side_is_dropped() {
        let map = FileMap::new(&[
            entry(
                RawStatus::TypeChanged,
                side(0o100644, '1', "f2l"),
                side(0o120000, '2', "f2l"),
            ),
            entry(
                RawStatus::TypeChanged,
                side(0o120000, '3', "l2f"),
                side(0o100644, '4', "l2f"),
            ),
            entry(
                RawStatus::TypeChanged,
                side(0o100644, '5', "f2s"),
                side(0o160000, '6', "f2s"),
            ),
        ]);
        assert!(matches!(map.old_file(b"f2l"), OldFile::Deleted { .. }));
        assert_eq!(map.new_file(b"f2l"), &NewFile::Same);
        assert_eq!(map.old_file(b"l2f"), &OldFile::Same);
        assert!(matches!(map.new_file(b"l2f"), NewFile::Added { .. }));
        assert!(matches!(map.old_file(b"f2s"), OldFile::Deleted { .. }));
    }

    #[test]
    fn non_regular_files_are_ignored() {
        let map = FileMap::new(&[
            entry(RawStatus::Added, None, side(0o120000, '1', "link")),
            entry(
                RawStatus::Modified,
                side(0o160000, '2', "sub"),
                side(0o160000, '3', "sub"),
            ),
            entry(RawStatus::Deleted, side(0o120000, '4', "old-link"), None),
        ]);
        assert_eq!(map.new_file(b"link"), &NewFile::Same);
        assert_eq!(map.old_file(b"sub"), &OldFile::Same);
        assert_eq!(map.old_file(b"old-link"), &OldFile::Same);
    }

    #[test]
    fn deleted_and_added_files_have_lines_on_one_side() {
        let blob = Rc::new(BlobLines::new(b"a\nb\n".to_vec()));
        let deleted = FileLines::deleted(Rc::clone(&blob));
        assert_eq!(deleted.events.old, [OldLine::Deleted, OldLine::Deleted]);
        assert_eq!(deleted.new.len(), 0);
        let added = FileLines::added(blob);
        assert_eq!(added.events.new, [NewLine::Added, NewLine::Added]);
        let mut edge = EdgeLines::default();
        edge.insert(Some(b"a".to_vec()), Some(b"b".to_vec()), added);
        assert!(edge.by_old(b"a").is_some());
        assert!(edge.by_new(b"b").is_some());
        assert!(edge.by_new(b"a").is_none());
    }
}
