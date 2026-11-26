mod cli;
mod config;
mod core;
mod error;
mod genealogy;
mod git;
mod graph;
mod pairs;
mod plugin;
mod process;
mod record;
mod status;
mod store;
mod version;

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> ExitCode {
    let interrupt = match process::install_signal_flags() {
        Ok(interrupt) => interrupt,
        Err(err) => return error::exit_code(Err(err)),
    };
    let cli = Cli::parse();
    init_logger(cli.log_level.filter());
    let result = match cli.command {
        Command::Record(args) => record::run(args, &interrupt),
        Command::Pairs(args) => pairs::run(args, &interrupt),
        Command::Genealogy(args) => genealogy::run(args, &interrupt),
        Command::Status(args) => status::run(args, &interrupt),
    };
    error::exit_code(result)
}

/// 時刻を env_logger の既定の形にしないのは，データベースの時刻と同じ書き方にするためである．
fn init_logger(level: log::LevelFilter) {
    env_logger::Builder::new()
        .filter_level(level)
        .target(env_logger::Target::Stderr)
        .format(|buf, record| {
            writeln!(buf, "{} {} {}", store::now(), record.level(), record.args())
        })
        .init();
}
