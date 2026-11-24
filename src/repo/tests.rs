use std::fs;
use std::path::Path;

use crate::diff::DiffOutput;
use crate::diff::FileDiff;
use crate::diff::HunkLineKind;
use crate::diff::parse_diff;
use crate::repo::git::git_commit_diff_command;
use crate::repo::git::git_diff_command;
use crate::repo::git::git_untracked_command;
use crate::repo::repo::parse_untracked_paths;
use crate::repo::sl::command::sl_commit_diff_command;
use crate::repo::sl::command::sl_diff_command;
use crate::repo::sl::command::sl_untracked_command;
use crate::repo::test_support::TestRepo;

/// Which repository the shared diff-command scenarios run against.
///
/// `Git` runs the git commands in a plain git repo. `Sl` runs the sl
/// commands in a native `.sl` repo. `SlOnGit` runs the sl commands in a
/// git repo with Sapling metadata in `.git/sl` (`sl init --git` is a
/// different layout: a `.sl` repo whose store is git).
#[derive(Clone, Copy)]
enum Kind {
    Git,
    Sl,
    SlOnGit,
}

#[test]
fn test_git_diff_command_worktree_clean() {
    check_diff_command_worktree_clean(&Repo::init(Kind::Git));
}

#[test]
fn test_sl_diff_command_worktree_clean_sl_mode() {
    check_diff_command_worktree_clean(&Repo::init(Kind::Sl));
}

#[test]
fn test_sl_diff_command_worktree_clean_git_mode() {
    check_diff_command_worktree_clean(&Repo::init(Kind::SlOnGit));
}

#[test]
fn test_git_diff_command_worktree_edit() {
    check_diff_command_worktree_edit(&Repo::init(Kind::Git));
}

#[test]
fn test_sl_diff_command_worktree_edit_sl_mode() {
    check_diff_command_worktree_edit(&Repo::init(Kind::Sl));
}

#[test]
fn test_sl_diff_command_worktree_edit_git_mode() {
    check_diff_command_worktree_edit(&Repo::init(Kind::SlOnGit));
}

#[test]
fn test_git_diff_command_context_zero() {
    check_diff_command_context_zero(&Repo::init(Kind::Git));
}

#[test]
fn test_sl_diff_command_context_zero_sl_mode() {
    check_diff_command_context_zero(&Repo::init(Kind::Sl));
}

#[test]
fn test_sl_diff_command_context_zero_git_mode() {
    check_diff_command_context_zero(&Repo::init(Kind::SlOnGit));
}

#[test]
fn test_git_diff_command_context_two() {
    check_diff_command_context_two(&Repo::init(Kind::Git));
}

#[test]
fn test_sl_diff_command_context_two_sl_mode() {
    check_diff_command_context_two(&Repo::init(Kind::Sl));
}

#[test]
fn test_sl_diff_command_context_two_git_mode() {
    check_diff_command_context_two(&Repo::init(Kind::SlOnGit));
}

#[test]
fn test_git_diff_command_rev() {
    check_diff_command_rev(&Repo::init(Kind::Git));
}

#[test]
fn test_sl_diff_command_rev_sl_mode() {
    check_diff_command_rev(&Repo::init(Kind::Sl));
}

#[test]
fn test_sl_diff_command_rev_git_mode() {
    check_diff_command_rev(&Repo::init(Kind::SlOnGit));
}

#[test]
fn test_git_diff_command_worktree_after_commit() {
    check_diff_command_worktree_after_commit(&Repo::init(Kind::Git));
}

#[test]
fn test_sl_diff_command_worktree_after_commit_sl_mode() {
    check_diff_command_worktree_after_commit(&Repo::init(Kind::Sl));
}

#[test]
fn test_sl_diff_command_worktree_after_commit_git_mode() {
    check_diff_command_worktree_after_commit(&Repo::init(Kind::SlOnGit));
}

#[test]
fn test_git_commit_diff_command_one_commit() {
    check_commit_diff_command_one_commit(&Repo::init(Kind::Git));
}

#[test]
fn test_sl_commit_diff_command_one_commit_sl_mode() {
    check_commit_diff_command_one_commit(&Repo::init(Kind::Sl));
}

#[test]
fn test_sl_commit_diff_command_one_commit_git_mode() {
    check_commit_diff_command_one_commit(&Repo::init(Kind::SlOnGit));
}

#[test]
fn test_git_commit_diff_command_root() {
    check_commit_diff_command_root(&Repo::init(Kind::Git));
}

