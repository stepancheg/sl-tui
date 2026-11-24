use bstr::ByteSlice;
use unicode_width::UnicodeWidthChar;

use crate::terminal::esc::strip_esc;
use crate::terminal::line::Line;
use crate::terminal::parse::parse_command_output;
use crate::terminal::style::Style;
use crate::theme::diff_delete_style;
use crate::theme::diff_file_header_style;
use crate::theme::diff_hunk_header_style;
use crate::theme::diff_insert_style;
use crate::theme::diff_note_style;

/// One file in a diff. `old` / `new` are `None` when that side is
/// `/dev/null` (a new or deleted file).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileDiff {
    pub(crate) old: Option<String>,
    pub(crate) new: Option<String>,
    pub(crate) binary: bool,
    pub(crate) hunks: Vec<Hunk>,
}

/// A single `@@` hunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Hunk {
    pub(crate) old_start: u32,
    pub(crate) old_len: u32,
    pub(crate) new_start: u32,
    pub(crate) new_len: u32,
    /// Text after the closing `@@`, usually a function name. Empty if absent.
    pub(crate) section: String,
    pub(crate) lines: Vec<HunkLine>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HunkLine {
    pub(crate) kind: HunkLineKind,
    /// Line text without the diff prefix (` `, `-`, `+`). For
    /// [`HunkLineKind::Note`] this is the full plain line.
    pub(crate) text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HunkLineKind {
    Context,
    Delete,
    Insert,
    /// Not a context/delete/insert line (`\ No newline at end of file`,
    /// or a combined-diff body line).
    Note,
}

/// Command output as structured file diffs plus any non-diff lines,
/// in the order they appeared.
///
/// `untracked` is filled in by the capture loop (not by [`parse_diff`]):
/// working-copy paths that are not tracked and not ignored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiffOutput {
    segments: Vec<Segment>,
    untracked: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Segment {
    /// A non-diff line, still carrying any original ANSI so it can be
    /// drawn as the command printed it.
    Text(String),
    File(FileDiff),
}

/// Line range of a file header in the projected view. `start..end` are
/// indices into the view's lines. Derived from [`FileDiff`], not from
/// scanning styled rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileHeaderSpan {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Identity of a diff file, used to restore the viewport across refreshes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileKey {
    pub(crate) old: Option<String>,
    pub(crate) new: Option<String>,
}

impl FileKey {
    pub(crate) fn of(file: &FileDiff) -> FileKey {
        FileKey {
            old: file.old.clone(),
            new: file.new.clone(),
        }
    }
}

/// Display lines built from a [`DiffOutput`], plus the header span of
/// each file in those lines.
pub(crate) struct DiffView {
    pub(crate) lines: Vec<Line>,
    pub(crate) headers: Vec<FileHeaderSpan>,
    /// Parallel to `headers`.
    pub(crate) keys: Vec<FileKey>,
}

impl DiffOutput {
    /// A message that is not a diff (capture error, etc.).
    pub(crate) fn message(text: &str) -> DiffOutput {
        let mut segments = Vec::new();
        for line in split_raw_lines(text.as_bytes()) {
            segments.push(Segment::Text(line.raw));
        }
        DiffOutput {
            segments,
            untracked: Vec::new(),
        }
    }

    pub(crate) fn set_untracked(&mut self, untracked: Vec<String>) {
        self.untracked = untracked;
    }

    pub(crate) fn untracked(&self) -> &[String] {
        &self.untracked
    }

    /// Rows on the diff-stat page: changed files, then untracked paths.
    pub(crate) fn stat_row_count(&self) -> usize {
        self.files().len().saturating_add(self.untracked.len())
    }

    /// Non-diff lines before the first file (the `git show` / `sl show`
    /// header when viewing one commit).
    pub(crate) fn preamble_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for segment in &self.segments {
            match segment {
                Segment::Text(line) => lines.push(line.clone()),
                Segment::File(_) => break,
            }
        }
        lines
    }

    pub(crate) fn files(&self) -> Vec<&FileDiff> {
        let mut files = Vec::new();
        for segment in &self.segments {
            if let Segment::File(file) = segment {
                files.push(file);
            }
        }
        files
    }

    /// Project this snapshot to styled lines. Diff content is drawn from
    /// the [`FileDiff`] fields; non-diff text is drawn from its original
    /// bytes.
    pub(crate) fn view(&self) -> DiffView {
        let mut lines = Vec::new();
        let mut headers = Vec::new();
        let mut keys = Vec::new();
        let mut i = 0;
        while i < self.segments.len() {
            match &self.segments[i] {
                Segment::Text(_) => {
                    // Parse a run of non-diff lines as one stream so SGR
                    // state carries across lines, as the command emitted it.
                    let mut raw = String::new();
                    while i < self.segments.len() {
                        match &self.segments[i] {
                            Segment::Text(line) => {
                                raw.push_str(line);
                                raw.push('\n');
                                i += 1;
                            }
                            Segment::File(_) => break,
                        }
                    }
                    lines.extend(parse_command_output(raw.as_bytes()));
                }
                Segment::File(file) => {
                    let start = lines.len();
                    lines.push(Line::from_styled(
                        &file_header_text(file),
                        diff_file_header_style(),
                    ));
                    let end = lines.len();
                    headers.push(FileHeaderSpan { start, end });
                    keys.push(FileKey::of(file));
                    push_file_body(file, &mut lines);
                    i += 1;
                }
            }
        }
        if lines.is_empty() {
            // A clean empty diff would otherwise be a blank screen.
            lines.push(Line::from_styled("(no changes)", diff_note_style()));
        }
        DiffView {
            lines,
            headers,
            keys,
        }
    }
}

/// Parse raw command output into file diffs.
///
/// ANSI escapes are stripped before any structural match. The terminal
/// line parser is not used here.
pub(crate) fn parse_diff(input: &[u8]) -> DiffOutput {
    let raw_lines = split_raw_lines(input);
    let mut segments = Vec::new();
    let mut i = 0;
    while i < raw_lines.len() {
        if let Some((file, next)) = parse_file(&raw_lines, i) {
            segments.push(Segment::File(file));
            i = next;
            continue;
        }
        segments.push(Segment::Text(raw_lines[i].raw.clone()));
        i += 1;
    }
    DiffOutput {
        segments,
        untracked: Vec::new(),
    }
}

impl FileDiff {
    fn change_counts(&self) -> (u32, u32) {
        let mut ins = 0u32;
        let mut del = 0u32;
        for hunk in &self.hunks {
            for line in &hunk.lines {
                match line.kind {
                    HunkLineKind::Insert => ins = ins.saturating_add(1),
                    HunkLineKind::Delete => del = del.saturating_add(1),
                    HunkLineKind::Context | HunkLineKind::Note => {}
                }
            }
        }
        (ins, del)
    }
}

