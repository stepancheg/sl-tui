use std::future;
use std::io;
use std::io::ErrorKind;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::process;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::anyhow;
use tokio::net::unix::pipe;
use tokio::process::Child;
use tokio::sync::mpsc::Sender;

use crate::diff::DiffOutput;
use crate::diff::parse_diff;
use crate::repo::repo::DiffTool;
use crate::repo::repo::command_for_tool;
use crate::repo::repo::parse_untracked_paths;
use crate::repo::repo::untracked_command_for_tool;
use crate::util::notify_pair::NotifyRx;

/// Result of a capture that may be cut short by `interrupt`.
pub(crate) enum CaptureOutcome {
    /// Child exited (or the pipe hit EOF) and output was collected.
    /// `bytes` includes an `exit status: N` line when `success` is false.
    Done { bytes: Vec<u8>, success: bool },
    /// `interrupt` fired. The child and its process group were killed.
    Interrupted,
}

/// Run `command` and return its combined stdout and stderr bytes.
/// A notification on `interrupt` kills the child and returns
/// [`CaptureOutcome::Interrupted`].
async fn capture_command_output_interruptible(
    command: &[String],
    interrupt: Option<&NotifyRx>,
) -> anyhow::Result<CaptureOutcome> {
    capture_interruptible(command, interrupt, true).await
}

/// Like [`capture_command_output_interruptible`], but stderr is discarded
/// so a NUL-separated stdout listing is not corrupted by diagnostics.
async fn capture_stdout_interruptible(
    command: &[String],
    interrupt: Option<&NotifyRx>,
) -> anyhow::Result<CaptureOutcome> {
    capture_interruptible(command, interrupt, false).await
}

async fn capture_interruptible(
    command: &[String],
    interrupt: Option<&NotifyRx>,
    combine_stderr: bool,
) -> anyhow::Result<CaptureOutcome> {
    let Some((program, args)) = command.split_first() else {
        return Err(anyhow!("empty command"));
    };
    // One pipe for stdout (and stderr when combined): the kernel keeps
    // write order.
    let (reader, writer) = io::pipe()?;
    // Own process group so an interrupt can kill grandchildren too.
    let mut child_cmd = tokio::process::Command::new(program);
    child_cmd
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .process_group(0)
        // `origin` warns and `gh` colorizes when FORCE_COLOR is set,
        // even though stdout is a pipe. Drop it so the captured diff
        // is plain text.
        .env_remove("FORCE_COLOR");
    if combine_stderr {
        let stderr_writer = writer.try_clone()?;
        child_cmd.stdout(writer).stderr(stderr_writer);
    } else {
        child_cmd.stdout(writer).stderr(Stdio::null());
    }
    let mut child = child_cmd.spawn()?;
    // Tokio drives the read end. The child keeps a blocking write end.
    let reader = pipe::Receiver::from_owned_fd(OwnedFd::from(reader))?;
    let mut out = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        tokio::select! {
            biased;
            () = wait_interrupt(interrupt) => {
                kill_process_group(&mut child).await;
                return Ok(CaptureOutcome::Interrupted);
            }
            ready = reader.readable() => {
                ready?;
                match read_ready(&reader, &mut buf, &mut out)? {
                    ReadReady::Eof => {
                        let status = child.wait().await?;
                        let success = status.success();
                        append_nonzero_status(&status, &mut out);
                        return Ok(CaptureOutcome::Done {
                            bytes: out,
                            success,
                        });
                    }
                    ReadReady::Pending => {}
                }
            }
            status = child.wait() => {
                let status = status?;
                // Do not wait for EOF: a grandchild may still hold the write end.
                // Drop the tokio registration and read whatever is already buffered.
                drain_nonblocking(reader, &mut out)?;
                let success = status.success();
                append_nonzero_status(&status, &mut out);
                return Ok(CaptureOutcome::Done {
                    bytes: out,
                    success,
                });
            }
        }
    }
}

/// What the last sent snapshot was built from. Used to skip
/// `parse_diff` and a UI update when a refresh reproduces the same
/// bytes. This does not skip the `sl`/`git` processes themselves.
struct CachedSnapshot {
    diff: CachedDiff,
    untracked: CachedUntracked,
    output: DiffOutput,
}

#[derive(Eq, PartialEq)]
enum CachedDiff {
    Bytes(Vec<u8>),
    Error(String),
}

