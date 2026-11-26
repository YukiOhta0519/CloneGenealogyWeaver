use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use anyhow::Result;

use crate::core::lines::BlobLines;

/// Rc で渡すのは，使っている途中の行の情報を表から捨てても，使い終わるまでメモリに残すためである．
pub struct LineCache {
    limit: u64,
    used: u64,
    clock: u64,
    entries: HashMap<String, (Rc<BlobLines>, u64)>,
    by_use: BTreeMap<u64, String>,
}

impl LineCache {
    pub fn new(limit_mib: u64) -> LineCache {
        LineCache {
            limit: limit_mib.saturating_mul(1 << 20),
            used: 0,
            clock: 0,
            entries: HashMap::new(),
            by_use: BTreeMap::new(),
        }
    }

    /// 1 つで上限を超える blob も読むのは，その edge の間は Rc で残り，使えるからである．
    pub fn get(
        &mut self,
        oid: &str,
        load: impl FnOnce() -> Result<Vec<u8>>,
    ) -> Result<Rc<BlobLines>> {
        self.clock += 1;
        if let Some((lines, used_at)) = self.entries.get_mut(oid) {
            self.by_use.remove(used_at);
            *used_at = self.clock;
            self.by_use.insert(self.clock, oid.to_string());
            return Ok(Rc::clone(lines));
        }
        let lines = Rc::new(BlobLines::new(load()?));
        self.used += lines.size();
        self.entries
            .insert(oid.to_string(), (Rc::clone(&lines), self.clock));
        self.by_use.insert(self.clock, oid.to_string());
        while self.used > self.limit {
            let Some((_, oldest)) = self.by_use.pop_first() else {
                break;
            };
            if let Some((evicted, _)) = self.entries.remove(&oldest) {
                self.used -= evicted.size();
            }
        }
        Ok(lines)
    }

    #[cfg(test)]
    fn held(&self) -> Vec<&str> {
        self.by_use.values().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(n: usize) -> Vec<u8> {
        vec![b'x'; n]
    }

    #[test]
    fn the_least_recently_used_blob_goes_first() {
        // 1 MiB の上限に，1 行の 400,000 バイトの blob を入れる．1 つは 400,013 バイトに数える
        let mut cache = LineCache::new(1);
        cache.get("a", || Ok(bytes(400_000))).unwrap();
        cache.get("b", || Ok(bytes(400_000))).unwrap();
        cache.get("a", || panic!("a is still held")).unwrap();
        cache.get("c", || Ok(bytes(400_000))).unwrap();
        assert_eq!(cache.held(), ["a", "c"]);
        let mut loaded = false;
        cache
            .get("b", || {
                loaded = true;
                Ok(bytes(400_000))
            })
            .unwrap();
        assert!(loaded);
        assert_eq!(cache.held(), ["c", "b"]);
    }

    #[test]
    fn a_blob_over_the_limit_is_still_usable_after_it_leaves() {
        let mut cache = LineCache::new(1);
        let big = cache.get("big", || Ok(b"a\nb\n".repeat(400_000))).unwrap();
        assert!(cache.held().is_empty());
        assert_eq!(big.len(), 800_000);
        assert_eq!(big.line(799_999), b"b");
    }

    #[test]
    fn a_load_failure_keeps_nothing() {
        let mut cache = LineCache::new(1);
        assert!(
            cache
                .get("a", || Err(anyhow::anyhow!("cannot read")))
                .is_err()
        );
        assert!(cache.held().is_empty());
    }
}