/// One row per file for the file-list mode, in `git diff --stat` form:
/// a path column, `|`, the change count, and a `+`/`-` bar.
/// `selected` is drawn with a grey / dark-grey background.
/// Commit message taken from a `git log -1 -p` / `sl log --patch` preamble.
/// Git indents the message with four spaces; sl emits a `description:`
/// block. Trailing blank lines are dropped. Empty if the preamble is
/// not a commit header.
pub(crate) fn commit_description_lines(preamble: &[String]) -> Vec<String> {
    if preamble.is_empty() {
        return Vec::new();
    }
    let head = preamble[0].as_str();
    let sl_header = head.starts_with("commit:");
    let git_header = head.starts_with("commit ");
    if !sl_header {
        if !git_header {
            return Vec::new();
        }
    }
    if sl_header {
        for (i, line) in preamble.iter().enumerate() {
            let trimmed = line.trim_end();
            if trimmed == "description:" {
                let mut body = preamble[i + 1..].to_vec();
                trim_trailing_blank_lines(&mut body);
                return body;
            }
            if let Some(rest) = trimmed.strip_prefix("description:") {
                let mut body = Vec::new();
                let rest = rest.trim_start();
                if !rest.is_empty() {
                    body.push(rest.to_owned());
                }
                body.extend(preamble[i + 1..].iter().cloned());
                trim_trailing_blank_lines(&mut body);
                return body;
            }
        }
        return Vec::new();
    }
    // git show: the message is the 4-space-indented block.
    let mut start = None;
    for (i, line) in preamble.iter().enumerate() {
        if line.starts_with("    ") {
            start = Some(i);
            break;
        }
    }
    let Some(start) = start else {
        return Vec::new();
    };
    let mut body = Vec::new();
    for line in &preamble[start..] {
        if let Some(rest) = line.strip_prefix("    ") {
            body.push(rest.to_owned());
        } else {
            break;
        }
    }
    trim_trailing_blank_lines(&mut body);
    body
}

fn trim_trailing_blank_lines(lines: &mut Vec<String>) {
    while let Some(last) = lines.last() {
        if !last.is_empty() {
            break;
        }
        lines.pop();
    }
}

/// How many list rows [`file_list_lines`] prepends for `description`.
pub(crate) fn file_list_description_rows(description: &[String]) -> usize {
    if description.is_empty() {
        return 0;
    }
    // The message, then a blank line before the stat.
    description.len().saturating_add(1)
}

pub(crate) fn file_list_lines(
    files: &[&FileDiff],
    untracked: &[String],
    selected: usize,
    terminal_bg: crate::terminal_bg::TerminalBg,
    width: crate::terminal::width::Width,
    description: &[String],
) -> Vec<Line> {
    let mut lines = Vec::new();
    for (i, text) in description.iter().enumerate() {
        let style = if i == 0 {
            diff_file_header_style()
        } else {
            Style::plain()
        };
        lines.push(Line::from_styled(text, style));
    }
    if !description.is_empty() {
        lines.push(Line::from_styled("", Style::plain()));
    }
    if files.is_empty() {
        if untracked.is_empty() {
            lines.push(Line::from_styled("(no files)", diff_note_style()));
            return lines;
        }
    }
    let cols = usize::try_from(width.0).unwrap_or(0);
    let names: Vec<String> = files.iter().map(|file| file_header_text(file)).collect();
    let counts: Vec<(u32, u32)> = files.iter().map(|file| file.change_counts()).collect();
    let max_change = counts
        .iter()
        .map(|(ins, del)| ins.saturating_add(*del))
        .max()
        .unwrap_or(0);
    let any_binary = files.iter().any(|file| file.binary);
    // "Bin" needs 3 columns; otherwise the decimal width of max_change.
    let num_w = decimal_width(max_change).max(if any_binary { 3 } else { 1 });
    // "> " / "  " plus " | " plus the count plus a space before the bar.
    let cursor_w = 2usize;
    // The count column is `num_w` plus a space. Untracked rows put the
    // tag in that right-hand area, so reserve the tag width when any
    // are present and the bar will sit in whatever is left.
    let right_w = if untracked.is_empty() {
        num_w + 1
    } else {
        let stat_right = num_w + 1;
        if stat_right < UNTRACKED_TAG_WIDTH {
            UNTRACKED_TAG_WIDTH
        } else {
            stat_right
        }
    };
    let fixed = cursor_w + 3 + right_w;
    let remain = cols.saturating_sub(fixed);
    let max_name = names
        .iter()
        .map(|name| unicode_width::UnicodeWidthStr::width(name.as_str()))
        .chain(
            untracked
                .iter()
                .map(|name| unicode_width::UnicodeWidthStr::width(name.as_str())),
        )
        .max()
        .unwrap_or(0);
    // Fit the longest path when possible and give the bar the rest.
    // Otherwise keep a short bar and truncate paths.
    let (name_w, graph_w) = if max_name <= remain {
        (max_name, remain - max_name)
    } else if remain == 0 {
        (0, 0)
    } else {
        let graph_w = (remain / 3).max(1).min(remain.saturating_sub(1));
        (remain - graph_w, graph_w)
    };

    for (i, file) in files.iter().enumerate() {
        let selected_row = i == selected;
        let mut name_style = diff_file_header_style();
        let mut plain_style = Style::plain();
        let mut ins_style = diff_insert_style();
        let mut del_style = diff_delete_style();
        if selected_row {
            let bg = Some(terminal_bg.row_highlight());
            name_style.bg = bg;
            plain_style.bg = bg;
            ins_style.bg = bg;
            del_style.bg = bg;
        }
        let mut line = Line::new();
        line.push_styled(if selected_row { "> " } else { "  " }, name_style);
        let shown = fit_name(&names[i], name_w);
        line.push_styled(&shown, name_style);
        let pad = name_w.saturating_sub(unicode_width::UnicodeWidthStr::width(shown.as_str()));
        if pad > 0 {
            line.push_styled(&" ".repeat(pad), name_style);
        }
        line.push_styled(" | ", plain_style);
        if file.binary {
            line.push_styled(&format!("{:>width$}", "Bin", width = num_w), plain_style);
        } else {
            let (ins, del) = counts[i];
            let total = ins.saturating_add(del);
            line.push_styled(&format!("{total:>num_w$}"), plain_style);
            line.push_styled(" ", plain_style);
            let (plus, minus) = stat_graph(ins, del, max_change, graph_w);
            if plus > 0 {
                line.push_styled(&"+".repeat(plus), ins_style);
            }
            if minus > 0 {
                line.push_styled(&"-".repeat(minus), del_style);
            }
        }
        if selected_row {
            let used = usize::try_from(line.width().0).unwrap_or(0);
            if used < cols {
                line.push_styled(&" ".repeat(cols - used), plain_style);
            }
        }
        lines.push(line);
    }
    // Untracked rows follow the stat and share its name column so `|`
    // lines up.
    for (j, path) in untracked.iter().enumerate() {
        let selected_row = files.len().saturating_add(j) == selected;
        push_untracked_list_row(&mut lines, path, selected_row, name_w, cols, terminal_bg);
    }
    lines
}

const UNTRACKED_TAG: &str = "untracked";
const UNTRACKED_TAG_WIDTH: usize = 9;