#[test]
fn test_sl_commit_diff_command_root_sl_mode() {
    check_commit_diff_command_root(&Repo::init(Kind::Sl));
}

#[test]
fn test_sl_commit_diff_command_root_git_mode() {
    check_commit_diff_command_root(&Repo::init(Kind::SlOnGit));
}

#[test]
fn test_git_commit_diff_command_rename() {
    check_commit_diff_command_rename(&Repo::init(Kind::Git));
}

#[test]
fn test_sl_commit_diff_command_rename_sl_mode() {
    check_commit_diff_command_rename(&Repo::init(Kind::Sl));
}

#[test]
fn test_sl_commit_diff_command_rename_git_mode() {
    check_commit_diff_command_rename(&Repo::init(Kind::SlOnGit));
}

#[test]
fn test_git_untracked_command_lists_paths() {
    check_untracked_command_lists_paths(&Repo::init(Kind::Git));
}

#[test]
fn test_sl_untracked_command_lists_paths_sl_mode() {
    check_untracked_command_lists_paths(&Repo::init(Kind::Sl));
}

#[test]
fn test_sl_untracked_command_lists_paths_git_mode() {
    check_untracked_command_lists_paths(&Repo::init(Kind::SlOnGit));
}

fn check_diff_command_worktree_clean(repo: &Repo) {
    let dir = repo.path();
    write_file(dir, "a.txt", "one\ntwo\nthree\n");
    repo.commit("add a");
    let clean = repo.run_diff(repo.diff_command(3, None));
    assert_eq!(0, clean.files().len());
}

fn check_diff_command_worktree_edit(repo: &Repo) {
    let dir = repo.path();
    write_file(dir, "a.txt", "one\ntwo\nthree\n");
    repo.commit("add a");
    write_file(dir, "a.txt", "one\nTWO\nthree\n");
    write_file(dir, "untracked.txt", "nope\n");
    let dirty = repo.run_diff(repo.diff_command(3, None));
    let files = dirty.files();
    assert_eq!(1, files.len());
    assert_eq!(Some(String::from("a.txt")), files[0].old);
    assert_eq!(Some(String::from("a.txt")), files[0].new);
    assert_eq!(
        vec![
            (HunkLineKind::Context, String::from("one")),
            (HunkLineKind::Delete, String::from("two")),
            (HunkLineKind::Insert, String::from("TWO")),
            (HunkLineKind::Context, String::from("three")),
        ],
        hunk_lines(files[0])
    );
}

