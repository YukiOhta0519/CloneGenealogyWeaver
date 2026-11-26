// 割り込みを別の戻り値にしないのは，止める誤りと同じく anyhow::Error として `?` で上に返せるからである．

use std::fmt;
use std::process::ExitCode;

#[derive(Debug)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// anyhow::Error だけにしないのは，やり直すプラグインの失敗と，そのまま上に返す誤りを
/// 呼び手が型で分けるためである．
#[derive(Debug)]
pub enum PluginError {
    Failed { reason: String },
    Abort(anyhow::Error),
}

pub fn exit_code(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let code = code_for(&err);
            if code == 130 {
                eprintln!("interrupted");
            } else {
                eprintln!("error: {}", one_line(&format!("{err:#}")));
            }
            ExitCode::from(code)
        }
    }
}

fn code_for(err: &anyhow::Error) -> u8 {
    if err.chain().any(|cause| cause.is::<Interrupted>()) {
        130
    } else {
        1
    }
}

/// 改行を残さないのは，理由の文に改行が混ざっても，誤りを 1 行で出すためである．
pub fn one_line(text: &str) -> String {
    text.split(['\n', '\r'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, anyhow};

    #[test]
    fn interrupted_gives_130_even_under_context() {
        let err = anyhow::Error::new(Interrupted).context("while recording version 3");
        assert_eq!(code_for(&err), 130);
    }

    #[test]
    fn other_errors_give_1() {
        let err = Err::<(), _>(anyhow!("disk full"))
            .context("cannot write")
            .unwrap_err();
        assert_eq!(code_for(&err), 1);
    }

    #[test]
    fn messages_become_one_line() {
        assert_eq!(one_line("a\nb\r\n  c  \n"), "a b c");
    }
}