fn push_untracked_list_row(
    lines: &mut Vec<Line>,
    path: &str,
    selected_row: bool,
    name_w: usize,
    cols: usize,
    terminal_bg: crate::terminal_bg::TerminalBg,
) {
    let cursor_w = 2usize;
    let pipe_w = 3usize;
    let overhead = cursor_w
        .saturating_add(pipe_w)
        .saturating_add(UNTRACKED_TAG_WIDTH);
    let mut name_style = diff_file_header_style();
    let mut plain_style = Style::plain();
    let mut tag_style = diff_note_style();
    if selected_row {
        let bg = Some(terminal_bg.row_highlight());
        name_style.bg = bg;
        plain_style.bg = bg;
        tag_style.bg = bg;
    }
    let mut line = Line::new();
    line.push_styled(if selected_row { "> " } else { "  " }, name_style);
    // Keep the tag when the row can hold cursor + pipe + tag.
    // Shrink the name column if the aligned width would overflow.
    if overhead <= cols {
        let mut budget = name_w;
        if overhead.saturating_add(budget) > cols {
            budget = cols.saturating_sub(overhead);
        }
        let shown = fit_name(path, budget);
        line.push_styled(&shown, name_style);
        let pad = budget.saturating_sub(unicode_width::UnicodeWidthStr::width(shown.as_str()));
        if pad > 0 {
            line.push_styled(&" ".repeat(pad), name_style);
        }
        line.push_styled(" | ", plain_style);
        line.push_styled(UNTRACKED_TAG, tag_style);
    } else {
        let budget = cols.saturating_sub(cursor_w);
        let shown = fit_name(path, budget);
        line.push_styled(&shown, name_style);
    }
    if selected_row {
        let used = usize::try_from(line.width().0).unwrap_or(0);
        if used < cols {
            line.push_styled(&" ".repeat(cols - used), plain_style);
        }
    }
    lines.push(line);
}

fn decimal_width(n: u32) -> usize {
    if n == 0 {
        return 1;
    }
    let mut n = n;
    let mut w = 0;
    while n > 0 {
        w += 1;
        n /= 10;
    }
    w
}

/// Keep the right-hand side of `name` (the file name) when it must shrink.
fn fit_name(name: &str, max_w: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(name);
    if w <= max_w {
        return name.to_owned();
    }
    if max_w == 0 {
        return String::new();
    }
    if max_w < 4 {
        return take_suffix_width(name, max_w);
    }
    let mut out = String::from("...");
    out.push_str(&take_suffix_width(name, max_w - 3));
    out
}

fn take_suffix_width(name: &str, max_w: usize) -> String {
    let mut cols = 0usize;
    let mut start = name.len();
    for (i, ch) in name.char_indices().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cols + cw > max_w {
            break;
        }
        cols += cw;
        start = i;
    }
    name[start..].to_owned()
}

/// Split a `+`/`-` bar of at most `graph_w` columns between inserts and
/// deletes, scaled so the largest file fills the bar.
fn stat_graph(ins: u32, del: u32, max_change: u32, graph_w: usize) -> (usize, usize) {
    let total = ins.saturating_add(del);
    if total == 0 || graph_w == 0 || max_change == 0 {
        return (0, 0);
    }
    let total_us = usize::try_from(total).unwrap_or(usize::MAX);
    let max_us = usize::try_from(max_change).unwrap_or(usize::MAX);
    let marks = if max_us <= graph_w {
        total_us
    } else {
        ((total_us.saturating_mul(graph_w)) / max_us).max(1)
    };
    if del == 0 {
        return (marks, 0);
    }
    if ins == 0 {
        return (0, marks);
    }
    let ins_us = usize::try_from(ins).unwrap_or(usize::MAX);
    let mut plus = ins_us.saturating_mul(marks) / total_us;
    let mut minus = marks.saturating_sub(plus);
    if plus == 0 {
        plus = 1;
        minus = marks.saturating_sub(1);
    } else if minus == 0 {
        minus = 1;
        plus = marks.saturating_sub(1);
    }
    (plus, minus)
}

/// Index of the header span whose file section contains `line`.
///
/// A section runs from `headers[i].start` to `headers[i + 1].start`
/// (or the end of the view). Returns `None` when `line` is before the
/// first header.
pub(crate) fn file_header_index(headers: &[FileHeaderSpan], line: usize) -> Option<usize> {
    let i = headers.partition_point(|header| header.start <= line);
    if i == 0 {
        return None;
    }
    Some(i - 1)
}

struct RawLine {
    /// Original line without the trailing newline, ANSI intact.
    raw: String,
    /// `raw` with CSI/OSC/other escapes removed. Used for structure.
    plain: String,
}

fn split_raw_lines(input: &[u8]) -> Vec<RawLine> {
    let mut lines = Vec::new();
    for part in input.lines() {
        let raw = String::from_utf8_lossy(part).into_owned();
        let plain = strip_esc(&raw);
        lines.push(RawLine { raw, plain });
    }
    lines
}

