/// Working-tree or `-r` diff.
///
/// `diff.showfunc=true` puts the enclosing function on the hunk
/// header. The sl default is off; the TUI always wants it.
pub(crate) fn sl_diff_command(context: u32, rev: Option<&str>) -> Vec<String> {
    let mut cmd = vec![
        String::from("sl"),
        String::from("diff"),
        String::from("-U"),
        context.to_string(),
        String::from("--config"),
        String::from("diff.showfunc=true"),
    ];
    if let Some(rev) = rev {
        cmd.push(String::from("-r"));
        cmd.push(rev.to_owned());
    }
    cmd
}

/// Commit message and diff introduced by one commit.
///
/// `sl log -r REV --patch`, not `sl show`: show's header includes a
/// `files:` list. The template keeps a `description:` block (full
/// message) and omits that list. `diff.unified` sets patch context;
/// `log` has no `-U`. `diff.showfunc=true` puts the enclosing
/// function on the hunk header. The sl default is off; the TUI
/// always wants it.
pub(crate) fn sl_commit_diff_command(context: u32, commit: &str) -> Vec<String> {
    // Newlines in the template are real newlines; sl accepts that.
    let template = "\
commit:      {node|short}
user:        {author}
date:        {date|isodate}
description:
{desc}

";
    vec![
        String::from("sl"),
        String::from("log"),
        String::from("-r"),
        commit.to_owned(),
        String::from("--patch"),
        String::from("--config"),
        format!("diff.unified={context}"),
        String::from("--config"),
        String::from("diff.showfunc=true"),
        String::from("-T"),
        String::from(template),
    ]
}

/// List untracked, non-ignored paths. Output is NUL-separated and
/// relative to the repository root.
pub(crate) fn sl_untracked_command() -> Vec<String> {
    vec![
        String::from("sl"),
        String::from("status"),
        String::from("-u"),
        String::from("-n"),
        String::from("-0"),
        String::from("--root-relative"),
    ]
}

#[cfg(test)]
mod tests {
    use crate::repo::sl::command::sl_commit_diff_command;
    use crate::repo::sl::command::sl_diff_command;
    use crate::repo::test_support::TestRepo;

    /// Raw stdout of `sl_commit_diff_command` in a native `.sl` repo.
    #[test]
    fn test_sl_commit_diff_command_output_sl_mode() {
        check_sl_commit_diff_command_output(&TestRepo::init_sl());
    }

    /// Raw stdout of `sl_commit_diff_command` in a git repo with `.git/sl`.
    #[test]
    fn test_sl_commit_diff_command_output_git_mode() {
        let repo = TestRepo::prepare(true);
        repo.ok("git", &["-c", "init.defaultBranch=master", "init", "-q"]);
        // Any sl command in a git repo creates `.git/sl`.
        repo.ok("sl", &["status"]);
        check_sl_commit_diff_command_output(&repo);
    }

    /// Raw stdout of `sl_diff_command`. The changed line is outside
    /// `-U3` of `fn target`, so that name is on the @@ line only
    /// because of showfunc.
    #[test]
    fn test_sl_diff_command_output_sl_mode() {
        check_sl_diff_command_output(&TestRepo::init_sl());
    }

    #[test]
    fn test_sl_diff_command_output_git_mode() {
        let repo = TestRepo::prepare(true);
        repo.ok("git", &["-c", "init.defaultBranch=master", "init", "-q"]);
        repo.ok("sl", &["status"]);
        check_sl_diff_command_output(&repo);
    }

    fn check_sl_diff_command_output(repo: &TestRepo) {
        repo.write("lib.rs", FN_BEFORE);
        repo.sl_commit("first");
        repo.write("lib.rs", FN_AFTER);
        let stdout = repo.run_ok(&sl_diff_command(3, None));
        let text = String::from_utf8(stdout).unwrap();
        let expected = "\
diff --git a/lib.rs b/lib.rs
--- a/lib.rs
+++ b/lib.rs
@@ -3,5 +3,5 @@ fn target() {
     let b = 2;
     let c = 3;
     let d = 4;
-    let e = 5;
+    let e = 50;
 }
";
        assert_eq!(expected, text);
    }

    /// Header template and patch. Digests and the date are masked;
    /// everything else, including the function on the @@ line, is
    /// compared exactly. The changed line is outside `-U3` of
    /// `fn target`, so that name is on the header only because of
    /// showfunc.
    fn check_sl_commit_diff_command_output(repo: &TestRepo) {
        repo.write("lib.rs", FN_BEFORE);
        repo.sl_commit("first");
        repo.write("lib.rs", FN_AFTER);
        let c2 = repo.sl_commit("second");
        let stdout = repo.run_ok(&sl_commit_diff_command(3, &c2));
        let text = String::from_utf8(stdout).unwrap();
        let expected = "\
commit:      HASH
user:        Live Diff Test <live-diff-test@example.com>
date:        DATE
description:
second

diff --git a/lib.rs b/lib.rs
--- a/lib.rs
+++ b/lib.rs
@@ -3,5 +3,5 @@ fn target() {
     let b = 2;
     let c = 3;
     let d = 4;
-    let e = 5;
+    let e = 50;
 }

";
        assert_eq!(expected, mask_sl_commit_diff_output(&text));
    }

    /// Change on `let e` is four body lines below `fn target`.
    const FN_BEFORE: &str = "\
fn target() {
    let a = 1;
    let b = 2;
    let c = 3;
    let d = 4;
    let e = 5;
}
";
    const FN_AFTER: &str = "\
fn target() {
    let a = 1;
    let b = 2;
    let c = 3;
    let d = 4;
    let e = 50;
}
";

    /// `label` + spaces stay; the field value is replaced.
    fn mask_field_value(line: &str, placeholder: &str) -> String {
        let Some(colon) = line.find(':') else {
            return String::from(line);
        };
        let after = &line[colon + 1..];
        let pad = after.len() - after.trim_start().len();
        let mut out = String::from(&line[..=colon]);
        out.push_str(&after[..pad]);
        out.push_str(placeholder);
        out
    }

    fn mask_sl_commit_diff_output(text: &str) -> String {
        let mut out = String::new();
        for line in text.lines() {
            if line.starts_with("commit:") {
                // Keep the label and spacing; mask only the digest.
                out.push_str(&mask_field_value(line, "HASH"));
                out.push('\n');
            } else if line.starts_with("date:") {
                // Keep the label and spacing; mask only the timestamp.
                out.push_str(&mask_field_value(line, "DATE"));
                out.push('\n');
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }
}
