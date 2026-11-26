// 短い名前の引数を足さないのは，引数を全部名前付きにすると決めたからである．

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::version::VERSION_TEXT;

#[derive(Parser)]
#[command(
    name = "cgweave",
    version = VERSION_TEXT,
    propagate_version = true,
    about = "Record Type-3 clone pair genealogy events from a Git history"
)]
pub struct Cli {
    /// Log level
    #[arg(long, global = true, value_enum, default_value_t = LogLevel::Info)]
    pub log_level: LogLevel,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn filter(self) -> log::LevelFilter {
        match self {
            LogLevel::Error => log::LevelFilter::Error,
            LogLevel::Warn => log::LevelFilter::Warn,
            LogLevel::Info => log::LevelFilter::Info,
            LogLevel::Debug => log::LevelFilter::Debug,
            LogLevel::Trace => log::LevelFilter::Trace,
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Detect clones version by version and record the events into a database
    Record(RecordArgs),
    /// Print the clone pairs of one recorded version as JSON Lines
    Pairs(PairsArgs),
    /// Build the genealogy file from a finished event database
    Genealogy(GenealogyArgs),
    /// Print the progress of a record and how to resume it
    Status(StatusArgs),
}

#[derive(Args)]
pub struct RecordArgs {
    /// Repository to analyze: a work tree root or a bare repository
    #[arg(long)]
    pub repo: PathBuf,
    /// Graph file: one version per line, followed by its parents
    #[arg(long)]
    pub graph: PathBuf,
    /// Configuration file in TOML
    #[arg(long)]
    pub config: PathBuf,
    /// Event database to create, or to resume with --resume
    #[arg(long)]
    pub db: PathBuf,
    /// Where to put the analysis clone [default: <db>.clone]
    #[arg(long)]
    pub clone_dir: Option<PathBuf>,
    /// Version to resume from
    #[arg(long)]
    pub resume: Option<String>,
}

#[derive(Args)]
pub struct PairsArgs {
    /// Event database
    #[arg(long)]
    pub db: PathBuf,
    /// Recorded version whose pairs to print
    #[arg(long)]
    pub commit: String,
}

#[derive(Args)]
pub struct GenealogyArgs {
    /// Finished event database
    #[arg(long)]
    pub db: PathBuf,
    /// Genealogy file to create
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Event database
    #[arg(long)]
    pub db: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap が定義の食い違いを見つけると，起動したときに panic する．
    /// `pairs` の版の引数と共通の `--version` の衝突のような誤りを，テストの時点で見つける．
    #[test]
    fn the_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn log_level_can_follow_the_command() {
        let cli =
            Cli::try_parse_from(["cgweave", "status", "--db", "e.db", "--log-level", "debug"])
                .unwrap();
        assert!(matches!(cli.log_level, LogLevel::Debug));
    }
}
