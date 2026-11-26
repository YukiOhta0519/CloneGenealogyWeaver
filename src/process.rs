use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rustix::process::{Pid, Signal};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use crate::error::{Interrupted, PluginError};

const STDERR_TAIL: usize = 4096;
const POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Default)]
pub struct Interrupt(Arc<AtomicBool>);

impl Interrupt {
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_set() {
            Err(Interrupted.into())
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// 信号ですぐに終わらせずに印を立てるだけにするのは，版の途中なら，その版のトランザクションを
/// 戻してから止めるためである．
pub fn install_signal_flags() -> Result<Interrupt> {
    let interrupt = Interrupt::default();
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&interrupt.0))
            .with_context(|| format!("cannot register the handler of signal {signal}"))?;
    }
    Ok(interrupt)
}

pub struct ProgramSpec<'a> {
    pub program: &'a Path,
    pub args: &'a [OsString],
    pub temp_dir: &'a Path,
    /// 長さでなく締め切りの時刻にするのは，1 回の呼び出しで複数のプログラムを動かすときに，
    /// 合計に上限を当てるためである．
    pub deadline: Option<Instant>,
    pub capture_stdout: bool,
    pub watch: &'a [&'a str],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgramEnd {
    Exited(i32),
    Signaled(i32),
    TimedOut,
}

#[derive(Debug)]
pub struct ProgramOutput {
    pub end: ProgramEnd,
    pub stdout: Vec<u8>,
    pub stderr_tail: Vec<u8>,
    pub watched: Vec<bool>,
}

/// 時間切れを PluginError にしないのは，失敗の理由の文をプラグインが組み立てるからである．
pub fn run_program(
    spec: &ProgramSpec,
    interrupt: &Interrupt,
) -> Result<ProgramOutput, PluginError> {
    let mut command = Command::new(spec.program);
    command
        .args(spec.args)
        .current_dir(spec.temp_dir)
        .env_clear()
        .env("LC_ALL", "C.UTF-8")
        .env("TMPDIR", spec.temp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let parent = rustix::process::getpid();
    // SAFETY: fork の後の子で呼ぶのは，確保もロックもしない 2 つのシステムコールだけである
    unsafe {
        command.pre_exec(move || die_with_parent(parent));
    }
    // 別のスレッドで起動しないのは，親が死んだら届く信号が，起動したスレッドが終わったときにも届くからである
    let mut child = command.spawn().map_err(|err| PluginError::Failed {
        reason: format!("cannot start {}: {err}", spec.program.display()),
    })?;
    let mut group = Group::of(&child);

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    // 読まずに待たないのは，出力がたまるとプログラムが止まるからである
    let stdout_reader = read_in_thread(stdout, spec.capture_stdout, spec.watch, None);
    let stderr_reader = read_in_thread(stderr, false, spec.watch, Some(STDERR_TAIL));
    let waiter = wait_in_thread(child);

    let end = loop {
        match waiter.recv_timeout(POLL) {
            Ok(status) => break end_of(status.map_err(abort)?),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(abort(io::Error::other("the waiting thread stopped")));
            }
        }
        if interrupt.is_set() {
            group.kill();
            let _ = waiter.recv();
            group.forget();
            let _ = join(stdout_reader);
            let _ = join(stderr_reader);
            return Err(PluginError::Abort(Interrupted.into()));
        }
        if spec
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            group.kill();
            let _ = waiter.recv();
            break ProgramEnd::TimedOut;
        }
    };
    group.kill();
    group.forget();

    let out = join(stdout_reader)?;
    let err = join(stderr_reader)?;
    let watched = out
        .found
        .iter()
        .zip(&err.found)
        .map(|(a, b)| *a || *b)
        .collect();
    Ok(ProgramOutput {
        end,
        stdout: out.kept,
        stderr_tail: err.kept,
        watched,
    })
}

fn die_with_parent(parent: Pid) -> io::Result<()> {
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL))?;
    // 親を確かめ直すのは，設定する前に親が死んでいたら信号が届かないからである
    if rustix::process::getppid() != Some(parent) {
        return Err(rustix::io::Errno::SRCH.into());
    }
    Ok(())
}

/// 起動したプロセスグループ．捨てるときに止めるのは，誤りや panic で途中から抜けたときも，
/// プログラムを残さないためである．
struct Group(Option<Pid>);

impl Group {
    fn of(child: &Child) -> Group {
        Group(Pid::from_raw(child.id() as i32))
    }

    /// 猶予を置かないのは，プラグインの外部のプログラムに，止まる前に片づけることが無いからである．
    fn kill(&self) {
        if let Some(pid) = self.0 {
            let _ = rustix::process::kill_process_group(pid, Signal::KILL);
        }
    }

