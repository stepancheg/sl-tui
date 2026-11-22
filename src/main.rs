mod capture;
mod diff;
mod event_loop;
mod help;
mod interval_arg;
mod lines_with_wrapped_row_count;
mod render;
mod repo;
mod search;
mod smartlog;
mod terminal;
mod terminal_bg;
mod theme;
mod util;
mod wrapped_row;

use std::io;
use std::panic;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clap::Parser;
use crossterm::event::Event;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::task::AbortOnDropHandle;

use crate::capture::CaptureSpec;
use crate::capture::capture_command_output_loop;
use crate::event_loop::EventLoop;
use crate::event_loop::StartView;
use crate::interval_arg::IntervalArg;
use crate::render::renderer::terminal_size;
use crate::repo::pr_diff::github_pr_diff_command;
use crate::repo::pr_diff::origin_pr_diff_command;
use crate::repo::pr_status_actor::PrStatusActor;
use crate::repo::repo::DEFAULT_CONTEXT;
use crate::repo::repo::DiffTool;
use crate::repo::repo::MAX_CONTEXT;
use crate::repo::repo::diff_tool;
use crate::repo::sl::smartlog_actor::SmartlogActor;
use crate::search::history::store::History;
use crate::terminal::guard::TerminalGuard;
use crate::terminal::guard::restore_terminal;
use crate::terminal_bg::query_terminal_bg;
use crate::util::notify_pair::NotifyTx;

#[derive(clap::Parser, Debug)]
struct Args {
    /// Seconds between command runs.
    #[clap(
        short = 'n',
        long,
        env = "LIVE_DIFF_INTERVAL",
        default_value = "0.5",
        global = true
    )]
    interval: IntervalArg,

    /// Use Sapling (`sl`).
    #[clap(long, conflicts_with = "git", global = true)]
    sl: bool,

    /// Use git.
    #[clap(long, conflicts_with = "sl", global = true)]
    git: bool,

    #[clap(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    #[clap(visible_alias = "di")]
    Diff(DiffCommand),

    Show(ShowCommand),

    Sl(SlCommand),
}

/// Open the smartlog
///
/// This is the default when no subcommand is given.
#[derive(clap::Args, Debug)]
#[clap(verbatim_doc_comment)]
struct SlCommand {}

/// Show a commit message and diff
///
/// Like `sl show` / `git show`. Without a revision, the current
/// commit (`.`), which is refreshed on the interval. A digest is
/// captured once.
#[derive(clap::Args, Debug)]
#[clap(verbatim_doc_comment)]
struct ShowCommand {
    /// Revision to show.
    #[clap(short = 'r', long = "rev")]
    rev: Option<String>,

    /// Revision to show. Used when `-r` is not given.
    #[clap(value_name = "REV")]
    rev_pos: Option<String>,

    /// Open the diff stat view.
    #[clap(long)]
    stat: bool,

    /// Initial unified-diff context lines.
    #[clap(short = 'U', long = "unified", default_value_t = DEFAULT_CONTEXT)]
    context: u32,
}

/// Show a diff
///
/// Without `-r` or `-c`, the working-copy diff.
#[derive(clap::Args, Debug)]
#[clap(verbatim_doc_comment)]
struct DiffCommand {
    /// Revision to compare the working copy against.
    /// Without this flag, run the tool's default diff.
    #[clap(
        short = 'r',
        long = "rev",
        conflicts_with = "commit",
        conflicts_with = "origin_pr",
        conflicts_with = "github_pr"
    )]
    rev: Option<String>,

    /// Show this commit's message and diff (`sl show`, `git show`).
    /// A digest is captured once. `.` (the working copy) is
    /// refreshed on the interval.
    #[clap(
        short = 'c',
        long = "change",
        conflicts_with = "rev",
        conflicts_with = "origin_pr",
        conflicts_with = "github_pr"
    )]
    commit: Option<String>,

    /// Show Origin pull request N (`origin pr diff N`). Fetched once;
    /// not refreshed on an interval.
    #[clap(
        long,
        conflicts_with = "github_pr",
        conflicts_with = "rev",
        conflicts_with = "commit"
    )]
    origin_pr: Option<u64>,

    /// Show GitHub pull request N (`gh pr diff N`). Fetched once;
    /// not refreshed on an interval.
    #[clap(
        long,
        conflicts_with = "origin_pr",
        conflicts_with = "rev",
        conflicts_with = "commit"
    )]
    github_pr: Option<u64>,

    /// Open the diff stat view.
    #[clap(long)]
    stat: bool,

    /// Initial unified-diff context lines.
    #[clap(short = 'U', long = "unified", default_value_t = DEFAULT_CONTEXT)]
    context: u32,
}