/// `Failed` is a non-zero exit or a spawn error. Both leave the
/// untracked list empty, matching the previous loop behaviour.
#[derive(Eq, PartialEq)]
enum CachedUntracked {
    Failed,
    Ok(Vec<u8>),
}

/// What the capture loop should run.
#[derive(Clone)]
pub(crate) enum CaptureSpec {
    /// Working-copy diff (optional `rev`). Polled on an interval.
    /// Untracked files are listed with each snapshot.
    Live { tool: DiffTool, rev: Option<String> },
    /// One commit (`sl show` / `git show`). No interval poll and no
    /// untracked listing. `u`/`U` and `r` still re-run via refresh.
    Commit { tool: DiffTool, commit: String },
    /// A fixed command, such as a pull-request diff. Same once-only
    /// behavior as [`CaptureSpec::Commit`]. The context size is not
    /// part of the command.
    Fixed { command: Vec<String> },
}

/// Choices on the `d` diff-mode menu.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffViewKind {
    /// Tool default working-copy diff.
    Default,
    /// Diff introduced by the current commit (`-c .` / `show HEAD`).
    ChangeCurrent,
    /// Working copy against the previous commit (`-r prev` / `HEAD^`).
    RevPrev,
}

pub(crate) const DIFF_VIEW_KINDS: [DiffViewKind; 3] = [
    DiffViewKind::Default,
    DiffViewKind::ChangeCurrent,
    DiffViewKind::RevPrev,
];

impl DiffViewKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            DiffViewKind::Default => "diff -r .",
            DiffViewKind::ChangeCurrent => "diff -c .",
            DiffViewKind::RevPrev => "diff -r prev",
        }
    }
}

/// Status-bar name for a fixed PR (or other) command.
fn fixed_command_status(command: &[String]) -> String {
    let Some(program) = command.first() else {
        return String::from("diff");
    };
    let mut number = "";
    for arg in command.iter().rev() {
        if arg.is_empty() {
            continue;
        }
        if arg.bytes().all(|b| b.is_ascii_digit()) {
            number = arg.as_str();
            break;
        }
    }
    if number.is_empty() {
        return program.clone();
    }
    if program == "origin" {
        return format!("origin pr {number}");
    }
    if program == "gh" {
        return format!("gh pr {number}");
    }
    format!("{program} {number}")
}

/// Revision naming the current commit for `tool`.
fn current_commit_rev(tool: DiffTool) -> &'static str {
    match tool {
        DiffTool::Sl => ".",
        DiffTool::Git => "HEAD",
    }
}

/// Revision naming the parent of the current commit for `tool`.
/// Sapling's `prev` is that parent; git's equivalent is `HEAD^`.
fn prev_rev(tool: DiffTool) -> &'static str {
    match tool {
        DiffTool::Sl => "prev",
        DiffTool::Git => "HEAD^",
    }
}

impl CaptureSpec {
    pub(crate) fn for_kind(tool: DiffTool, kind: DiffViewKind) -> CaptureSpec {
        match kind {
            DiffViewKind::Default => CaptureSpec::Live { tool, rev: None },
            DiffViewKind::ChangeCurrent => CaptureSpec::Commit {
                tool,
                commit: String::from(current_commit_rev(tool)),
            },
            DiffViewKind::RevPrev => CaptureSpec::Live {
                tool,
                rev: Some(String::from(prev_rev(tool))),
            },
        }
    }

    /// Which menu row this spec is, if it is one of the three modes.
    pub(crate) fn view_kind(&self) -> Option<DiffViewKind> {
        match self {
            CaptureSpec::Live { tool: _, rev: None } => Some(DiffViewKind::Default),
            CaptureSpec::Live {
                tool,
                rev: Some(rev),
            } => {
                if rev == prev_rev(*tool) {
                    Some(DiffViewKind::RevPrev)
                } else {
                    None
                }
            }
            CaptureSpec::Commit { tool, commit } => {
                if commit == current_commit_rev(*tool) {
                    Some(DiffViewKind::ChangeCurrent)
                } else {
                    None
                }
            }
            CaptureSpec::Fixed { .. } => None,
        }
    }

    /// Whether the capture loop re-runs on the interval.
    /// Working-copy diffs are live. A commit named by a symbolic
    /// rev (`.` / `HEAD`) can move or be amended, so that is live
    /// too. A digest or a PR diff is not.
    pub(crate) fn is_live(&self) -> bool {
        match self {
            CaptureSpec::Live { .. } => true,
            CaptureSpec::Commit { tool, commit } => commit == current_commit_rev(*tool),
            CaptureSpec::Fixed { .. } => false,
        }
    }

