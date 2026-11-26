pub const TOOL_VERSION: &str = env!("CGWEAVE_VERSION");

pub const EVENTS_SCHEMA_VERSION: i64 = 1;

pub const GENEALOGY_SCHEMA_VERSION: i64 = 1;

pub const DETECTORS: &[&str] = &["fixture", "msccd"];

pub const CLASSIFIERS: &[&str] = &["claim", "fixture", "generated", "test_path", "vendor"];

/// 上の 2 つの一覧から作らずに手で写すのは，clap には固定の文字列しか渡せないからである．
/// 食い違いはテストで見つける．
pub const VERSION_TEXT: &str = concat!(
    env!("CGWEAVE_VERSION"),
    "\ndetectors: fixture, msccd",
    "\nclassifiers: claim, fixture, generated, test_path, vendor"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_text_lists_the_built_in_plugins() {
        let expected = format!(
            "{TOOL_VERSION}\ndetectors: {}\nclassifiers: {}",
            DETECTORS.join(", "),
            CLASSIFIERS.join(", ")
        );
        assert_eq!(VERSION_TEXT, expected);
    }

    #[test]
    fn plugin_names_are_sorted() {
        assert!(DETECTORS.is_sorted());
        assert!(CLASSIFIERS.is_sorted());
    }
}
