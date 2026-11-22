# sl-tui

A terminal UI for [Sapling](https://sapling-scm.com/). With no
subcommand it opens the smartlog. `diff` (alias `di`) opens a live
working-copy diff or a pull request. `show` opens one commit.

While an agent is editing the tree, the diff is re-run on an interval.
The smartlog is re-queried on that interval while it is open.
When the diff output changes, the scroll position is kept and the
current file header stays pinned.

GitHub and Origin pull-request review and CI status are not fetched
before the smartlog is drawn. They load in the background and the
tree is repainted when they arrive. While the smartlog stays open
that status is refreshed on its own interval.

Sapling is used when the repository already has sl metadata (`.sl` or
`.git/sl`). Otherwise the repository is treated as git.

## Features

- Smartlog (`t`), drawn from `sl smartlog` and refreshed while open,
  with GitHub or Origin PR links; review and CI status load in the
  background
- Live working-copy diff that keeps the scroll position
- Commit view (`-c`, or Enter in the smartlog) and diff against the
  previous commit
- Git and Sapling
- Origin and GitHub pull request diffs
- Pinned file header
- Untracked files on the status bar and in the diff stat
- Diff stat (`s`), including the commit message in `-c` mode
- Wrapped lines stay one logical line, so a double-click selects the whole word
- `less`-like navigation, including search and marks

## Screenshots

Diff view, scrolled partway down. The first file's header stays
pinned and the top of its diff is off-screen; the pane shows the tail
of that diff and the start of the next file. `/bar` highlights the
matches. The status bar shows the untracked count and the scroll
position.

![Diff view](screenshots/diff.png)

Diff stat for the same tree.

![Diff stat](screenshots/diffstat.png)

Smartlog (`t`). `@` is the working copy; a side draft joins the stack.

![Smartlog](screenshots/smartlog.png)

## Installation

```
cargo install --git https://github.com/stepancheg/sl-tui
```

or `bin/sl-tui`

## Usage

```
sl-tui              smartlog (Sapling); diff if the repo is git-only
sl-tui sl           smartlog (Sapling); same as no subcommand
sl-tui diff         working-copy diff
sl-tui di           same as diff
sl-tui show [REV]   commit message and diff (default `.`)
```

## Options

Global flags apply to every command.

| Flag | Meaning |
| --- | --- |
| `-n`, `--interval` | Seconds between runs (default `0.5`, or `LIVE_DIFF_INTERVAL`) |
| `--sl` | Force Sapling |
| `--git` | Force git |

`show` takes an optional revision (`show REV` or `show -r REV`),
`--stat`, and `-U`/`--unified`. With no revision it shows `.`.

`diff` / `di` flags:

| Flag | Meaning |
| --- | --- |
| `-r`, `--rev` | Revision to compare against. Without this, the tool's default diff |
| `-c`, `--change` | Commit message and diff for one commit. `-c .` live-refreshes; a digest does not |
| `-U`, `--unified` | Initial context lines (default 3) |
| `--stat` | Open the diff stat view |
| `--origin-pr N` | Origin PR diff (`origin pr diff N`). No live refresh |
| `--github-pr N` | GitHub PR diff (`gh pr diff N`). No live refresh |

## Keys

| Key | Action |
| --- | --- |
| `h` | Help |
| `q` | Quit |
| `r` | Run the diff now |
| `j` / `k`, arrows | Scroll |
| `^e` / `^y` | Scroll down / up; in a list the selection stays |
| `Space` / `PgDn`, `PgUp` | Page |
| `g` / `G` | Top / bottom |
| `/` `?` `n` `N` | Search |
| `s` | Diff stat (Enter or `s` opens the file, Esc returns) |
| `d` | Diff mode: `-r .`, `-c .`, or `-r prev` |
| `t` | Smartlog (Enter opens that commit, or `diff -r prev` on `@`; `c`/`v` open `-c`/`-r` for that commit; Esc returns) |
| `u` / `U` | Decrease / increase context |
| `mx` / `'x` | Set / go to mark |

Untracked files are counted on the status bar and listed after the stat.