    /// Short name of this spec (no refresh marker).
    pub(crate) fn mode_status(&self) -> String {
        if let Some(kind) = self.view_kind() {
            return String::from(kind.label());
        }
        match self {
            CaptureSpec::Live {
                tool,
                rev: Some(rev),
            } => match tool {
                DiffTool::Sl => format!("diff -r {rev}"),
                DiffTool::Git => format!("diff {rev}"),
            },
            CaptureSpec::Commit { tool, commit } => match tool {
                DiffTool::Sl => format!("diff -c {commit}"),
                DiffTool::Git => format!("show {commit}"),
            },
            CaptureSpec::Fixed { command } => fixed_command_status(command),
            CaptureSpec::Live { rev: None, .. } => String::from("diff -r ."),
        }
    }

    /// Right-hand status-bar mode. Live specs append `(R)`.
    pub(crate) fn status_mode_text(&self) -> String {
        let base = self.mode_status();
        if self.is_live() {
            format!("{base} (R)")
        } else {
            base
        }
    }

    pub(crate) fn command(&self, context: u32) -> Vec<String> {
        match self {
            CaptureSpec::Live { tool, rev } => {
                command_for_tool(*tool, context, rev.as_deref(), None)
            }
            CaptureSpec::Commit { tool, commit } => {
                command_for_tool(*tool, context, None, Some(commit.as_str()))
            }
            CaptureSpec::Fixed { command } => command.clone(),
        }
    }
}

/// Capture in a loop, sending each snapshot on `tx`.
/// `refresh` kills the current command and starts the next run at once.
pub(crate) async fn capture_command_output_loop(
    shared_spec: Arc<Mutex<CaptureSpec>>,
    context: Arc<AtomicU32>,
    interval: Duration,
    tx: Sender<DiffOutput>,
    refresh: NotifyRx,
) {
    // A fixed commit or PR diff is not polled, and untracked files
    // are not listed for those. `u`/`U` and `r` still re-run via
    // `refresh`. `.` / `HEAD` commit views are live. The spec can
    // change under `d`; re-read it each run.
    let mut cached: Option<CachedSnapshot> = None;
    // An explicit refresh (mode switch, `r`, context change) may have
    // replaced the on-screen diff with a loading line. The next
    // completed snapshot must be sent even when the bytes match the
    // cache; otherwise that loading line never clears.
    let mut force_send = false;
    loop {
        let spec = shared_spec.lock().unwrap().clone();
        let live = spec.is_live();
        let command = spec.command(context.load(Ordering::SeqCst));
        let outcome = capture_command_output_interruptible(&command, Some(&refresh)).await;
        let diff = match outcome {
            Ok(CaptureOutcome::Interrupted) => {
                force_send = true;
                continue;
            }
            Ok(CaptureOutcome::Done { bytes, .. }) => CachedDiff::Bytes(bytes),
            Err(err) => {
                let text = format!("error: {err:?}");
                CachedDiff::Error(text)
            }
        };
        // A failed listing leaves the diff without untracked paths.
        // An interrupt restarts the whole snapshot so the count matches it.
        // Working-copy views (including `show .` / `diff -c .`) list
        // untracked paths. A fixed commit or PR does not.
        let untracked_tool = match &spec {
            CaptureSpec::Live { tool, .. } => Some(*tool),
            CaptureSpec::Commit { tool, .. } if spec.is_live() => Some(*tool),
            CaptureSpec::Commit { .. } => None,
            CaptureSpec::Fixed { .. } => None,
        };
        let untracked = match untracked_tool {
            Some(tool) => {
                let untracked_cmd = untracked_command_for_tool(tool);
                match capture_stdout_interruptible(&untracked_cmd, Some(&refresh)).await {
                    Ok(CaptureOutcome::Interrupted) => {
                        force_send = true;
                        continue;
                    }
                    Ok(CaptureOutcome::Done { bytes, success }) => {
                        if success {
                            CachedUntracked::Ok(bytes)
                        } else {
                            CachedUntracked::Failed
                        }
                    }
                    Err(_) => CachedUntracked::Failed,
                }
            }
            None => CachedUntracked::Ok(Vec::new()),
        };
        let unchanged = match &cached {
            Some(prev) => {
                if prev.diff == diff {
                    prev.untracked == untracked
                } else {
                    false
                }
            }
            None => false,
        };
        if unchanged {
            if force_send {
                if let Some(prev) = &cached {
                    if tx.send(prev.output.clone()).await.is_err() {
                        return;
                    }
                }
                force_send = false;
            }
            // Same bytes: reuse the cached DiffOutput. A forced send
            // still delivers it so a loading placeholder can clear.
        } else {
            let output = match &cached {
                Some(prev) if prev.diff == diff => {
                    let mut output = prev.output.clone();
                    output.set_untracked(untracked_paths(&untracked));
                    output
                }
                _ => {
                    let mut output = match &diff {
                        CachedDiff::Bytes(bytes) => parse_diff(bytes),
                        CachedDiff::Error(text) => DiffOutput::message(text),
                    };
                    output.set_untracked(untracked_paths(&untracked));
                    output
                }
            };
            cached = Some(CachedSnapshot {
                diff,
                untracked,
                output: output.clone(),
            });
            if tx.send(output).await.is_err() {
                return;
            }
            force_send = false;
        }
        if live {
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = refresh.notified() => {
                    force_send = true;
                }
            }
        } else {
            // Commit mode: wait for an explicit refresh only.
            refresh.notified().await;
            force_send = true;
        }
    }
}

