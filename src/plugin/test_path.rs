use anyhow::Result;

use crate::config::PluginSection;
use crate::core::model::{Pair, TreeEntry};
use crate::error::PluginError;
use crate::plugin::{Classifier, Items, PluginCall, ScopeResult, classifier_label, refuse_timeout};

pub struct TestPath;

impl TestPath {
    pub fn new(section: &PluginSection) -> Result<TestPath> {
        let label = classifier_label(section);
        refuse_timeout(&label, section)?;
        Items::new(label, section, &[])?;
        Ok(TestPath)
    }
}

impl Classifier for TestPath {
    fn name(&self) -> &'static str {
        "test_path"
    }

    fn effective_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    fn labels(&self) -> &'static [&'static str] {
        &["product", "test", "mixed"]
    }

    fn scope(
        &mut self,
        _call: &PluginCall,
        _tree: &[TreeEntry],
    ) -> Result<ScopeResult, PluginError> {
        Ok(ScopeResult {
            keep: None,
            rows: Vec::new(),
        })
    }

    fn classify(&mut self, pairs: &[Pair]) -> Result<Vec<&'static str>, PluginError> {
        Ok(pairs
            .iter()
            .map(
                |pair| match (is_test_file(&pair.a.path), is_test_file(&pair.b.path)) {
                    (true, true) => "test",
                    (false, false) => "product",
                    _ => "mixed",
                },
            )
            .collect())
    }
}

/// パスを `/` で先に分けないのは，`/` も英字と数字でない文字として語を区切り，分けても語が変わらないからである．
/// UTF-8 の文字を見ずにバイトで見るのは，ASCII でないバイトも語の区切りにするためである
fn is_test_file(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut start = 0;
    for i in 0..=bytes.len() {
        let next = bytes.get(i);
        let camel = next.is_some_and(u8::is_ascii_uppercase)
            && i > 0
            && (bytes[i - 1].is_ascii_lowercase() || bytes[i - 1].is_ascii_digit());
        if camel || !next.is_some_and(u8::is_ascii_alphanumeric) {
            if bytes[start..i].eq_ignore_ascii_case(b"test") {
                return true;
            }
            start = if camel { i } else { i + 1 };
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::Fragment;

    #[test]
    fn test_words_follow_the_scam_rule() {
        for path in [
            "src/test/java/Foo.java",
            "src/main/java/FooTest.java",
            "TestUtils.java",
            "a/b/test",
            "x/my-test-data/A.java",
            "fooTEST.java",
            "Foo1Test.java",
            "src/TEST/A.java",
            "日本test/A.java",
        ] {
            assert!(is_test_file(path), "{path}");
        }
        for path in [
            "tests/A.java",
            "testing/A.java",
            "latest/A.java",
            "src/FooIT.java",
            "src/Testing.java",
            "contest.java",
            "TESTData.java",
            "",
        ] {
            assert!(!is_test_file(path), "{path}");
        }
    }

    #[test]
    fn pairs_get_test_product_or_mixed() {
        let fragment = |path: &str| Fragment {
            path: path.into(),
            start: 1,
            end: 2,
        };
        let pair = |a: &str, b: &str| Pair {
            a: fragment(a),
            b: fragment(b),
        };
        let labels = TestPath
            .classify(&[
                pair("src/test/A.java", "src/test/B.java"),
                pair("src/main/A.java", "src/main/B.java"),
                pair("src/main/A.java", "src/test/B.java"),
            ])
            .unwrap();
        assert_eq!(labels, ["test", "product", "mixed"]);
    }

    #[test]
    fn items_and_timeout_are_refused() {
        let section = |rest: &str, timeout: Option<u64>| PluginSection {
            name: "test_path".to_string(),
            timeout_secs: timeout.and_then(std::num::NonZeroU64::new),
            rest: toml::from_str(rest).unwrap(),
        };
        assert!(TestPath::new(&section("", None)).is_ok());
        let err = TestPath::new(&section("words = 1", None)).err().unwrap();
        assert_eq!(
            err.to_string(),
            "[classifiers.test_path] unknown item `words` for test_path"
        );
        let err = TestPath::new(&section("", Some(5))).err().unwrap();
        assert!(
            err.to_string().contains("timeout_secs is not allowed"),
            "{err}"
        );
    }
}
