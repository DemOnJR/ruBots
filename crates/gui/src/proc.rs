//! Child-process supervisor: the bots, the docker test server, cargo builds.
//!
//! The GUI launches `capture_running` directly rather than shelling out to
//! `scripts/swarm.ps1`, because a script that owns the children can only be
//! stopped as a whole — here every bot is a tracked child with its own pid,
//! lifetime and log stream, so one can be killed without touching the others.
//!
//! Two rules from the field are encoded here rather than left to the operator:
//!
//! * **Launches are staggered** (`Config::stagger_ms`). A mass connect from one
//!   IP trips ReAuthCheck / ReChecker rate limits — see the swarm scripts.
//! * **Stops are staggered too**, for the same reason: seven disconnects inside
//!   15 s is the documented ban trigger.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// What a managed process is, which decides how it is shown and stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Bot,
    Server,
    Build,
    Tool,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::Server => "server",
            Self::Build => "build",
            Self::Tool => "tool",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Warn,
    Error,
    /// Written by the shell itself, not by a child.
    Meta,
}

#[derive(Clone, Debug)]
pub struct LogLine {
    /// Seconds since the shell started — the app has no clock of its own and
    /// std has no local time, so everything is relative to launch.
    pub at: f32,
    pub src: String,
    pub kind: Kind,
    pub level: Level,
    pub text: String,
}

/// A bounded log shared by every reader thread.
pub struct LogBuf {
    lines: VecDeque<LogLine>,
    cap: usize,
    /// Bumped on every push, so a view can tell whether to auto-scroll.
    pub seq: u64,
    pub errors: u64,
    start: Instant,
}

impl LogBuf {
    pub fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(cap.min(4096)),
            cap,
            seq: 0,
            errors: 0,
            start: Instant::now(),
        }
    }

    pub fn push(&mut self, src: &str, kind: Kind, level: Level, text: impl Into<String>) {
        if level == Level::Error {
            self.errors += 1;
        }
        self.lines.push_back(LogLine {
            at: self.start.elapsed().as_secs_f32(),
            src: src.to_string(),
            kind,
            level,
            text: text.into(),
        });
        while self.lines.len() > self.cap {
            self.lines.pop_front();
        }
        self.seq += 1;
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &LogLine> {
        self.lines.iter()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.errors = 0;
        self.seq += 1;
    }
}

/// Classify a child's line so the console can color it without a log format.
fn classify(text: &str) -> Level {
    let lower = text.to_ascii_lowercase();
    if lower.contains("error")
        || lower.contains("panic")
        || lower.contains("failed")
        || lower.contains("fatal")
    {
        Level::Error
    } else if lower.contains("warn") || lower.contains("timeout") || lower.contains("retry") {
        Level::Warn
    } else {
        Level::Info
    }
}

pub struct Managed {
    pub id: u32,
    pub label: String,
    pub kind: Kind,
    /// 1 = T, 2 = CT, 0 = not a bot.
    pub team: u8,
    pub pid: u32,
    pub started: Instant,
    /// `None` while running; `Some(code)` once reaped (`-1` = killed/unknown).
    pub exit: Option<i32>,
    /// When the exit was noticed, so the table can retire old rows.
    pub ended: Option<Instant>,
    child: Option<Child>,
}

impl Managed {
    pub fn running(&self) -> bool {
        self.exit.is_none()
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }
}

/// A launch that has not happened yet, held back by the stagger.
struct Pending {
    at: Instant,
    label: String,
    team: u8,
    program: PathBuf,
    args: Vec<String>,
    envs: Vec<(String, String)>,
    cwd: PathBuf,
}