fn install_panic_hook() {
    let original = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original(info);
    }));
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let spec = capture_spec(&args)?;
    // `d` can switch to a working-copy mode; that needs the repo tool.
    // A PR diff may run outside a repository, in which case the menu
    // cannot switch.
    let menu_tool = match &spec {
        CaptureSpec::Live { tool, .. } => Some(*tool),
        CaptureSpec::Commit { tool, .. } => Some(*tool),
        CaptureSpec::Fixed { .. } => diff_tool().ok(),
    };
    let start_view = match &args.command {
        None => match menu_tool {
            // No subcommand behaves as `sl`. A git-only repository
            // has no smartlog, so open the diff.
            Some(DiffTool::Sl) => StartView::Tree,
            Some(DiffTool::Git) => StartView::Diff,
            None => StartView::Diff,
        },
        Some(Command::Sl(_)) => match menu_tool {
            Some(DiffTool::Sl) => StartView::Tree,
            Some(DiffTool::Git) => anyhow::bail!("sl requires a Sapling repository"),
            None => anyhow::bail!("sl requires a Sapling repository"),
        },
        Some(Command::Diff(diff)) => {
            if diff.stat {
                StartView::Stat
            } else {
                StartView::Diff
            }
        }
        Some(Command::Show(show)) => {
            if show.stat {
                StartView::Stat
            } else {
                StartView::Diff
            }
        }
    };
    let spec = Arc::new(Mutex::new(spec));
    let context = Arc::new(AtomicU32::new(initial_context(&args).min(MAX_CONTEXT)));
    let history = History::load_from_home().await;

    // Capture the first snapshot on the primary screen so the alternate
    // screen is not shown empty while the command runs.
    let (output_tx, mut output_rx) = mpsc::channel(1);
    let (refresh_tx, refresh_rx) = NotifyTx::pair();
    let capture_task = AbortOnDropHandle::new(tokio::spawn(capture_command_output_loop(
        Arc::clone(&spec),
        Arc::clone(&context),
        args.interval.as_duration(),
        output_tx,
        refresh_rx,
    )));
    let Some(first_lines) = output_rx.recv().await else {
        capture_task.await?;
        return Ok(());
    };

    install_panic_hook();
    // Query before raw mode; the color query reads a terminal response.
    let terminal_bg = query_terminal_bg();
    let _guard = TerminalGuard::enter_alt_mode()?;
    let size = terminal_size()?;

    // One reader thread: crossterm events must not be polled concurrently.
    // `poll` with a timeout lets the thread exit after `stop` is set.
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_on_drop = StopOnDrop(Arc::clone(&stop));
    let (_key_reader, keys) = spawn_key_reader(stop);

    // Spawn the tree actors first and hand their channel ends to the
    // UI loop. The handles stay in this scope until `run` returns.
    let smartlog_actor = SmartlogActor::spawn(menu_tool, args.interval.as_duration());
    let pr_status_actor = PrStatusActor::spawn();
    EventLoop::new(
        terminal_bg,
        history,
        size,
        context,
        spec,
        menu_tool,
        start_view,
        smartlog_actor.live,
        smartlog_actor.epoch,
        smartlog_actor.wake_tx,
        pr_status_actor.job_tx,
    )
    .run(
        first_lines,
        output_rx,
        keys,
        refresh_tx,
        capture_task,
        smartlog_actor.views_rx,
        pr_status_actor.result_rx,
    )
    .await
}

fn initial_context(args: &Args) -> u32 {
    match &args.command {
        Some(Command::Diff(diff)) => diff.context,
        Some(Command::Show(show)) => show.context,
        Some(Command::Sl(_)) => DEFAULT_CONTEXT,
        None => DEFAULT_CONTEXT,
    }
}

/// Revision for `show`: `-r REV`, or a positional REV, or `.`.
fn show_revision(show: &ShowCommand) -> anyhow::Result<String> {
    match (&show.rev, &show.rev_pos) {
        (Some(_), Some(_)) => {
            anyhow::bail!("show: pass the revision with -r or as an argument, not both")
        }
        (Some(rev), None) => Ok(rev.clone()),
        (None, Some(rev)) => Ok(rev.clone()),
        (None, None) => Ok(String::from(".")),
    }
}

