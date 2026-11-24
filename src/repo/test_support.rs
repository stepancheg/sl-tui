use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

/// Temporary repository with an isolated HOME so git and sl do not
/// read the real user config.
pub(crate) struct TestRepo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl TestRepo {
    /// Create `<tmpdir>/{repo,home}` and, when `sl_config` is set, a
    /// per-user Sapling config under that HOME.
    pub(crate) fn prepare(sl_config: bool) -> TestRepo {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let root = dir.path().join("repo");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&root).unwrap();
        let repo = TestRepo {
            _dir: dir,
            root,
            home,
        };
        if sl_config {
            repo.write_user_sl_config();
        }
        repo
    }

    pub(crate) fn init_sl() -> TestRepo {
        let repo = TestRepo::prepare(true);
        repo.ok("sl", &["init", "-q"]);
        repo
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    pub(crate) fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// `sl addremove` + `sl commit -m`, then the full node id of `.`.
    pub(crate) fn sl_commit(&self, message: &str) -> String {
        self.ok("sl", &["addremove"]);
        self.ok("sl", &["commit", "-m", message]);
        let hash = self.ok("sl", &["log", "-r", ".", "-T", "{node}\n"]);
        hash.trim().to_owned()
    }

    pub(crate) fn ok(&self, program: &str, args: &[&str]) -> String {
        String::from_utf8(self.run_program(program, args)).unwrap()
    }

    pub(crate) fn run_ok(&self, argv: &[String]) -> Vec<u8> {
        let Some((program, args)) = argv.split_first() else {
            panic!("empty command");
        };
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_program(program, &arg_refs)
    }

    fn run_program(&self, program: &str, args: &[&str]) -> Vec<u8> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        apply_env(&mut cmd, &self.root, &self.home);
        let out = cmd.output().unwrap();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            panic!("command {program} {args:?} failed\nstdout: {stdout}\nstderr: {stderr}");
        }
        out.stdout
    }

    fn write_user_sl_config(&self) {
        // Builtin config turns fsmonitor on. The user file has to
        // override that for commands that cannot pass `--config`.
        // cwd is HOME so configfile cannot latch onto a parent repo.
        let mut cmd = Command::new("sl");
        cmd.args(["configfile"]);
        apply_env(&mut cmd, &self.home, &self.home);
        let out = cmd.output().unwrap();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            panic!("sl configfile failed: {stderr}");
        }
        let text = String::from_utf8(out.stdout).unwrap();
        let mut user_path = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("User config path:") {
                user_path = Some(PathBuf::from(rest.trim()));
            }
        }
        let Some(user_path) = user_path else {
            panic!("sl configfile did not print a user config path:\n{text}");
        };
        if let Some(parent) = user_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(
            &user_path,
            "\
[ui]
username = Live Diff Test <live-diff-test@example.com>
color = never
paginate = never
[fsmonitor]
mode = off
",
        )
        .unwrap();
    }
}

/// Clear the environment, keep PATH, and point HOME and git/sl
/// identity at the fixture.
fn apply_env(cmd: &mut Command, root: &Path, home: &Path) {
    cmd.current_dir(root).stdin(Stdio::null()).env_clear();
    if let Some(path) = env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Live Diff Test")
        .env("GIT_AUTHOR_EMAIL", "live-diff-test@example.com")
        .env("GIT_COMMITTER_NAME", "Live Diff Test")
        .env("GIT_COMMITTER_EMAIL", "live-diff-test@example.com")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00+0000")
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00+0000")
        .env("LC_ALL", "C");
}