    /// 止め終えた後に番号を持ち続けないのは，同じ番号が別のプロセスグループに使い回されうるからである
    fn forget(&mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

fn wait_in_thread(mut child: Child) -> mpsc::Receiver<io::Result<ExitStatus>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(child.wait());
    });
    receiver
}

fn end_of(status: ExitStatus) -> ProgramEnd {
    match (status.code(), status.signal()) {
        (Some(code), _) => ProgramEnd::Exited(code),
        (None, Some(signal)) => ProgramEnd::Signaled(signal),
        (None, None) => unreachable!("a process ends with an exit code or a signal on Linux"),
    }
}

struct Stream {
    kept: Vec<u8>,
    found: Vec<bool>,
}

fn read_in_thread(
    mut reader: impl Read + Send + 'static,
    keep_all: bool,
    watch: &[&str],
    tail: Option<usize>,
) -> JoinHandle<io::Result<Stream>> {
    let mut watcher = Watcher::new(watch);
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let n = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            let chunk = &buffer[..n];
            watcher.feed(chunk);
            if keep_all || tail.is_some() {
                kept.extend_from_slice(chunk);
            }
            if let Some(limit) = tail
                && kept.len() > limit
            {
                kept.drain(..kept.len() - limit);
            }
        }
        Ok(Stream {
            kept,
            found: watcher.found,
        })
    })
}

fn join(reader: JoinHandle<io::Result<Stream>>) -> Result<Stream, PluginError> {
    match reader.join() {
        Ok(result) => result.map_err(abort),
        Err(_) => Err(abort(io::Error::other("the reading thread panicked"))),
    }
}

fn abort(err: io::Error) -> PluginError {
    PluginError::Abort(anyhow!(err).context("cannot run the external program"))
}

/// 末尾だけから探さないのは，MSCCD の OutOfMemoryError が末尾の 4,096 バイトより前に出ることがあるからである．
struct Watcher {
    needles: Vec<Vec<u8>>,
    found: Vec<bool>,
    /// 前の読み取りの末尾を残すのは，読み取りの境目をまたいだ文字列も見つけるためである
    carry: Vec<u8>,
    keep: usize,
}

impl Watcher {
    fn new(watch: &[&str]) -> Watcher {
        let needles: Vec<Vec<u8>> = watch.iter().map(|w| w.as_bytes().to_vec()).collect();
        let found = needles.iter().map(Vec::is_empty).collect();
        let keep = needles
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        Watcher {
            needles,
            found,
            carry: Vec::new(),
            keep,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        if self.found.iter().all(|f| *f) {
            return;
        }
        let mut window = std::mem::take(&mut self.carry);
        window.extend_from_slice(chunk);
        for (needle, found) in self.needles.iter().zip(&mut self.found) {
            if !*found && window.windows(needle.len()).any(|w| w == needle.as_slice()) {
                *found = true;
            }
        }
        self.carry = window[window.len().saturating_sub(self.keep)..].to_vec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str], deadline: Option<Duration>, watch: &[&str]) -> ProgramOutput {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let dir = std::env::temp_dir();
        let spec = ProgramSpec {
            program: Path::new("/bin/sh"),
            args: &args,
            temp_dir: &dir,
            deadline: deadline.map(|d| Instant::now() + d),
            capture_stdout: true,
            watch,
        };
        run_program(&spec, &Interrupt::default()).unwrap()
    }

    fn stdout(output: &ProgramOutput) -> String {
        String::from_utf8(output.stdout.clone()).unwrap()
    }