fn untracked_paths(untracked: &CachedUntracked) -> Vec<String> {
    match untracked {
        CachedUntracked::Failed => Vec::new(),
        CachedUntracked::Ok(bytes) => parse_untracked_paths(bytes),
    }
}

async fn wait_interrupt(interrupt: Option<&NotifyRx>) {
    match interrupt {
        Some(rx) => rx.notified().await,
        None => future::pending().await,
    }
}

/// SIGKILL the child's process group, then reap the child.
async fn kill_process_group(child: &mut Child) {
    if let Some(id) = child.id() {
        // SAFETY: `id` is the pid of a child we spawned with `process_group(0)`,
        // so it is the process-group id. Negative pid targets that group.
        unsafe {
            libc::kill(-(id as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

enum ReadReady {
    Eof,
    Pending,
}

/// Read while tokio still considers the pipe readable.
fn read_ready(
    reader: &pipe::Receiver,
    buf: &mut [u8],
    out: &mut Vec<u8>,
) -> anyhow::Result<ReadReady> {
    loop {
        match reader.try_read(buf) {
            Ok(0) => return Ok(ReadReady::Eof),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(ReadReady::Pending),
            Err(err) => return Err(err.into()),
        }
    }
}

/// Append bytes already buffered on `reader`. Stop at EOF or when a read would block.
///
/// Deregisters the tokio pipe and reads the underlying fd. A grandchild
/// may still hold the write end, so this must not block for EOF.
fn drain_nonblocking(reader: pipe::Receiver, out: &mut Vec<u8>) -> anyhow::Result<()> {
    // into_nonblocking_fd deregisters the pipe and returns the fd still
    // in nonblocking mode (from_owned_fd set O_NONBLOCK; only
    // into_blocking_fd clears it).
    let fd = reader.into_nonblocking_fd()?;
    let mut reader = io::PipeReader::from(fd);
    let mut buf = [0_u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(err) => return Err(err.into()),
        }
    }
}

fn append_nonzero_status(status: &process::ExitStatus, out: &mut Vec<u8>) {
    if status.success() {
        return;
    }
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    let line = match status.code() {
        Some(code) => format!("exit status: {code}\n"),
        None => format!("{status}\n"),
    };
    out.extend_from_slice(line.as_bytes());
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::path::PathBuf;
    use std::process;
    use std::time::Duration;

    use tokio::net::unix::pipe;

    use crate::capture::CaptureOutcome;
    use crate::capture::capture_command_output_interruptible;
    use crate::capture::drain_nonblocking;

    async fn capture_command_output(command: &[String]) -> anyhow::Result<Vec<u8>> {
        match capture_command_output_interruptible(command, None).await? {
            CaptureOutcome::Done { bytes, .. } => Ok(bytes),
            CaptureOutcome::Interrupted => Err(anyhow::anyhow!("capture interrupted")),
        }
    }

    #[test]
    fn test_diff_view_kind_commands() {
        use crate::capture::CaptureSpec;
        use crate::capture::DiffViewKind;
        use crate::repo::repo::DiffTool;

        assert_eq!("diff -r .", DiffViewKind::Default.label());
        assert_eq!("diff -c .", DiffViewKind::ChangeCurrent.label());
        assert_eq!("diff -r prev", DiffViewKind::RevPrev.label());

        let sl_default = CaptureSpec::for_kind(DiffTool::Sl, DiffViewKind::Default);
        assert_eq!(
            vec![
                String::from("sl"),
                String::from("diff"),
                String::from("-U"),
                String::from("3"),
                String::from("--config"),
                String::from("diff.showfunc=true"),
            ],
            sl_default.command(3)
        );
        assert_eq!(Some(DiffViewKind::Default), sl_default.view_kind());
        assert_eq!(true, sl_default.is_live());

        let sl_change = CaptureSpec::for_kind(DiffTool::Sl, DiffViewKind::ChangeCurrent);
        assert_eq!(
            vec![
                String::from("sl"),
                String::from("log"),
                String::from("-r"),
                String::from("."),
                String::from("--patch"),
                String::from("--config"),
                String::from("diff.unified=3"),
                String::from("--config"),
                String::from("diff.showfunc=true"),
                String::from("-T"),
                String::from(
                    "commit:      {node|short}\nuser:        {author}\ndate:        {date|isodate}\ndescription:\n{desc}\n\n"
                ),
            ],
            sl_change.command(3)
        );
        assert_eq!(Some(DiffViewKind::ChangeCurrent), sl_change.view_kind());
        assert_eq!(true, sl_change.is_live());
        assert_eq!("diff -c .", sl_change.mode_status());
        assert_eq!("diff -c . (R)", sl_change.status_mode_text());
        assert_eq!("diff -r . (R)", sl_default.status_mode_text());

        let sl_prev = CaptureSpec::for_kind(DiffTool::Sl, DiffViewKind::RevPrev);
        assert_eq!(
            vec![
                String::from("sl"),
                String::from("diff"),
                String::from("-U"),
                String::from("3"),
                String::from("--config"),
                String::from("diff.showfunc=true"),
                String::from("-r"),
                String::from("prev"),
            ],
            sl_prev.command(3)
        );
        assert_eq!(Some(DiffViewKind::RevPrev), sl_prev.view_kind());
        assert_eq!(true, sl_prev.is_live());
        assert_eq!("diff -r prev", sl_prev.mode_status());

        let git_default = CaptureSpec::for_kind(DiffTool::Git, DiffViewKind::Default);
        assert_eq!(
            vec![
                String::from("git"),
                String::from("diff"),
                String::from("--no-ext-diff"),
                String::from("--unified=3"),
            ],
            git_default.command(3)
        );

        let git_change = CaptureSpec::for_kind(DiffTool::Git, DiffViewKind::ChangeCurrent);
        assert_eq!(
            vec![
                String::from("git"),
                String::from("log"),
                String::from("-1"),
                String::from("-p"),
                String::from("--no-ext-diff"),
                String::from("--unified=3"),
                String::from("HEAD"),
            ],
            git_change.command(3)
        );
        assert_eq!(Some(DiffViewKind::ChangeCurrent), git_change.view_kind());
        assert_eq!(true, git_change.is_live());
        assert_eq!("diff -c .", git_change.mode_status());
        assert_eq!("diff -c . (R)", git_change.status_mode_text());
        let fixed = CaptureSpec::Commit {
            tool: DiffTool::Sl,
            commit: String::from("0123456789abcdef0123456789abcdef01234567"),
        };
        assert_eq!(false, fixed.is_live());
        assert_eq!(
            "diff -c 0123456789abcdef0123456789abcdef01234567",
            fixed.mode_status()
        );

        let git_prev = CaptureSpec::for_kind(DiffTool::Git, DiffViewKind::RevPrev);
        assert_eq!(
            vec![
                String::from("git"),
                String::from("diff"),
                String::from("--no-ext-diff"),
                String::from("--unified=3"),
                String::from("HEAD^"),
            ],
            git_prev.command(3)
        );
        assert_eq!(Some(DiffViewKind::RevPrev), git_prev.view_kind());

        assert_eq!("diff -r .", sl_default.mode_status());
        assert_eq!("diff -c .", sl_change.mode_status());
        assert_eq!("diff -r prev", sl_prev.mode_status());
        assert_eq!("diff -c .", git_change.mode_status());
        assert_eq!("diff -r prev", git_prev.mode_status());
        assert_eq!("diff -r . (R)", sl_default.status_mode_text());
        let custom = CaptureSpec::Live {
            tool: DiffTool::Sl,
            rev: Some(String::from("main")),
        };
        assert_eq!("diff -r main", custom.mode_status());
        assert_eq!("diff -r main (R)", custom.status_mode_text());
        let pr = CaptureSpec::Fixed {
            command: crate::repo::pr_diff::origin_pr_diff_command(41),
        };
        assert_eq!("origin pr 41", pr.mode_status());
        let gh = CaptureSpec::Fixed {
            command: crate::repo::pr_diff::github_pr_diff_command(7),
        };
        assert_eq!("gh pr 7", gh.mode_status());
    }

    #[test]
    fn test_untracked_paths_empty_on_failure() {
        assert_eq!(
            Vec::<String>::new(),
            crate::capture::untracked_paths(&crate::capture::CachedUntracked::Failed)
        );
        assert_eq!(
            vec![String::from("a"), String::from("b")],
            crate::capture::untracked_paths(&crate::capture::CachedUntracked::Ok(
                b"b\0a\0".to_vec()
            ))
        );
    }

    fn receiver_from_pipe(reader: io::PipeReader) -> pipe::Receiver {
        pipe::Receiver::from_owned_fd(OwnedFd::from(reader)).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_drain_nonblocking_reads_buffer_and_stops_with_writer_open() {
        let (reader, mut writer) = io::pipe().unwrap();
        writer.write_all(b"hello").unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(b"pre");
        // Writer stays open, so a blocking read would wait for EOF.
        drain_nonblocking(receiver_from_pipe(reader), &mut out).unwrap();
        assert_eq!(b"prehello".as_slice(), out.as_slice());
        drop(writer);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_drain_nonblocking_empty_pipe_with_writer_open() {
        let (reader, writer) = io::pipe().unwrap();
        let mut out = Vec::new();
        drain_nonblocking(receiver_from_pipe(reader), &mut out).unwrap();
        assert_eq!(Vec::<u8>::new(), out);
        drop(writer);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_drain_nonblocking_reads_to_eof() {
        let (reader, mut writer) = io::pipe().unwrap();
        writer.write_all(b"abc").unwrap();
        drop(writer);
        let mut out = Vec::new();
        drain_nonblocking(receiver_from_pipe(reader), &mut out).unwrap();
        assert_eq!(b"abc".as_slice(), out.as_slice());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_capture_stdout_and_stderr() {
        let command = [
            String::from("sh"),
            String::from("-c"),
            String::from("printf 'a\\n'; printf 'b\\n' >&2"),
        ];
        let bytes = capture_command_output(&command).await.unwrap();
        assert_eq!(b"a\nb\n".as_slice(), bytes.as_slice());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_capture_appends_nonzero_exit_status() {
        let command = [
            String::from("sh"),
            String::from("-c"),
            String::from("printf 'no\\n'; exit 3"),
        ];
        let bytes = capture_command_output(&command).await.unwrap();
        assert_eq!(b"no\nexit status: 3\n".as_slice(), bytes.as_slice());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_interrupt_kills_hung_command() {
        use crate::util::notify_pair::NotifyTx;

        let (tx, rx) = NotifyTx::pair();
        let command = [String::from("sleep"), String::from("30")];
        let capture = capture_command_output_interruptible(&command, Some(&rx));
        let interrupter = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            tx.notify_one();
        };
        let joined = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(capture, interrupter)
        });
        let (outcome, ()) = joined.await.expect("interrupt did not kill the child");
        assert_eq!(
            true,
            matches!(outcome.unwrap(), CaptureOutcome::Interrupted)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_capture_returns_when_child_exits_despite_open_pipe() {
        let pid_path = PathBuf::from(format!("/tmp/sl-tui-capture-test-{}.pid", process::id()));
        let _cleanup = KillPidFile {
            path: pid_path.clone(),
        };
        let script = format!(
            "printf 'hi\\n'; sleep 30 & echo $! > '{}'",
            pid_path.display()
        );
        let command = [String::from("sh"), String::from("-c"), script];
        let bytes = tokio::time::timeout(Duration::from_secs(2), capture_command_output(&command))
            .await
            .expect("capture waited for the grandchild")
            .unwrap();
        assert_eq!(b"hi\n".as_slice(), bytes.as_slice());
    }

    struct KillPidFile {
        path: PathBuf,
    }

    impl Drop for KillPidFile {
        fn drop(&mut self) {
            if let Ok(text) = fs::read_to_string(&self.path) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    // SAFETY: pid came from a child we started; SIGKILL is fine in tests.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
            let _ = fs::remove_file(&self.path);
        }
    }
}