fn parse_file(lines: &[RawLine], start: usize) -> Option<(FileDiff, usize)> {
    let plain = &lines[start].plain;
    let from_diff_cmd = is_diff_command_start(plain);
    let from_pair = is_unified_pair_start(lines, start);
    if !from_diff_cmd && !from_pair {
        return None;
    }

    let mut i = start;
    let mut old_path: Option<String> = None;
    let mut new_path: Option<String> = None;
    let mut old_is_null = false;
    let mut new_is_null = false;
    let mut rename_from: Option<String> = None;
    let mut rename_to: Option<String> = None;
    let mut git_old: Option<String> = None;
    let mut git_new: Option<String> = None;
    let mut rev_path: Option<String> = None;

    if from_diff_cmd {
        if plain.starts_with("diff --git ") {
            let (a, b) = parse_diff_git_paths(plain);
            git_old = a;
            git_new = b;
        } else if plain.starts_with("diff -r ") {
            rev_path = parse_diff_r_path(plain);
        } else if plain.starts_with("diff --cc ") || plain.starts_with("diff --combined ") {
            // Combined diffs: one path.
            let path = plain
                .strip_prefix("diff --cc ")
                .or_else(|| plain.strip_prefix("diff --combined "))
                .map(parse_path_field);
            git_old = path.clone();
            git_new = path;
        }
        i += 1;
    }

    // Metadata lines, including a leading ---/+++ pair.
    while i < lines.len() {
        let text = &lines[i].plain;
        if is_diff_command_start(text) {
            break;
        }
        if is_hunk_header(text) || is_binary_notice(text) || text.starts_with("GIT binary patch") {
            break;
        }
        if !is_header_meta(text) {
            // Blank lines between the header and the first hunk are
            // skipped. Anything else ends the header; leave it for the
            // body/outer parser.
            if text.is_empty() {
                i += 1;
                continue;
            }
            break;
        }
        if is_minus_file_line(text) {
            match path_from_marker(text) {
                None => old_is_null = true,
                Some(path) => {
                    old_path = Some(path);
                    old_is_null = false;
                }
            }
        } else if is_plus_file_line(text) {
            match path_from_marker(text) {
                None => new_is_null = true,
                Some(path) => {
                    new_path = Some(path);
                    new_is_null = false;
                }
            }
        } else if let Some(path) = text.strip_prefix("rename from ") {
            rename_from = Some(parse_path_field(path));
        } else if let Some(path) = text.strip_prefix("rename to ") {
            rename_to = Some(parse_path_field(path));
        } else if let Some(path) = text.strip_prefix("copy from ") {
            rename_from = Some(parse_path_field(path));
        } else if let Some(path) = text.strip_prefix("copy to ") {
            rename_to = Some(parse_path_field(path));
        }
        i += 1;
    }

    let mut binary = false;
    let mut hunks = Vec::new();
    let combined = lines[start].plain.starts_with("diff --cc ")
        || lines[start].plain.starts_with("diff --combined ");

    // Skip blanks before the body.
    while i < lines.len() && lines[i].plain.is_empty() {
        i += 1;
    }

    if i < lines.len() && is_binary_notice(&lines[i].plain) {
        binary = true;
        i += 1;
    } else if i < lines.len() && lines[i].plain.starts_with("GIT binary patch") {
        binary = true;
        i += 1;
        while i < lines.len() && !is_file_start(lines, i) {
            i += 1;
        }
    } else if combined {
        // Keep combined-diff body lines as notes inside one hunk so the
        // text is not dropped, and stop at the next file.
        let mut body = Vec::new();
        while i < lines.len() && !is_file_start(lines, i) {
            if is_hunk_header(&lines[i].plain) || lines[i].plain.starts_with("@@@") {
                // Fall through: store the line as a note.
            }
            body.push(HunkLine {
                kind: HunkLineKind::Note,
                text: lines[i].plain.clone(),
            });
            i += 1;
        }
        if !body.is_empty() {
            hunks.push(Hunk {
                old_start: 0,
                old_len: 0,
                new_start: 0,
                new_len: 0,
                section: String::new(),
                lines: body,
            });
        }
    } else {
        while i < lines.len() && is_hunk_header(&lines[i].plain) {
            let Some(hunk_header) = parse_hunk_header(&lines[i].plain) else {
                break;
            };
            i += 1;
            let (hunk_lines, next) =
                parse_hunk_lines(lines, i, hunk_header.old_len, hunk_header.new_len);
            i = next;
            hunks.push(Hunk {
                old_start: hunk_header.old_start,
                old_len: hunk_header.old_len,
                new_start: hunk_header.new_start,
                new_len: hunk_header.new_len,
                section: hunk_header.section,
                lines: hunk_lines,
            });
            while i < lines.len() && lines[i].plain.is_empty() {
                // A blank line is not part of the hunk (counts are done).
                // Leave it if the next non-blank is not another hunk, so
                // it stays visible; if another hunk follows, skip it.
                if i + 1 < lines.len() && is_hunk_header(&lines[i + 1].plain) {
                    i += 1;
                } else {
                    break;
                }
            }
        }
    }

    let old = if old_is_null {
        None
    } else {
        rename_from
            .or(old_path)
            .or(git_old)
            .or_else(|| rev_path.clone())
    };
    let new = if new_is_null {
        None
    } else {
        rename_to.or(new_path).or(git_new).or(rev_path)
    };

    // A diff command with no paths and no body is still a file, but a
    // bare ---/+++ pair that we entered must have been a real pair.
    Some((
        FileDiff {
            old,
            new,
            binary,
            hunks,
        },
        i,
    ))
}

struct HunkHeader {
    old_start: u32,
    old_len: u32,
    new_start: u32,
    new_len: u32,
    section: String,
}

fn parse_hunk_lines(
    lines: &[RawLine],
    start: usize,
    old_len: u32,
    new_len: u32,
) -> (Vec<HunkLine>, usize) {
    let mut out = Vec::new();
    let mut i = start;
    let mut old_left = old_len;
    let mut new_left = new_len;
    while (old_left > 0 || new_left > 0) && i < lines.len() {
        let text = &lines[i].plain;
        if let Some(rest) = strip_hunk_prefix(text, ' ') {
            if old_left == 0 || new_left == 0 {
                break;
            }
            out.push(HunkLine {
                kind: HunkLineKind::Context,
                text: rest,
            });
            old_left -= 1;
            new_left -= 1;
            i += 1;
        } else if let Some(rest) = strip_hunk_prefix(text, '-') {
            if old_left == 0 {
                break;
            }
            out.push(HunkLine {
                kind: HunkLineKind::Delete,
                text: rest,
            });
            old_left -= 1;
            i += 1;
        } else if let Some(rest) = strip_hunk_prefix(text, '+') {
            if new_left == 0 {
                break;
            }
            out.push(HunkLine {
                kind: HunkLineKind::Insert,
                text: rest,
            });
            new_left -= 1;
            i += 1;
        } else if text.starts_with('\\') {
            out.push(HunkLine {
                kind: HunkLineKind::Note,
                text: text.clone(),
            });
            i += 1;
        } else {
            break;
        }
    }
    // Counts do not include the marker, so it can sit after the last
    // counted line, once old_left and new_left are already 0.
    while i < lines.len() && lines[i].plain.starts_with('\\') {
        out.push(HunkLine {
            kind: HunkLineKind::Note,
            text: lines[i].plain.clone(),
        });
        i += 1;
    }
    (out, i)
}

/// Strip a one-character diff prefix. The prefix must be present; the
/// rest of the line (possibly empty) is the file text.
fn strip_hunk_prefix(text: &str, prefix: char) -> Option<String> {
    let mut chars = text.chars();
    if chars.next() == Some(prefix) {
        Some(chars.as_str().to_owned())
    } else {
        None
    }
}

fn parse_hunk_header(line: &str) -> Option<HunkHeader> {
    let mut s = line.strip_prefix("@@ ")?;
    s = s.strip_prefix('-')?;
    let (old_start, rest) = parse_u32_prefix(s)?;
    s = rest;
    let (old_len, rest) = match s.strip_prefix(',') {
        Some(rest) => parse_u32_prefix(rest)?,
        None => (1, s),
    };
    s = rest;
    s = s.strip_prefix(' ')?;
    s = s.strip_prefix('+')?;
    let (new_start, rest) = parse_u32_prefix(s)?;
    s = rest;
    let (new_len, rest) = match s.strip_prefix(',') {
        Some(rest) => parse_u32_prefix(rest)?,
        None => (1, s),
    };
    s = rest;
    s = s.strip_prefix(" @@")?;
    let section = match s.strip_prefix(' ') {
        Some(rest) => rest.trim_end().to_owned(),
        None => s.trim_end().to_owned(),
    };
    Some(HunkHeader {
        old_start,
        old_len,
        new_start,
        new_len,
        section,
    })
}

fn parse_u32_prefix(s: &str) -> Option<(u32, &str)> {
    let end = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let n = s[..end].parse::<u32>().ok()?;
    Some((n, &s[end..]))
}

fn is_file_start(lines: &[RawLine], index: usize) -> bool {
    is_diff_command_start(&lines[index].plain) || is_unified_pair_start(lines, index)
}

fn is_diff_command_start(text: &str) -> bool {
    text.starts_with("diff --git ")
        || text.starts_with("diff --cc ")
        || text.starts_with("diff --combined ")
        || text.starts_with("diff -r ")
}

