use std::collections::BTreeMap;
use std::io::{BufWriter, ErrorKind, Write};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::PairsArgs;
use crate::process::Interrupt;
use crate::store;
use crate::store::reconstruct::reconstruct;

/// serde_json の Map にしないのは，片の項目を path，start，end の順に書くためである
#[derive(Serialize)]
struct Line<'a> {
    a: Fragment<'a>,
    b: Fragment<'a>,
    labels: BTreeMap<&'a str, &'a str>,
    similarity: Option<f64>,
}

#[derive(Serialize)]
struct Fragment<'a> {
    path: &'a str,
    start: u32,
    end: u32,
}

pub fn run(args: PairsArgs, interrupt: &Interrupt) -> Result<()> {
    let rebuilt = store::read_snapshot(&args.db, |tx| {
        let version = store::find_version(tx, &args.commit)?;
        match version.status.as_str() {
            "ok" => reconstruct(tx, version.id, interrupt),
            "broken" => bail!("version {} is broken, so it has no pairs", version.commit),
            _ => bail!("version {} is not recorded yet", version.commit),
        }
    })?;
    // 1 行ずつ OS に渡さないのは，遅いからである．
    let mut out = BufWriter::new(std::io::stdout().lock());
    for (pair, recorded) in &rebuilt.pairs {
        interrupt.check()?;
        let line = Line {
            a: fragment(&pair.a),
            b: fragment(&pair.b),
            labels: rebuilt
                .labelers
                .iter()
                .map(String::as_str)
                .zip(recorded.labels.iter().map(String::as_str))
                .collect(),
            similarity: recorded.similarity,
        };
        let mut text = serde_json::to_string(&line)?;
        text.push('\n');
        if let Err(err) = out.write_all(text.as_bytes()) {
            return closed_or(err);
        }
    }
    match out.flush() {
        Ok(()) => Ok(()),
        Err(err) => closed_or(err),
    }
}

fn fragment(f: &crate::core::model::Fragment) -> Fragment<'_> {
    Fragment {
        path: &f.path,
        start: f.start,
        end: f.end,
    }
}

/// 読む側が先に閉じたのを誤りにしないのは，`cgweave pairs … | head` を普通の使い方として受け付けるためである．
fn closed_or(err: std::io::Error) -> Result<()> {
    if err.kind() == ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(err.into())
    }
}
