use std::collections::HashMap;

use anyhow::{Result, bail};

const SECONDS_PER_DAY: f64 = 86_400.0;

/// 版をグラフの順に並べて渡す．親を先に求めておけるのは，グラフの順で親が子より前にあるからである
pub fn effective_times(
    order: &[(i64, i64)],
    parent: &HashMap<i64, i64>,
) -> Result<HashMap<i64, i64>> {
    let mut effective = HashMap::with_capacity(order.len());
    for &(version, committed) in order {
        let time = match parent.get(&version) {
            None => committed,
            Some(p) => match effective.get(p) {
                Some(&t) => committed.max(t),
                None => {
                    bail!("the record is broken: version {version} comes before its parent {p}")
                }
            },
        };
        effective.insert(version, time);
    }
    Ok(effective)
}

pub fn days(birth: i64, end: i64) -> f64 {
    (end - birth) as f64 / SECONDS_PER_DAY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_earlier_commit_after_its_parent_takes_the_parent_time() {
        let order = [(1, 100), (2, 50), (3, 300), (4, 10)];
        let parent = HashMap::from([(2, 1), (3, 2), (4, 1)]);
        let effective = effective_times(&order, &parent).unwrap();
        assert_eq!(effective[&1], 100);
        assert_eq!(effective[&2], 100);
        assert_eq!(effective[&3], 300);
        assert_eq!(effective[&4], 100);
    }

    #[test]
    fn a_child_before_its_parent_is_refused() {
        let err = effective_times(&[(2, 0), (1, 0)], &HashMap::from([(2, 1)])).unwrap_err();
        assert!(err.to_string().contains("comes before its parent"), "{err}");
    }

    #[test]
    fn days_are_seconds_over_86400() {
        assert_eq!(days(0, 86_400 * 3), 3.0);
        assert_eq!(days(10, 10 + 43_200), 0.5);
        assert_eq!(days(5, 5), 0.0);
    }
}
