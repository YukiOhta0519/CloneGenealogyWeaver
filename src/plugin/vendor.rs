use anyhow::Result;

use crate::config::PluginSection;
use crate::core::model::{Pair, TreeEntry};
use crate::error::PluginError;
use crate::plugin::{Classifier, Items, PluginCall, ScopeResult, classifier_label, refuse_timeout};

pub struct Vendor;

impl Vendor {
    pub fn new(section: &PluginSection) -> Result<Vendor> {
        let label = classifier_label(section);
        refuse_timeout(&label, section)?;
        Items::new(label, section, &[])?;
        Ok(Vendor)
    }
}

impl Classifier for Vendor {
    fn name(&self) -> &'static str {
        "vendor"
    }

    fn effective_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    fn labels(&self) -> &'static [&'static str] {
        &[]
    }

    fn scope(
        &mut self,
        _call: &PluginCall,
        tree: &[TreeEntry],
    ) -> Result<ScopeResult, PluginError> {
        let keep = tree
            .iter()
            .filter(|entry| entry.is_regular_file() && !under_vendor(&entry.path))
            .map(|entry| entry.path.clone())
            .collect();
        Ok(ScopeResult {
            keep: Some(keep),
            rows: Vec::new(),
        })
    }

    fn classify(&mut self, _pairs: &[Pair]) -> Result<Vec<&'static str>, PluginError> {
        unreachable!("vendor gives no labels, so classify is not called")
    }
}

fn under_vendor(path: &[u8]) -> bool {
    let mut elements: Vec<&[u8]> = path.split(|&b| b == b'/').collect();
    elements.pop();
    elements.contains(&&b"vendor"[..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::ObjectKind;

    #[test]
    fn files_under_a_vendor_directory_are_left_out() {
        for path in ["vendor/a.go", "x/vendor/y/B.java", "a/vendor/vendor"] {
            assert!(under_vendor(path.as_bytes()), "{path}");
        }
        for path in [
            "vendor",
            "x/vendor",
            "Vendor/a.go",
            "vendors/a.go",
            "my-vendor/a",
            "a.go",
        ] {
            assert!(!under_vendor(path.as_bytes()), "{path}");
        }
    }

    #[test]
    fn scope_keeps_regular_files_outside_vendor() {
        let entry = |mode, kind, path: &str| TreeEntry {
            mode,
            kind,
            oid: "0".repeat(40),
            path: path.as_bytes().to_vec(),
        };
        let tree = [
            entry(0o100644, ObjectKind::Blob, "src/A.java"),
            entry(0o100755, ObjectKind::Blob, "run.sh"),
            entry(0o100644, ObjectKind::Blob, "vendor/lib/B.java"),
            entry(0o120000, ObjectKind::Blob, "link"),
            entry(0o160000, ObjectKind::Commit, "sub"),
        ];
        let interrupt = crate::process::Interrupt::default();
        let call = PluginCall {
            commit: "c",
            work_tree: std::path::Path::new("/nowhere"),
            temp_dir: std::path::Path::new("/nowhere"),
            deadline: None,
            interrupt: &interrupt,
        };
        let scoped = Vendor.scope(&call, &tree).unwrap();
        let keep: Vec<&[u8]> = scoped
            .keep
            .as_ref()
            .unwrap()
            .iter()
            .map(Vec::as_slice)
            .collect();
        assert_eq!(keep, [&b"run.sh"[..], b"src/A.java"]);
        assert!(scoped.rows.is_empty());
    }
}