fn is_unified_pair_start(lines: &[RawLine], index: usize) -> bool {
    let Some(next) = lines.get(index + 1) else {
        return false;
    };
    is_minus_file_line(&lines[index].plain) && is_plus_file_line(&next.plain)
}

fn is_hunk_header(text: &str) -> bool {
    text.starts_with("@@ ") || text.starts_with("@@\t")
}

fn is_binary_notice(text: &str) -> bool {
    text.starts_with("Binary files ") || text.starts_with("Binary file ")
}

fn is_header_meta(text: &str) -> bool {
    is_minus_file_line(text)
        || is_plus_file_line(text)
        || text.starts_with("old mode ")
        || text.starts_with("new mode ")
        || text.starts_with("deleted file mode ")
        || text.starts_with("new file mode ")
        || text.starts_with("copy from ")
        || text.starts_with("copy to ")
        || text.starts_with("rename from ")
        || text.starts_with("rename to ")
        || text.starts_with("similarity index ")
        || text.starts_with("dissimilarity index ")
        || text.starts_with("index ")
}

fn is_minus_file_line(text: &str) -> bool {
    // `--- ` / `---\t` are file markers. A marker with no space (`---`)
    // is not used by git or sl.
    text.starts_with("--- ") || text.starts_with("---\t")
}

fn is_plus_file_line(text: &str) -> bool {
    text.starts_with("+++ ") || text.starts_with("+++\t")
}

/// Path from a `---` / `+++` line. `None` means `/dev/null`.
fn path_from_marker(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("--- ")
        .or_else(|| line.strip_prefix("---\t"))
        .or_else(|| line.strip_prefix("+++ "))
        .or_else(|| line.strip_prefix("+++\t"))?;
    let path = parse_path_field(rest);
    if path == "/dev/null" {
        None
    } else {
        Some(strip_ab_prefix(&path).to_owned())
    }
}

fn parse_path_field(s: &str) -> String {
    let s = s.trim_end();
    let s = match s.split_once('\t') {
        Some((path, _)) => path,
        None => s,
    };
    let s = s.trim();
    if s.starts_with('"') {
        let (path, _) = take_quoted(s);
        return path;
    }
    s.to_owned()
}

fn take_quoted(s: &str) -> (String, &str) {
    let Some(rest) = s.strip_prefix('"') else {
        return (s.to_owned(), "");
    };
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((idx, c)) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some((_, escaped)) => out.push(match escaped {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    other => other,
                }),
                None => {
                    out.push('\\');
                    return (out, "");
                }
            }
            continue;
        }
        if c == '"' {
            let next = idx + 1;
            return (out, &rest[next..]);
        }
        out.push(c);
    }
    (out, "")
}

fn parse_diff_git_paths(line: &str) -> (Option<String>, Option<String>) {
    let Some(rest) = line.strip_prefix("diff --git ") else {
        return (None, None);
    };
    let (old, new) = parse_two_paths(rest);
    (normalize_side(&old), normalize_side(&new))
}

fn parse_two_paths(s: &str) -> (String, String) {
    let s = s.trim();
    if s.starts_with('"') {
        let (first, rest) = take_quoted(s);
        let rest = rest.trim_start();
        let second = if rest.starts_with('"') {
            take_quoted(rest).0
        } else {
            rest.to_owned()
        };
        return (first, second);
    }
    if s.starts_with("a/") {
        if let Some(pos) = s.find(" b/") {
            return (s[..pos].to_owned(), s[pos + 1..].to_owned());
        }
    }
    match s.rsplit_once(' ') {
        Some((old, new)) => (old.to_owned(), new.to_owned()),
        None => (s.to_owned(), s.to_owned()),
    }
}

fn parse_diff_r_path(line: &str) -> Option<String> {
    let mut rest = line.strip_prefix("diff -r ")?;
    // First revision token.
    let space = rest.find(' ')?;
    rest = &rest[space + 1..];
    if let Some(after) = rest.strip_prefix("-r ") {
        let space = after.find(' ')?;
        rest = &after[space + 1..];
    }
    let path = parse_path_field(rest);
    if path.is_empty() {
        None
    } else {
        Some(strip_ab_prefix(&path).to_owned())
    }
}

fn normalize_side(path: &str) -> Option<String> {
    let path = parse_path_field(path);
    if path == "/dev/null" {
        None
    } else {
        Some(strip_ab_prefix(&path).to_owned())
    }
}

fn strip_ab_prefix(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("a/") {
        rest
    } else if let Some(rest) = path.strip_prefix("b/") {
        rest
    } else {
        path
    }
}

pub(crate) fn file_header_text(file: &FileDiff) -> String {
    match (&file.old, &file.new) {
        (Some(old), Some(new)) if old == new => old.clone(),
        (Some(old), Some(new)) => format!("{old} -> {new}"),
        (None, Some(new)) => format!("{new} (new)"),
        (Some(old), None) => format!("{old} (deleted)"),
        (None, None) => String::from("(unknown)"),
    }
}

fn push_file_body(file: &FileDiff, lines: &mut Vec<Line>) {
    if file.binary {
        lines.push(Line::from_styled("Binary files differ", diff_note_style()));
    }
    for hunk in &file.hunks {
        if hunk.old_len == 0
            && hunk.new_len == 0
            && hunk.section.is_empty()
            && hunk_is_notes_only(hunk)
        {
            for line in &hunk.lines {
                lines.push(Line::from_styled(
                    &expand_tabs(&line.text),
                    diff_note_style(),
                ));
            }
            continue;
        }
        lines.push(Line::from_styled(
            &expand_tabs(&hunk_header_text(hunk)),
            diff_hunk_header_style(),
        ));
        for line in &hunk.lines {
            let (text, style) = match line.kind {
                HunkLineKind::Context => (
                    expand_tabs(&format!(" {text}", text = line.text)),
                    Style::plain(),
                ),
                HunkLineKind::Delete => (
                    expand_tabs(&format!("-{text}", text = line.text)),
                    diff_delete_style(),
                ),
                HunkLineKind::Insert => (
                    expand_tabs(&format!("+{text}", text = line.text)),
                    diff_insert_style(),
                ),
                HunkLineKind::Note => (expand_tabs(&line.text), diff_note_style()),
            };
            lines.push(Line::from_styled(&text, style));
        }
    }
}

fn hunk_is_notes_only(hunk: &Hunk) -> bool {
    !hunk.lines.is_empty()
        && hunk
            .lines
            .iter()
            .all(|line| line.kind == HunkLineKind::Note)
}

fn hunk_header_text(hunk: &Hunk) -> String {
    let mut s = String::from("@@ -");
    s.push_str(&hunk.old_start.to_string());
    if hunk.old_len != 1 {
        s.push(',');
        s.push_str(&hunk.old_len.to_string());
    }
    s.push_str(" +");
    s.push_str(&hunk.new_start.to_string());
    if hunk.new_len != 1 {
        s.push(',');
        s.push_str(&hunk.new_len.to_string());
    }
    s.push_str(" @@");
    if !hunk.section.is_empty() {
        s.push(' ');
        s.push_str(&hunk.section);
    }
    s
}