fn check_diff_command_context_zero(repo: &Repo) {
    let dir = repo.path();
    write_file(dir, "a.txt", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n");
    repo.commit("add a");
    write_file(dir, "a.txt", "l1\nl2\nl3\nl4\nL5\nl6\nl7\nl8\nl9\n");
    let narrow = repo.run_diff(repo.diff_command(0, None));
    let narrow_files = narrow.files();
    assert_eq!(1, narrow_files.len());
    assert_eq!(
        vec![
            (HunkLineKind::Delete, String::from("l5")),
            (HunkLineKind::Insert, String::from("L5")),
        ],
        hunk_lines(narrow_files[0])
    );
}

fn check_diff_command_context_two(repo: &Repo) {
    let dir = repo.path();
    write_file(dir, "a.txt", "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n");
    repo.commit("add a");
    write_file(dir, "a.txt", "l1\nl2\nl3\nl4\nL5\nl6\nl7\nl8\nl9\n");
    let wide = repo.run_diff(repo.diff_command(2, None));
    let wide_files = wide.files();
    assert_eq!(1, wide_files.len());
    assert_eq!(
        vec![
            (HunkLineKind::Context, String::from("l3")),
            (HunkLineKind::Context, String::from("l4")),
            (HunkLineKind::Delete, String::from("l5")),
            (HunkLineKind::Insert, String::from("L5")),
            (HunkLineKind::Context, String::from("l6")),
            (HunkLineKind::Context, String::from("l7")),
        ],
        hunk_lines(wide_files[0])
    );
}

fn check_diff_command_rev(repo: &Repo) {
    let dir = repo.path();
    write_file(dir, "a.txt", "base\n");
    let c1 = repo.commit("base");
    write_file(dir, "a.txt", "base\nsecond\n");
    repo.commit("second");
    write_file(dir, "a.txt", "base\nsecond\nworktree\n");
    let since_c1 = repo.run_diff(repo.diff_command(3, Some(&c1)));
    let files = since_c1.files();
    assert_eq!(1, files.len());
    assert_eq!(
        vec![
            (HunkLineKind::Context, String::from("base")),
            (HunkLineKind::Insert, String::from("second")),
            (HunkLineKind::Insert, String::from("worktree")),
        ],
        hunk_lines(files[0])
    );
}

fn check_diff_command_worktree_after_commit(repo: &Repo) {
    let dir = repo.path();
    write_file(dir, "a.txt", "base\n");
    repo.commit("base");
    write_file(dir, "a.txt", "base\nsecond\n");
    repo.commit("second");
    write_file(dir, "a.txt", "base\nsecond\nworktree\n");
    let worktree_only = repo.run_diff(repo.diff_command(3, None));
    let worktree_files = worktree_only.files();
    assert_eq!(1, worktree_files.len());
    assert_eq!(
        vec![
            (HunkLineKind::Context, String::from("base")),
            (HunkLineKind::Context, String::from("second")),
            (HunkLineKind::Insert, String::from("worktree")),
        ],
        hunk_lines(worktree_files[0])
    );
}

fn check_commit_diff_command_one_commit(repo: &Repo) {
    let dir = repo.path();
    write_file(dir, "a.txt", "one\ntwo\nthree\n");
    repo.commit("first");
    write_file(dir, "a.txt", "one\nTWO\nthree\n");
    let c2 = repo.commit("second");

    let stdout = repo.run_ok(&repo.commit_diff_command(3, &c2));
    assert_commit_diff_preamble(repo, &stdout, "second");
    let diff = parse_diff(&stdout);
    let files = diff.files();
    assert_eq!(1, files.len());
    assert_eq!(Some(String::from("a.txt")), files[0].old);
    assert_eq!(Some(String::from("a.txt")), files[0].new);
    assert_eq!(
        vec![
            (HunkLineKind::Context, String::from("one")),
            (HunkLineKind::Delete, String::from("two")),
            (HunkLineKind::Insert, String::from("TWO")),
            (HunkLineKind::Context, String::from("three")),
        ],
        hunk_lines(files[0])
    );
}

fn check_commit_diff_command_root(repo: &Repo) {
    let dir = repo.path();
    write_file(dir, "a.txt", "one\ntwo\n");
    let c1 = repo.commit("root");
    write_file(dir, "a.txt", "one\ntwo\nworktree\n");

    let stdout = repo.run_ok(&repo.commit_diff_command(3, &c1));
    assert_commit_diff_preamble(repo, &stdout, "root");
    let diff = parse_diff(&stdout);
    let files = diff.files();
    assert_eq!(1, files.len());
    assert_eq!(None, files[0].old);
    assert_eq!(Some(String::from("a.txt")), files[0].new);
    assert_eq!(
        vec![
            (HunkLineKind::Insert, String::from("one")),
            (HunkLineKind::Insert, String::from("two")),
        ],
        hunk_lines(files[0])
    );
}

fn check_commit_diff_command_rename(repo: &Repo) {
    let dir = repo.path();
    write_file(dir, "old.txt", "hello\nworld\n");
    repo.commit("add");
    fs::rename(dir.join("old.txt"), dir.join("new.txt")).unwrap();
    let c2 = repo.commit("rename");

    let stdout = repo.run_ok(&repo.commit_diff_command(3, &c2));
    assert_commit_diff_preamble(repo, &stdout, "rename");
    let diff = parse_diff(&stdout);
    let files = diff.files();
    assert_eq!(1, files.len());
    assert_eq!(Some(String::from("old.txt")), files[0].old);
    assert_eq!(Some(String::from("new.txt")), files[0].new);
    assert_eq!(0, files[0].hunks.len());
}

fn check_untracked_command_lists_paths(repo: &Repo) {
    let dir = repo.path();
    write_file(dir, "tracked.txt", "t\n");
    write_file(dir, ".gitignore", "ignored.txt\n");
    repo.commit("tracked");
    write_file(dir, "untracked.txt", "u\n");
    write_file(dir, "sub/nested.txt", "n\n");
    write_file(dir, "ignored.txt", "i\n");

    let bytes = repo.run_ok(&repo.untracked_command());
    assert_eq!(
        vec![
            String::from("sub/nested.txt"),
            String::from("untracked.txt")
        ],
        parse_untracked_paths(&bytes)
    );
}

/// Working tree plus an isolated HOME so git and sl do not read the
/// real user config.
struct Repo {
    inner: TestRepo,
    kind: Kind,
}

impl Repo {
    fn init(kind: Kind) -> Repo {
        let sl_config = match kind {
            Kind::Git => false,
            Kind::Sl => true,
            Kind::SlOnGit => true,
        };
        let inner = TestRepo::prepare(sl_config);
        let repo = Repo { inner, kind };
        match kind {
            Kind::Git => {
                repo.ok("git", &["-c", "init.defaultBranch=master", "init", "-q"]);
                assert_eq!(true, repo.path().join(".git").is_dir());
                assert_eq!(false, repo.path().join(".sl").exists());
            }
            Kind::Sl => {
                repo.ok("sl", &["init", "-q"]);
                assert_eq!(true, repo.path().join(".sl").is_dir());
                assert_eq!(false, repo.path().join(".git").exists());
            }
            Kind::SlOnGit => {
                repo.ok("git", &["-c", "init.defaultBranch=master", "init", "-q"]);
                // Any sl command in a git repo creates `.git/sl`.
                repo.ok("sl", &["status"]);
                assert_eq!(true, repo.path().join(".git/sl").is_dir());
                assert_eq!(false, repo.path().join(".sl").exists());
            }
        }
        repo
    }

    fn path(&self) -> &Path {
        self.inner.path()
    }

    fn diff_command(&self, context: u32, rev: Option<&str>) -> Vec<String> {
        match self.kind {
            Kind::Git => git_diff_command(context, rev),
            Kind::Sl => sl_diff_command(context, rev),
            Kind::SlOnGit => sl_diff_command(context, rev),
        }
    }

    fn commit_diff_command(&self, context: u32, commit: &str) -> Vec<String> {
        match self.kind {
            Kind::Git => git_commit_diff_command(context, commit),
            Kind::Sl => sl_commit_diff_command(context, commit),
            Kind::SlOnGit => sl_commit_diff_command(context, commit),
        }
    }

    fn untracked_command(&self) -> Vec<String> {
        match self.kind {
            Kind::Git => git_untracked_command(),
            Kind::Sl => sl_untracked_command(),
            Kind::SlOnGit => sl_untracked_command(),
        }
    }

    fn commit(&self, message: &str) -> String {
        match self.kind {
            Kind::Git => {
                self.ok("git", &["add", "-A"]);
                self.ok("git", &["commit", "-q", "-m", message]);
                let hash = self.ok("git", &["rev-parse", "HEAD"]);
                hash.trim().to_owned()
            }
            Kind::Sl => self.inner.sl_commit(message),
            Kind::SlOnGit => self.inner.sl_commit(message),
        }
    }

    fn ok(&self, program: &str, args: &[&str]) -> String {
        self.inner.ok(program, args)
    }

    fn run_diff(&self, argv: Vec<String>) -> DiffOutput {
        let stdout = self.inner.run_ok(&argv);
        parse_diff(&stdout)
    }

    fn run_ok(&self, argv: &[String]) -> Vec<u8> {
        self.inner.run_ok(argv)
    }
}

fn write_file(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Header before `diff --git`, compared for equality. Commit ids
/// (and sl's isodate) are masked.
fn assert_commit_diff_preamble(repo: &Repo, stdout: &[u8], message: &str) {
    let text = String::from_utf8_lossy(stdout);
    let Some(diff_at) = text.find("diff --git ") else {
        panic!("commit output had no diff --git header:\n{text}");
    };
    let preamble = &text[..diff_at];
    let expected = match repo.kind {
        Kind::Git => format!(
            "\
commit HASH
Author: Live Diff Test <live-diff-test@example.com>
Date:   Wed Jan 1 00:00:00 2020 +0000

    {message}

"
        ),
        Kind::Sl | Kind::SlOnGit => format!(
            "\
commit:      HASH
user:        Live Diff Test <live-diff-test@example.com>
date:        DATE
description:
{message}

"
        ),
    };
    assert_eq!(expected, mask_commit_diff_preamble(repo.kind, preamble));
}

fn mask_commit_diff_preamble(kind: Kind, preamble: &str) -> String {
    let mut out = String::new();
    for line in preamble.lines() {
        match kind {
            Kind::Git => {
                if line.starts_with("commit ") {
                    out.push_str("commit HASH\n");
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Kind::Sl | Kind::SlOnGit => {
                if line.starts_with("commit:") {
                    out.push_str("commit:      HASH\n");
                } else if line.starts_with("date:") {
                    out.push_str("date:        DATE\n");
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    out
}

fn hunk_lines(file: &FileDiff) -> Vec<(HunkLineKind, String)> {
    let mut lines = Vec::new();
    for hunk in &file.hunks {
        for line in &hunk.lines {
            lines.push((line.kind, line.text.clone()));
        }
    }
    lines
}
