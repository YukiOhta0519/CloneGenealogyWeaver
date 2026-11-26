use std::io::{BufWriter, ErrorKind, Write};

use anyhow::Result;
use rusqlite::OptionalExtension;

use crate::cli::StatusArgs;
use crate::process::Interrupt;
use crate::store;

pub fn run(args: StatusArgs, interrupt: &Interrupt) -> Result<()> {
    let lines = store::read_snapshot(&args.db, |tx| {
        let (status, stop_message, repository): (String, Option<String>, String) = tx.query_row(
            "SELECT status, stop_message, repository FROM run",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let count = |status: &str| -> rusqlite::Result<i64> {
            tx.query_row(
                "SELECT count(*) FROM version WHERE status = ?1",
                [status],
                |row| row.get(0),
            )
        };
        let (ok, broken, pending) = (count("ok")?, count("broken")?, count("pending")?);
        let next: Option<String> = tx
            .query_row(
                "SELECT commit_id FROM version WHERE status = 'pending' ORDER BY topo_order LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let mut lines = vec![format!("status: {status}")];
        // ロックを試して見分けないのは，その瞬間に始めた record がロックを取れずに止まるからである．
        if status == "running" {
            lines.push(
                "note: the record may still be running, or it stopped by an interrupt or a crash"
                    .to_string(),
            );
        }
        lines.push(format!(
            "versions: {} total, {ok} ok, {broken} broken, {pending} pending",
            ok + broken + pending
        ));
        if let Some(message) = stop_message {
            lines.push(format!("stopped: {message}"));
        }
        if let Some(next) = next {
            lines.push(format!("next: {next}"));
            // グラフと設定のパスを書かないのは，記録に残っておらず，中身のハッシュと中身だけがあるからである．
            lines.push(format!(
                "resume: cgweave record --repo {repository} --graph <graph> --config <config> --db {} --resume {next}",
                args.db.display()
            ));
        }
        Ok(lines)
    })?;
    let mut out = BufWriter::new(std::io::stdout().lock());
    for line in lines {
        interrupt.check()?;
        if let Err(err) = writeln!(out, "{line}") {
            return closed_or(err);
        }
    }
    match out.flush() {
        Ok(()) => Ok(()),
        Err(err) => closed_or(err),
    }
}

fn closed_or(err: std::io::Error) -> Result<()> {
    if err.kind() == ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(err.into())
    }
}
