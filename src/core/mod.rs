// 1 から数える値と 0 から数える値を直す場所を，Fragment::lines，Fragment::from_lines，
// line_events の入り口の 3 か所に限るのは，1 ずれる誤りを探す場所を減らすためである．

pub mod detection;
pub mod file_mapping;
pub mod fragment_events;
pub mod fragment_mapping;
pub mod line_events;
pub mod lines;
pub mod model;
pub mod pair_events;
pub mod similarity;
#[cfg(test)]
pub mod testing;