fn expand_tabs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut col = 0u32;
    for c in s.chars() {
        if c == '\t' {
            let spaces = 8 - (col % 8);
            for _ in 0..spaces {
                out.push(' ');
            }
            col = col.saturating_add(spaces);
            continue;
        }
        out.push(c);
        let w = u32::try_from(c.width_cjk().unwrap_or(0)).unwrap_or(0);
        col = col.saturating_add(w);
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::diff::FileDiff;
    use crate::diff::Hunk;
    use crate::diff::HunkLine;
    use crate::diff::HunkLineKind;
    use crate::diff::file_header_index;
    use crate::diff::parse_diff;

    fn files_of(input: &[u8]) -> Vec<FileDiff> {
        parse_diff(input).files().into_iter().cloned().collect()
    }

    fn line(kind: HunkLineKind, text: &str) -> HunkLine {
        HunkLine {
            kind,
            text: text.to_owned(),
        }
    }

    #[test]
    fn test_git_extended_two_files() {
        let input = b"\
diff --git a/foo.rs b/foo.rs
index 1111111..2222222 100644
--- a/foo.rs
+++ b/foo.rs
@@ -1,2 +1,2 @@
-old
+new
diff --git a/bar.rs b/bar.rs
new file mode 100644
--- /dev/null
+++ b/bar.rs
@@ -0,0 +1 @@
+hello
";
        assert_eq!(
            vec![
                FileDiff {
                    old: Some(String::from("foo.rs")),
                    new: Some(String::from("foo.rs")),
                    binary: false,
                    hunks: vec![Hunk {
                        old_start: 1,
                        old_len: 2,
                        new_start: 1,
                        new_len: 2,
                        section: String::new(),
                        lines: vec![
                            line(HunkLineKind::Delete, "old"),
                            line(HunkLineKind::Insert, "new"),
                        ],
                    }],
                },
                FileDiff {
                    old: None,
                    new: Some(String::from("bar.rs")),
                    binary: false,
                    hunks: vec![Hunk {
                        old_start: 0,
                        old_len: 0,
                        new_start: 1,
                        new_len: 1,
                        section: String::new(),
                        lines: vec![line(HunkLineKind::Insert, "hello")],
                    }],
                },
            ],
            files_of(input)
        );
    }

    #[test]
    fn test_colored_git_header_matches_plain() {
        let plain = b"\
diff --git a/foo b/foo
new file mode 100644
--- /dev/null
+++ b/foo
@@ -0,0 +1 @@
+x
";
        let colored = b"\
\x1b[1mdiff --git a/foo b/foo\x1b[0m
\x1b[1m\x1b[36mnew file mode 100644\x1b[0m
\x1b[1m\x1b[31m--- /dev/null\x1b[0m
\x1b[1m\x1b[32m+++ b/foo\x1b[0m
\x1b[35m@@ -0,0 +1 @@\x1b[39m
\x1b[32m+x\x1b[39m
";
        assert_eq!(files_of(plain), files_of(colored));
        assert_eq!(
            vec![FileDiff {
                old: None,
                new: Some(String::from("foo")),
                binary: false,
                hunks: vec![Hunk {
                    old_start: 0,
                    old_len: 0,
                    new_start: 1,
                    new_len: 1,
                    section: String::new(),
                    lines: vec![line(HunkLineKind::Insert, "x")],
                }],
            }],
            files_of(colored)
        );
    }

    #[test]
    fn test_sl_unified_header() {
        let input = b"\
diff -r 84ed658d4664 foo.rs
--- a/foo.rs\tThu Jan 01 00:00:00 1970 +0000
+++ b/foo.rs\tMon Nov 16 02:57:15 2026 +0000
@@ -1 +1 @@
-old
+new
";
        assert_eq!(
            vec![FileDiff {
                old: Some(String::from("foo.rs")),
                new: Some(String::from("foo.rs")),
                binary: false,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_len: 1,
                    new_start: 1,
                    new_len: 1,
                    section: String::new(),
                    lines: vec![
                        line(HunkLineKind::Delete, "old"),
                        line(HunkLineKind::Insert, "new"),
                    ],
                }],
            }],
            files_of(input)
        );
    }

    #[test]
    fn test_rename_paths() {
        let input = b"\
diff --git a/old b/new
similarity index 95%
rename from old
rename to new
index abcdef0..1234567 100644
--- a/old
+++ b/new
@@ -1 +1 @@
-a
+b
";
        let files = files_of(input);
        assert_eq!(1, files.len());
        assert_eq!(Some(String::from("old")), files[0].old);
        assert_eq!(Some(String::from("new")), files[0].new);
        assert_eq!(1, files[0].hunks.len());
    }

    #[test]
    fn test_binary_file() {
        let input = b"\
diff --git a/foo.bin b/foo.bin
index 1111111..2222222 100644
Binary files a/foo.bin and b/foo.bin differ
diff --git a/bar b/bar
--- a/bar
+++ b/bar
@@ -1 +1 @@
-a
+b
";
        let files = files_of(input);
        assert_eq!(2, files.len());
        assert_eq!(true, files[0].binary);
        assert_eq!(true, files[0].hunks.is_empty());
        assert_eq!(Some(String::from("foo.bin")), files[0].old);
        assert_eq!(false, files[1].binary);
        assert_eq!(Some(String::from("bar")), files[1].new);
    }

    #[test]
    fn test_classic_unified_pair() {
        let input = b"\
--- a/foo
+++ b/foo
@@ -1 +1 @@
-a
+b
";
        assert_eq!(
            vec![FileDiff {
                old: Some(String::from("foo")),
                new: Some(String::from("foo")),
                binary: false,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_len: 1,
                    new_start: 1,
                    new_len: 1,
                    section: String::new(),
                    lines: vec![
                        line(HunkLineKind::Delete, "a"),
                        line(HunkLineKind::Insert, "b"),
                    ],
                }],
            }],
            files_of(input)
        );
    }

    #[test]
    fn test_deleted_file() {
        let input = b"\
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        let files = files_of(input);
        assert_eq!(1, files.len());
        assert_eq!(Some(String::from("gone.rs")), files[0].old);
        assert_eq!(None, files[0].new);
        assert_eq!(
            vec![line(HunkLineKind::Delete, "bye")],
            files[0].hunks[0].lines
        );
    }

    #[test]
    fn test_preamble_and_exit_status_are_not_files() {
        let input = b"\
exit status: 1
diff --git a/foo b/foo
--- a/foo
+++ b/foo
@@ -1 +1 @@
-a
+b
exit status: 3
";
        let output = parse_diff(input);
        assert_eq!(1, output.files().len());
        assert_eq!(Some(String::from("foo")), output.files()[0].new);
        let view = output.view();
        let text: Vec<String> = view.lines.iter().map(|l| l.plain_text()).collect();
        assert_eq!("exit status: 1", text[0]);
        assert_eq!("foo", text[1]);
        assert_eq!(true, text.iter().any(|l| l == "exit status: 3"));
        assert_eq!(None, file_header_index(&view.headers, 0));
        assert_eq!(Some(0), file_header_index(&view.headers, 1));
    }

    #[test]
    fn test_non_diff_has_no_files() {
        let output = parse_diff(b"hello\nworld\n");
        assert_eq!(true, output.files().is_empty());
        let view = output.view();
        assert_eq!(2, view.lines.len());
        assert_eq!("hello", view.lines[0].plain_text());
        assert_eq!("world", view.lines[1].plain_text());
        assert_eq!(true, view.headers.is_empty());
    }

    #[test]
    fn test_hunk_body_does_not_start_a_file() {
        let input = b"\
diff --git a/foo b/foo
--- a/foo
+++ b/foo
@@ -1,3 +1,3 @@
-diff --git a/x b/x
+diff --git a/y b/y
 context diff --git a/z b/z
";
        let files = files_of(input);
        assert_eq!(1, files.len());
        assert_eq!(
            vec![
                line(HunkLineKind::Delete, "diff --git a/x b/x"),
                line(HunkLineKind::Insert, "diff --git a/y b/y"),
                line(HunkLineKind::Context, "context diff --git a/z b/z"),
            ],
            files[0].hunks[0].lines
        );
    }

    #[test]
    fn test_hunk_counts_keep_minus_marker_lookalike() {
        // A removed line whose text is `-- a/bar` is written as `--- a/bar`.
        // The hunk count must keep it inside the hunk, not start a new file.
        let input = b"\
diff --git a/foo b/foo
--- a/foo
+++ b/foo
@@ -2,2 +1 @@
--- a/bar
-x
+y
--- a/next
+++ b/next
@@ -1 +1 @@
-p
+q
";
        let files = files_of(input);
        assert_eq!(2, files.len());
        assert_eq!(
            vec![
                line(HunkLineKind::Delete, "-- a/bar"),
                line(HunkLineKind::Delete, "x"),
                line(HunkLineKind::Insert, "y"),
            ],
            files[0].hunks[0].lines
        );
        assert_eq!(Some(String::from("next")), files[1].new);
    }

    #[test]
    fn test_context_and_section() {
        let input = b"\
diff --git a/foo b/foo
--- a/foo
+++ b/foo
@@ -10,4 +10,4 @@ fn foo()
 keep
-old
+new
 keep2
";
        let files = files_of(input);
        assert_eq!("fn foo()", files[0].hunks[0].section);
        assert_eq!(10, files[0].hunks[0].old_start);
        assert_eq!(
            vec![
                line(HunkLineKind::Context, "keep"),
                line(HunkLineKind::Delete, "old"),
                line(HunkLineKind::Insert, "new"),
                line(HunkLineKind::Context, "keep2"),
            ],
            files[0].hunks[0].lines
        );
    }

    #[test]
    fn test_no_newline_marker() {
        let input = b"\
diff --git a/foo b/foo
--- a/foo
+++ b/foo
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        let files = files_of(input);
        assert_eq!(
            vec![
                line(HunkLineKind::Delete, "old"),
                line(HunkLineKind::Note, "\\ No newline at end of file"),
                line(HunkLineKind::Insert, "new"),
                line(HunkLineKind::Note, "\\ No newline at end of file"),
            ],
            files[0].hunks[0].lines
        );
    }

    #[test]
    fn test_view_header_is_path_and_hunk_is_prefixed() {
        let input = b"\
diff --git a/src/foo.rs b/src/foo.rs
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,2 +1,2 @@
-old
+new
";
        let view = parse_diff(input).view();
        let text: Vec<String> = view.lines.iter().map(|l| l.plain_text()).collect();
        assert_eq!(
            vec![
                String::from("src/foo.rs"),
                String::from("@@ -1,2 +1,2 @@"),
                String::from("-old"),
                String::from("+new"),
            ],
            text
        );
        assert_eq!(
            vec![crate::diff::FileHeaderSpan { start: 0, end: 1 }],
            view.headers
        );
    }

    #[test]
    fn test_view_new_and_rename_headers() {
        let input = b"\
diff --git a/foo b/foo
new file mode 100644
--- /dev/null
+++ b/foo
@@ -0,0 +1 @@
+x
diff --git a/old b/new
similarity index 100%
rename from old
rename to new
";
        let view = parse_diff(input).view();
        assert_eq!("foo (new)", view.lines[0].plain_text());
        assert_eq!("old -> new", view.lines[3].plain_text());
    }

    #[test]
    fn test_header_index_selects_section() {
        let headers = vec![
            crate::diff::FileHeaderSpan { start: 2, end: 3 },
            crate::diff::FileHeaderSpan { start: 10, end: 11 },
        ];
        assert_eq!(None, file_header_index(&headers, 0));
        assert_eq!(None, file_header_index(&headers, 1));
        assert_eq!(Some(0), file_header_index(&headers, 2));
        assert_eq!(Some(0), file_header_index(&headers, 9));
        assert_eq!(Some(1), file_header_index(&headers, 10));
        assert_eq!(Some(1), file_header_index(&headers, 40));
    }

    #[test]
    fn test_file_list_lines_marks_selection_and_counts() {
        let input = b"\
diff --git a/foo b/foo
--- a/foo
+++ b/foo
@@ -1 +1,2 @@
-old
+new
+extra
diff --git a/bar b/bar
--- a/bar
+++ b/bar
@@ -1 +1 @@
-a
+b
";
        let output = parse_diff(input);
        let files = output.files();
        let lines = crate::diff::file_list_lines(
            &files,
            &[],
            1,
            crate::terminal_bg::TerminalBg::Dark,
            crate::terminal::width::Width(40),
            &[],
        );
        assert_eq!(2, lines.len());
        // git --stat: paths padded so `|` aligns, then the change count
        // and a +/− bar. foo is +2−1 (total 3); bar is +1−1 (total 2).
        let a = lines[0].plain_text();
        let b = lines[1].plain_text();
        assert_eq!(true, a.starts_with("  foo"));
        assert_eq!(true, b.starts_with("> bar"));
        let pipe_a = a.find(" | ").unwrap();
        let pipe_b = b.find(" | ").unwrap();
        assert_eq!(pipe_a, pipe_b);
        assert_eq!(true, a[pipe_a..].starts_with(" | 3 "));
        assert_eq!(true, b[pipe_b..].starts_with(" | 2 "));
        assert_eq!(
            true,
            lines[1]
                .spans
                .iter()
                .all(|s| s.style.bg == Some(crate::terminal::color::TerminalColor::Ansi256(8)))
        );
        assert_eq!(true, lines[0].spans.iter().all(|s| s.style.bg.is_none()));
        assert_eq!(false, lines[1].spans.iter().any(|s| s.style.reverse));
        let ins = lines[0]
            .spans
            .iter()
            .find(|s| {
                let t = s.content.as_ref().as_str();
                !t.is_empty() && t.chars().all(|c| c == '+')
            })
            .unwrap();
        let del = lines[0]
            .spans
            .iter()
            .find(|s| {
                let t = s.content.as_ref().as_str();
                !t.is_empty() && t.chars().all(|c| c == '-')
            })
            .unwrap();
        // max change is 3 and the bar is wide, so marks are unscaled:
        // two inserts and one delete.
        assert_eq!("++", ins.content.as_ref().as_str());
        assert_eq!("-", del.content.as_ref().as_str());
        assert_eq!(
            Some(crate::terminal::color::TerminalColor::Green),
            ins.style.fg
        );
        assert_eq!(
            Some(crate::terminal::color::TerminalColor::Red),
            del.style.fg
        );
    }

    #[test]
    fn test_file_list_stat_aligns_unequal_names() {
        let input = b"\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1 +1 @@
-a
+b
diff --git a/much/longer/path.rs b/much/longer/path.rs
--- a/much/longer/path.rs
+++ b/much/longer/path.rs
@@ -1,4 +1,4 @@
-a
-b
-c
-d
+w
+x
+y
+z
";
        let output = parse_diff(input);
        let files = output.files();
        let lines = crate::diff::file_list_lines(
            &files,
            &[],
            0,
            crate::terminal_bg::TerminalBg::Dark,
            crate::terminal::width::Width(60),
            &[],
        );
        let a = lines[0].plain_text();
        let b = lines[1].plain_text();
        assert_eq!(a.find(" | "), b.find(" | "));
        // 4 deletes + 4 inserts on the long path; 1 and 1 on x.
        assert_eq!(true, b.contains(" | 8 "));
        assert_eq!(true, a.contains(" | 2 "));
    }

    #[test]
    fn test_file_list_lines_appends_untracked() {
        let input = b"\
diff --git a/foo b/foo
--- a/foo
+++ b/foo
@@ -1 +1 @@
-a
+b
";
        let output = parse_diff(input);
        let files = output.files();
        let untracked = vec![String::from("notes.txt"), String::from("tmp/scratch")];
        let lines = crate::diff::file_list_lines(
            &files,
            &untracked,
            2,
            crate::terminal_bg::TerminalBg::Dark,
            crate::terminal::width::Width(48),
            &[],
        );
        assert_eq!(3, lines.len());
        let stat = lines[0].plain_text();
        let first_u = lines[1].plain_text();
        let second_u = lines[2].plain_text();
        assert_eq!(true, stat.starts_with("  foo"));
        assert_eq!(true, stat.contains(" | "));
        assert_eq!(true, first_u.starts_with("  notes.txt"));
        assert_eq!(true, first_u.contains(" | untracked"));
        assert_eq!(true, second_u.starts_with("> tmp/scratch"));
        assert_eq!(true, second_u.contains(" | untracked"));
        // Pipe column matches the stat row.
        assert_eq!(stat.find(" | "), first_u.find(" | "));
        assert_eq!(stat.find(" | "), second_u.find(" | "));
        assert_eq!(
            true,
            lines[2]
                .spans
                .iter()
                .all(|s| s.style.bg == Some(crate::terminal::color::TerminalColor::Ansi256(8)))
        );
        assert_eq!(true, lines[1].spans.iter().all(|s| s.style.bg.is_none()));
    }

    #[test]
    fn test_file_list_lines_are_stable() {
        // The stat page skips a repaint when a refresh rebuilds equal
        // lines. That equality is what keeps a text selection alive.
        let input = b"\
diff --git a/foo b/foo
--- a/foo
+++ b/foo
@@ -1 +1,2 @@
-old
+new
+extra
";
        let output = parse_diff(input);
        let files = output.files();
        let untracked = vec![String::from("notes.txt")];
        let a = crate::diff::file_list_lines(
            &files,
            &untracked,
            1,
            crate::terminal_bg::TerminalBg::Dark,
            crate::terminal::width::Width(40),
            &[],
        );
        let b = crate::diff::file_list_lines(
            &files,
            &untracked,
            1,
            crate::terminal_bg::TerminalBg::Dark,
            crate::terminal::width::Width(40),
            &[],
        );
        assert_eq!(a, b);
    }

    #[test]
    fn test_file_list_lines_untracked_only() {
        let lines = crate::diff::file_list_lines(
            &[],
            &[String::from("a.txt"), String::from("dir/b.txt")],
            0,
            crate::terminal_bg::TerminalBg::Dark,
            crate::terminal::width::Width(40),
            &[],
        );
        assert_eq!(2, lines.len());
        assert_eq!(true, lines[0].plain_text().starts_with("> a.txt"));
        assert_eq!(true, lines[0].plain_text().contains(" | untracked"));
        assert_eq!(true, lines[1].plain_text().starts_with("  dir/b.txt"));
        assert_eq!(true, lines[1].plain_text().contains(" | untracked"));
        assert_eq!(
            lines[0].plain_text().find(" | "),
            lines[1].plain_text().find(" | ")
        );
        let empty = crate::diff::file_list_lines(
            &[],
            &[],
            0,
            crate::terminal_bg::TerminalBg::Dark,
            crate::terminal::width::Width(40),
            &[],
        );
        assert_eq!(1, empty.len());
        assert_eq!("(no files)", empty[0].plain_text());
    }

    fn owned_lines(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| String::from(*line)).collect()
    }

    #[test]
    fn test_commit_description_from_git_show_preamble() {
        let preamble = owned_lines(&[
            "commit 03446b88a53d3a3aa3ddf57bbc69db3e7c442967",
            "Author: T <t@e>",
            "Date:   Wed Jan 1 00:00:00 2020 +0000",
            "",
            "    subject line",
            "    ",
            "    body paragraph",
            "    second body",
            "",
        ]);
        assert_eq!(
            owned_lines(&["subject line", "", "body paragraph", "second body"]),
            crate::diff::commit_description_lines(&preamble)
        );
    }

    #[test]
    fn test_commit_description_from_sl_show_preamble() {
        let preamble = owned_lines(&[
            "commit:      c4f82db4495f",
            "user:        T <t@e>",
            "date:        Tue Nov 17 20:17:16 2026 +0000",
            "files:       a",
            "description:",
            "subject line",
            "",
            "body paragraph",
            "second body",
            "",
            "",
        ]);
        assert_eq!(
            owned_lines(&["subject line", "", "body paragraph", "second body"]),
            crate::diff::commit_description_lines(&preamble)
        );
    }

    #[test]
    fn test_file_list_lines_prepends_commit_description() {
        let input = b"\
diff --git a/foo b/foo
--- a/foo
+++ b/foo
@@ -1 +1 @@
-a
+b
";
        let output = parse_diff(input);
        let files = output.files();
        let desc = vec![String::from("subject line"), String::from("body paragraph")];
        let lines = crate::diff::file_list_lines(
            &files,
            &[],
            0,
            crate::terminal_bg::TerminalBg::Dark,
            crate::terminal::width::Width(40),
            &desc,
        );
        assert_eq!(
            crate::diff::file_list_description_rows(&desc) + 1,
            lines.len()
        );
        assert_eq!("subject line", lines[0].plain_text());
        assert_eq!("body paragraph", lines[1].plain_text());
        assert_eq!("", lines[2].plain_text());
        assert_eq!(true, lines[3].plain_text().starts_with("> foo"));
    }

    #[test]
    fn test_empty_input() {
        let output = parse_diff(b"");
        assert_eq!(true, output.files().is_empty());
        let view = output.view();
        assert_eq!(1, view.lines.len());
        assert_eq!("(no changes)", view.lines[0].plain_text());
        assert_eq!(true, view.headers.is_empty());
    }
}