/// PR diffs do not need a local repository. A working-copy or commit
/// diff does, and that is when the sl/git tool is resolved.
fn capture_spec(args: &Args) -> anyhow::Result<CaptureSpec> {
    let diff = match &args.command {
        Some(Command::Diff(diff)) => Some(diff),
        Some(Command::Show(_)) => None,
        Some(Command::Sl(_)) => None,
        None => None,
    };
    if let Some(number) = diff.and_then(|d| d.origin_pr) {
        return Ok(CaptureSpec::Fixed {
            command: origin_pr_diff_command(number),
        });
    }
    if let Some(number) = diff.and_then(|d| d.github_pr) {
        return Ok(CaptureSpec::Fixed {
            command: github_pr_diff_command(number),
        });
    }
    let tool = if args.sl {
        DiffTool::Sl
    } else if args.git {
        DiffTool::Git
    } else {
        diff_tool()?
    };
    if let Some(Command::Show(show)) = &args.command {
        let commit = show_revision(show)?;
        return Ok(CaptureSpec::Commit { tool, commit });
    }
    if let Some(commit) = diff.and_then(|d| d.commit.as_ref()) {
        return Ok(CaptureSpec::Commit {
            tool,
            commit: commit.clone(),
        });
    }
    Ok(CaptureSpec::Live {
        tool,
        rev: diff.and_then(|d| d.rev.clone()),
    })
}

