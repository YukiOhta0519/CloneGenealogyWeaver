// 捨てる時期をここで数えないのは，graph/edges.rs が数え，ここは言われた版を捨てるだけにするためである．

use std::collections::HashMap;

use crate::core::model::VersionState;
use crate::graph::VersionIdx;

/// 壊れた版の状態を持たないのは，壊れた版から出る edge を作らないからである
#[derive(Default)]
pub struct States(HashMap<VersionIdx, VersionState>);

impl States {
    pub fn insert(&mut self, v: VersionIdx, state: VersionState) {
        self.0.insert(v, state);
    }

    pub fn get(&self, v: VersionIdx) -> &VersionState {
        self.0
            .get(&v)
            .unwrap_or_else(|| panic!("the state of {v:?} was dropped before its last edge"))
    }

    pub fn drop_versions(&mut self, released: &[VersionIdx]) {
        for v in released {
            self.0.remove(v);
        }
    }
}