    /// ゾンビのまま残る間も止まったとみなすのは，止めた後に親が回収するまでの時間があるからである
    fn is_gone(pid: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return true,
                Ok(stat) if stat.contains(") Z ") => return true,
                Ok(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
        false
    }

    #[test]
    fn check_reports_interrupted_after_the_flag_is_set() {
        let interrupt = Interrupt::default();
        assert!(interrupt.check().is_ok());
        interrupt.clone().set();
        let err = interrupt.check().unwrap_err();
        assert!(err.is::<Interrupted>());
    }

    #[test]
    fn the_exit_code_and_stdout_come_back_and_the_work_dir_is_the_temp_dir() {
        let output = run(&["-c", "pwd; echo err >&2; exit 3"], None, &[]);
        assert_eq!(output.end, ProgramEnd::Exited(3));
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert_eq!(stdout(&output), format!("{}\n", dir.display()));
        assert_eq!(output.stderr_tail, b"err\n");
    }

    #[test]
    fn only_path_lc_all_and_tmpdir_are_passed() {
        let dir = std::env::temp_dir();
        let spec = ProgramSpec {
            program: Path::new("/usr/bin/env"),
            args: &[],
            temp_dir: &dir,
            deadline: None,
            capture_stdout: true,
            watch: &[],
        };
        let output = run_program(&spec, &Interrupt::default()).unwrap();
        let mut names: Vec<String> = stdout(&output)
            .lines()
            .map(|line| line.split('=').next().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["LC_ALL", "PATH", "TMPDIR"]);
        assert!(stdout(&output).contains("LC_ALL=C.UTF-8\n"));
        assert!(stdout(&output).contains(&format!("TMPDIR={}\n", dir.display())));
    }

    #[test]
    fn stdin_is_empty() {
        let output = run(&["-c", "cat"], Some(Duration::from_secs(10)), &[]);
        assert_eq!(output.end, ProgramEnd::Exited(0));
        assert!(output.stdout.is_empty());
    }

    #[test]
    fn stdout_is_dropped_unless_captured() {
        let dir = std::env::temp_dir();
        let args = [OsString::from("-c"), OsString::from("echo out")];
        let spec = ProgramSpec {
            program: Path::new("/bin/sh"),
            args: &args,
            temp_dir: &dir,
            deadline: None,
            capture_stdout: false,
            watch: &["out"],
        };
        let output = run_program(&spec, &Interrupt::default()).unwrap();
        assert!(output.stdout.is_empty());
        assert_eq!(output.watched, [true]);
    }

    #[test]
    fn stderr_keeps_the_last_4096_bytes() {
        let output = run(
            &[
                "-c",
                "head -c 10000 /dev/zero | tr '\\0' a >&2; printf b >&2",
            ],
            None,
            &[],
        );
        assert_eq!(output.stderr_tail.len(), 4096);
        assert_eq!(output.stderr_tail.last(), Some(&b'b'));
        assert!(output.stderr_tail[..4095].iter().all(|&c| c == b'a'));
    }

    #[test]
    fn watched_strings_are_found_in_both_streams_and_before_the_tail() {
        let output = run(
            &[
                "-c",
                "echo OutOfMemoryError >&2; head -c 20000 /dev/zero | tr '\\0' x >&2; \
                 echo Keywords filter unavaliable",
            ],
            None,
            &["OutOfMemoryError", "Keywords filter unavaliable", "absent"],
        );
        assert_eq!(output.watched, [true, true, false]);
    }

    #[test]
    fn watched_strings_are_found_across_reads() {
        let mut watcher = Watcher::new(&["abcdef", "zz", ""]);
        watcher.feed(b"xxabc");
        watcher.feed(b"d");
        assert_eq!(watcher.found, [false, false, true]);
        watcher.feed(b"efz");
        watcher.feed(b"z");
        assert_eq!(watcher.found, [true, true, true]);
    }

    #[test]
    fn a_signal_ends_the_program() {
        let output = run(&["-c", "kill -TERM $$"], None, &[]);
        assert_eq!(output.end, ProgramEnd::Signaled(15));
    }

    #[test]
    fn the_deadline_kills_the_whole_group() {
        let start = Instant::now();
        let output = run(
            &["-c", "sleep 100 & echo $!; sleep 100"],
            Some(Duration::from_millis(300)),
            &[],
        );
        assert_eq!(output.end, ProgramEnd::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(is_gone(stdout(&output).trim()));
    }

    #[test]
    fn processes_left_after_a_normal_exit_are_killed() {
        let output = run(&["-c", "sleep 100 & echo $!; exit 0"], None, &[]);
        assert_eq!(output.end, ProgramEnd::Exited(0));
        assert!(is_gone(stdout(&output).trim()));
    }

    #[test]
    fn an_interrupt_kills_the_program_and_returns_interrupted() {
        let interrupt = Interrupt::default();
        let flag = interrupt.clone();
        let setter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            flag.set();
        });
        let dir = std::env::temp_dir();
        let args = [OsString::from("-c"), OsString::from("sleep 100")];
        let spec = ProgramSpec {
            program: Path::new("/bin/sh"),
            args: &args,
            temp_dir: &dir,
            deadline: None,
            capture_stdout: false,
            watch: &[],
        };
        let start = Instant::now();
        match run_program(&spec, &interrupt) {
            Err(PluginError::Abort(err)) => assert!(err.is::<Interrupted>()),
            other => panic!("expected an interrupt, got {other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        setter.join().unwrap();
    }

    #[test]
    fn a_program_that_cannot_start_is_a_plugin_failure() {
        let dir = std::env::temp_dir();
        let spec = ProgramSpec {
            program: Path::new("/nonexistent/cgweave-test"),
            args: &[],
            temp_dir: &dir,
            deadline: None,
            capture_stdout: false,
            watch: &[],
        };
        match run_program(&spec, &Interrupt::default()) {
            Err(PluginError::Failed { reason }) => {
                assert!(
                    reason.starts_with("cannot start /nonexistent/cgweave-test: "),
                    "{reason}"
                )
            }
            other => panic!("expected a plugin failure, got {other:?}"),
        }
    }
}