fn spawn_key_reader(
    stop: Arc<AtomicBool>,
) -> (AbortOnDropHandle<()>, UnboundedReceiver<io::Result<Event>>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = AbortOnDropHandle::new(tokio::task::spawn_blocking(move || {
        while !stop.load(Ordering::SeqCst) {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(false) => {}
                Ok(true) => match crossterm::event::read() {
                    Ok(event) => {
                        if tx.send(Ok(event)).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err));
                        break;
                    }
                },
                Err(err) => {
                    let _ = tx.send(Err(err));
                    break;
                }
            }
        }
    }));
    (handle, rx)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::Args;

    #[test]
    fn test_sl_and_git_flags_conflict() {
        let err = Args::try_parse_from(["sl-tui", "--sl", "--git"]).unwrap_err();
        let text = err.to_string();
        assert_eq!(true, text.contains("--sl"));
        assert_eq!(true, text.contains("--git"));
    }

    #[test]
    fn test_sl_flag_parses() {
        let args = Args::try_parse_from(["sl-tui", "--sl"]).unwrap();
        assert_eq!(true, args.sl);
        assert_eq!(false, args.git);
    }

    #[test]
    fn test_git_flag_parses() {
        let args = Args::try_parse_from(["sl-tui", "--git"]).unwrap();
        assert_eq!(false, args.sl);
        assert_eq!(true, args.git);
    }

    fn diff_args(args: &Args) -> &crate::DiffCommand {
        match &args.command {
            Some(crate::Command::Diff(diff)) => diff,
            Some(crate::Command::Show(_)) => panic!("expected diff subcommand"),
            Some(crate::Command::Sl(_)) => panic!("expected diff subcommand"),
            None => panic!("expected diff subcommand"),
        }
    }

    #[test]
    fn test_default_command_is_tree() {
        let args = Args::try_parse_from(["sl-tui"]).unwrap();
        assert_eq!(true, args.command.is_none());
        let args = Args::try_parse_from(["sl-tui", "sl"]).unwrap();
        assert_eq!(true, matches!(args.command, Some(crate::Command::Sl(_))));
    }

    #[test]
    fn test_show_revision_parses() {
        let args = Args::try_parse_from(["sl-tui", "show"]).unwrap();
        let spec = crate::capture_spec(&args).unwrap();
        match spec {
            crate::capture::CaptureSpec::Commit { commit, .. } => {
                assert_eq!(String::from("."), commit);
            }
            crate::capture::CaptureSpec::Live { .. } => panic!("expected commit"),
            crate::capture::CaptureSpec::Fixed { .. } => panic!("expected commit"),
        }
        let args = Args::try_parse_from(["sl-tui", "show", "abc"]).unwrap();
        match crate::capture_spec(&args).unwrap() {
            crate::capture::CaptureSpec::Commit { commit, .. } => {
                assert_eq!(String::from("abc"), commit);
            }
            _ => panic!("expected commit"),
        }
        let args = Args::try_parse_from(["sl-tui", "show", "-r", "main"]).unwrap();
        match crate::capture_spec(&args).unwrap() {
            crate::capture::CaptureSpec::Commit { commit, .. } => {
                assert_eq!(String::from("main"), commit);
            }
            _ => panic!("expected commit"),
        }
        let args = Args::try_parse_from(["sl-tui", "show", "-r", "main", "abc"]).unwrap();
        match &args.command {
            Some(crate::Command::Show(show)) => {
                assert_eq!(true, crate::show_revision(show).is_err());
            }
            _ => panic!("expected show"),
        }
    }

    #[test]
    fn test_diff_alias_di_parses() {
        let args = Args::try_parse_from(["sl-tui", "di"]).unwrap();
        assert_eq!(true, matches!(args.command, Some(crate::Command::Diff(_))));
        let args = Args::try_parse_from(["sl-tui", "diff", "-c", "."]).unwrap();
        assert_eq!(Some(String::from(".")), diff_args(&args).commit);
    }

    #[test]
    fn test_rev_flag_parses() {
        let args = Args::try_parse_from(["sl-tui", "diff", "-r", ".^"]).unwrap();
        assert_eq!(Some(String::from(".^")), diff_args(&args).rev);
        let args = Args::try_parse_from(["sl-tui", "diff", "--rev", "main"]).unwrap();
        assert_eq!(Some(String::from("main")), diff_args(&args).rev);
        let args = Args::try_parse_from(["sl-tui", "diff"]).unwrap();
        assert_eq!(None, diff_args(&args).rev);
    }

    #[test]
    fn test_origin_pr_flag_parses() {
        let args = Args::try_parse_from(["sl-tui", "diff", "--origin-pr", "41"]).unwrap();
        assert_eq!(Some(41), diff_args(&args).origin_pr);
        assert_eq!(None, diff_args(&args).github_pr);
        let spec = crate::capture_spec(&args).unwrap();
        match spec {
            crate::capture::CaptureSpec::Fixed { command } => {
                assert_eq!(crate::repo::pr_diff::origin_pr_diff_command(41), command);
            }
            crate::capture::CaptureSpec::Live { .. } => panic!("expected fixed command"),
            crate::capture::CaptureSpec::Commit { .. } => panic!("expected fixed command"),
        }
    }

    #[test]
    fn test_github_pr_flag_parses() {
        let args = Args::try_parse_from(["sl-tui", "diff", "--github-pr", "41"]).unwrap();
        assert_eq!(Some(41), diff_args(&args).github_pr);
        assert_eq!(None, diff_args(&args).origin_pr);
        let spec = crate::capture_spec(&args).unwrap();
        match spec {
            crate::capture::CaptureSpec::Fixed { command } => {
                assert_eq!(crate::repo::pr_diff::github_pr_diff_command(41), command);
            }
            crate::capture::CaptureSpec::Live { .. } => panic!("expected fixed command"),
            crate::capture::CaptureSpec::Commit { .. } => panic!("expected fixed command"),
        }
    }

    #[test]
    fn test_pr_flags_conflict_with_each_other_and_rev() {
        let err = Args::try_parse_from(["sl-tui", "diff", "--origin-pr", "1", "--github-pr", "2"])
            .unwrap_err();
        assert_eq!(true, err.to_string().contains("--origin-pr"));
        let err =
            Args::try_parse_from(["sl-tui", "diff", "--origin-pr", "1", "-r", "main"]).unwrap_err();
        assert_eq!(true, err.to_string().contains("--origin-pr"));
        let err =
            Args::try_parse_from(["sl-tui", "diff", "--github-pr", "1", "-c", "abc"]).unwrap_err();
        assert_eq!(true, err.to_string().contains("--github-pr"));
    }

    #[test]
    fn test_unified_flag_parses() {
        let args = Args::try_parse_from(["sl-tui"]).unwrap();
        assert_eq!(
            crate::repo::repo::DEFAULT_CONTEXT,
            crate::initial_context(&args)
        );
        let args = Args::try_parse_from(["sl-tui", "diff", "-U", "0"]).unwrap();
        assert_eq!(0, diff_args(&args).context);
        let args = Args::try_parse_from(["sl-tui", "diff", "--unified", "12"]).unwrap();
        assert_eq!(12, diff_args(&args).context);
        let args = Args::try_parse_from(["sl-tui", "show", "-U", "5"]).unwrap();
        match &args.command {
            Some(crate::Command::Show(show)) => assert_eq!(5, show.context),
            _ => panic!("expected show"),
        }
        let err = Args::try_parse_from(["sl-tui", "-U", "3"]).unwrap_err();
        assert_eq!(
            true,
            err.to_string().contains("-U") || err.to_string().contains("unified")
        );
    }

    #[test]
    fn test_stat_flag_parses_on_diff() {
        let args = Args::try_parse_from(["sl-tui", "diff"]).unwrap();
        assert_eq!(false, diff_args(&args).stat);
        let args = Args::try_parse_from(["sl-tui", "diff", "--stat"]).unwrap();
        assert_eq!(true, diff_args(&args).stat);
        let err = Args::try_parse_from(["sl-tui", "--stat"]).unwrap_err();
        assert_eq!(
            true,
            err.to_string().contains("--stat") || err.to_string().contains("stat")
        );
    }
}

/// Sets the key-reader stop flag when the watch loop exits.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