pub struct Supervisor {
    pub procs: Vec<Managed>,
    pub log: Arc<Mutex<LogBuf>>,
    next_id: u32,
    pending: VecDeque<Pending>,
    pending_kill: VecDeque<(u32, Instant)>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            procs: Vec::new(),
            log: Arc::new(Mutex::new(LogBuf::new(8000))),
            next_id: 1,
            pending: VecDeque::new(),
            pending_kill: VecDeque::new(),
        }
    }

    pub fn note(&self, src: &str, level: Level, text: impl Into<String>) {
        if let Ok(mut log) = self.log.lock() {
            log.push(src, Kind::Tool, level, text);
        }
    }

    pub fn running(&self, kind: Kind) -> usize {
        self.procs
            .iter()
            .filter(|p| p.kind == kind && p.running())
            .count()
    }

    pub fn queued(&self) -> usize {
        self.pending.len()
    }

    /// Start a process now. Returns its id, or logs and returns `None`.
    pub fn spawn(
        &mut self,
        kind: Kind,
        label: &str,
        team: u8,
        program: &Path,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Option<u32> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: 30 bots must not open 30 consoles.
            cmd.creation_flags(0x0800_0000);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.note(
                    label,
                    Level::Error,
                    format!("spawn failed: {e} ({})", program.display()),
                );
                return None;
            }
        };

        let id = self.next_id;
        self.next_id += 1;
        let pid = child.id();

        for (stream, is_err) in [
            (child.stdout.take().map(Stream::Out), false),
            (child.stderr.take().map(Stream::Err), true),
        ] {
            let Some(stream) = stream else { continue };
            let log = Arc::clone(&self.log);
            let src = label.to_string();
            thread::spawn(move || {
                let reader: Box<dyn BufRead> = match stream {
                    Stream::Out(s) => Box::new(BufReader::new(s)),
                    Stream::Err(s) => Box::new(BufReader::new(s)),
                };
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let text = line.trim_end().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    let level = if is_err {
                        classify(&text)
                    } else {
                        Level::Info
                    };
                    if let Ok(mut log) = log.lock() {
                        log.push(&src, kind, level, text);
                    }
                }
            });
        }

        self.note(label, Level::Meta, format!("started pid {pid}"));
        self.procs.push(Managed {
            id,
            label: label.to_string(),
            kind,
            team,
            pid,
            started: Instant::now(),
            exit: None,
            ended: None,
            child: Some(child),
        });
        Some(id)
    }

    /// Queue a launch for `delay` from now (the stagger).
    #[allow(clippy::too_many_arguments)]
    pub fn queue(
        &mut self,
        delay: Duration,
        label: &str,
        team: u8,
        program: &Path,
        args: Vec<String>,
        envs: Vec<(String, String)>,
        cwd: &Path,
    ) {
        self.pending.push_back(Pending {
            at: Instant::now() + delay,
            label: label.to_string(),
            team,
            program: program.to_path_buf(),
            args,
            envs,
            cwd: cwd.to_path_buf(),
        });
    }

    pub fn cancel_queued(&mut self) -> usize {
        let n = self.pending.len();
        self.pending.clear();
        if n > 0 {
            self.note("deploy", Level::Meta, format!("cancelled {n} queued launches"));
        }
        n
    }

    pub fn stop(&mut self, id: u32) {
        if let Some(p) = self.procs.iter_mut().find(|p| p.id == id) {
            if let Some(child) = p.child.as_mut() {
                let _ = child.kill();
            }
        }
    }

    /// Stop everything of a kind, staggered so a swarm exit does not look like
    /// an attack to the server's flood protection.
    pub fn stop_all(&mut self, kind: Kind, stagger: Duration) {
        self.cancel_queued();
        let ids: Vec<u32> = self
            .procs
            .iter()
            .filter(|p| p.kind == kind && p.running())
            .map(|p| p.id)
            .collect();
        let now = Instant::now();
        for (i, id) in ids.iter().enumerate() {
            self.pending_kill.push_back((*id, now + stagger * i as u32));
        }
        if !ids.is_empty() {
            self.note(
                "deploy",
                Level::Meta,
                format!(
                    "stopping {} {} process(es), {} ms apart",
                    ids.len(),
                    kind.label(),
                    stagger.as_millis()
                ),
            );
        }
    }

    /// Kill immediately, no stagger — used on window close.
    pub fn kill_everything(&mut self) {
        self.pending.clear();
        self.pending_kill.clear();
        for p in &mut self.procs {
            if let Some(child) = p.child.as_mut() {
                let _ = child.kill();
            }
        }
    }

    /// Per-frame housekeeping: due launches, due kills, and reaping.
    pub fn poll(&mut self) {
        let now = Instant::now();

        while self.pending.front().is_some_and(|p| p.at <= now) {
            let p = self.pending.pop_front().expect("checked");
            self.spawn(
                Kind::Bot,
                &p.label,
                p.team,
                &p.program,
                &p.args,
                &p.envs,
                &p.cwd,
            );
        }

        while self.pending_kill.front().is_some_and(|(_, at)| *at <= now) {
            let (id, _) = self.pending_kill.pop_front().expect("checked");
            self.stop(id);
        }

        let mut finished: Vec<(String, Kind, i32)> = Vec::new();
        for p in &mut self.procs {
            if p.exit.is_some() {
                continue;
            }
            let Some(child) = p.child.as_mut() else {
                continue;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code().unwrap_or(-1);
                    p.exit = Some(code);
                    p.ended = Some(now);
                    p.child = None;
                    finished.push((p.label.clone(), p.kind, code));
                }
                Ok(None) => {}
                Err(_) => {
                    p.exit = Some(-1);
                    p.ended = Some(now);
                    p.child = None;
                }
            }
        }
        for (label, kind, code) in finished {
            let level = if code == 0 { Level::Meta } else { Level::Warn };
            if let Ok(mut log) = self.log.lock() {
                log.push(&label, kind, level, format!("exited with code {code}"));
            }
        }

        // Keep the table honest: a finished process stays visible for five
        // minutes (long enough to read its exit code), then is retired.
        self.procs.retain(|p| {
            p.running()
                || p.ended
                    .is_none_or(|at| at.elapsed() < Duration::from_secs(300))
        });
    }
}

/// Reader-thread plumbing: one type so both pipes take the same code path.
enum Stream {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.kill_everything();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_is_bounded_and_counts_errors() {
        let mut log = LogBuf::new(4);
        for i in 0..10 {
            log.push("t", Kind::Bot, Level::Info, format!("line {i}"));
        }
        log.push("t", Kind::Bot, Level::Error, "connect failed");
        assert_eq!(log.len(), 4);
        assert_eq!(log.errors, 1);
        assert_eq!(log.iter().last().map(|l| l.text.as_str()), Some("connect failed"));
    }

    #[test]
    fn lines_are_classified_without_a_log_format() {
        assert_eq!(classify("connect failed: refused"), Level::Error);
        assert_eq!(classify("retry in 2s"), Level::Warn);
        assert_eq!(classify("entering game"), Level::Info);
    }
}
