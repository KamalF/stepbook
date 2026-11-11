# stepbook progress

Where the build stands against the build order in `docs/SPEC.md`, section 16.

## How we work

Claude works on its own. It writes the code, the tests and the commit messages, and commits. Kamal makes the design and product choices, and approves each new dependency. Claude makes the obvious choices itself and records them in the spec or here.

- One branch per milestone, cut from `main` after the merge of the one before.
- Small commits, with conventional prefixes: `feat:`, `fix:`, `docs:`, `refactor:`, `tests:`, `chore:`.
- Each commit passes `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and the tests. The spec and this file change in the same commit as the code that they describe.
- A milestone ends with `cargo mutants`, then the review loop, until a round finds nothing to fix, then `cargo mutants` again if the code changed. `/tmp` is a small tmpfs, so mutants runs with `TMPDIR=~/.cache/stepbook-mutants mise exec -- cargo mutants -j 6`, and each reviewer works in its own directory under `~/.cache/stepbook-review/`. Claude checks the TUI in tmux. Kamal's manual checklist keeps only what tmux cannot show: copy and paste in herdr and Ghostty, and the look in his terminal. Then Claude merges into `main`, and Kamal pushes.
- `docs/runbook.md`, excluded from git, logs each command that changes the setup, such as `cargo add` or `mise use`.

## Milestones

| # | Milestone | Status |
|---|---|---|
| 1 | Document view | Done, merged into `main` on 2026-11-10 |
| 2 | Running blocks | In progress, on `m2-running-blocks` |
| 3 | Input and full-screen view | — |
| 4 | Log and reload | — |
| 5 | Migration | — |

Kamal signed off the build order and the new dependencies on 2026-11-09.

## Milestone 1: document view

The plan, one commit per line:

1. `chore`: the crate, `mise.toml` with the `fmt`, `lint` and `test` tasks, `.gitignore` and the MIT licence.
2. `chore`: CI on GitHub Actions.
3. `docs`: the Tooling and Testing rules sections of the spec.
4. `feat`: the arguments and the startup errors (4).
5. `feat`: the terminal setup and restore, with the panic hook. `q` and `ctrl-c` quit.
6. `feat`: the blocks, their attributes and the frontmatter (5.1, 5.2).
7. `feat`: paragraphs, headings and inline styles, with word wrap (5.3). The document view fills the frame, so that each later commit shows its elements.
8. `feat`: lists, task lists and blockquotes, with the cap at 10 levels (5.3).
9. `feat`: tables (5.3).
10. `feat`: code blocks and blocks, with bash highlighting (5.3).
11. `feat`: thematic breaks, raw HTML, images and the frontmatter (5.3).
12. `feat`: control characters (6.5).
13. `feat`: the layout: sidebar, document view and footer (6.1).
14. `feat`: the keys that move and quit (7.1).
15. `tests`: each runbook in `~/dev/claude/runbooks/` opens with no panic.

The 15 commits are done. Kamal decided two extensions of 6.5 on 2026-11-09: bidi controls are escaped everywhere, and invisible characters in code.

The test of commit 15 is ignored, because CI has no runbooks: `cargo test --release -- --ignored` runs it on `~/dev/claude/runbooks/`, or on `$STEPBOOK_RUNBOOKS`. It opens each runbook at 40 by 8, the smallest screen, at 79 by 24 and at 120 by 40. On 2026-11-09, the 62 runbooks opened in 2.9 s.

### After the 15 commits

- `cargo mutants`: the first run caught 344 of 382 mutants, with 2 timeouts. The misses got tests, or an exclusion with its reason in `.cargo/mutants.toml`. The second run missed 4, which got the same treatment.
- `/simplify`, with 4 cleanup agents: 8 commits. It found a panic (a table cell of emoji with a variation selector) and fixed the measure of text: ratatui measures graphemes, not characters.
- Review loop, with a code reviewer and a security reviewer each round:
  - Round 1: 2 blockers and 7 should-fix, all fixed, among them code hidden by graphemes across spans, prefixes wider than the view, and the quadratic bash highlighting.
  - Round 2: 2 should-fix, fixed: a CR before an LF in plain code, and a combining mark after a token.
  - Round 3: 1 blocker and 3 should-fix, fixed: very wide graphemes in code, the warning for escaped graphemes, padded tables, and the tests of the code escapes.
  - Round 4: 1 blocker and 5 should-fix, fixed: CR line ends past the table bound, the warning after a tab, deep quotes that make the parse quadratic, the highlight budget, and the squares of long lines. Kamal chose to escape spaces other than U+0020 in code, and a minimum terminal size of 40 by 8.
  - Round 5: 1 blocker and 2 should-fix, fixed: a code block is now always followed by a blank line, the nesting limit counts indent, and code escapes the format characters that join the next one.
  - Round 6: the code reviewer found no defect. The security reviewer found 1 blocker and 2 should-fix, fixed: code escapes each grapheme of several code points after its first character, autowrap is off while stepbook runs, and warnings no longer quote free text of the info string.
  - Round 7: 2 should-fix, fixed: Page Down no longer selects an earlier block, and comments in code show in italic, not dimmed, since the highlighting can take running code for a comment.
  - Round 8: 1 blocker and 2 should-fix, fixed: each frame repaints the whole screen, since a row that a terminal drew shifted stayed on later frames; code escapes a first grapheme that would join the gutter's space; and a test checks the nesting limit at its boundary.
  - After round 8, `cargo mutants` ran again over all the fixes: 513 caught, 13 missed, which got tests or exclusions.
  - Round 9: the security reviewer found no defect. The code reviewer found 2 should-fix, fixed: the restore ends the synchronized update of a failed frame, and code escapes the regional indicators.
  - Round 10: the security reviewer found nothing. The code reviewer found 2 should-fix, fixed: a row that goes on from a wrapped line of code starts with a dim `↪ ` (Kamal's choice), and j shows the whole block when it fits.
  - Round 11: 2 should-fix, fixed: the `↪` mark moves into the gutter, where code text cannot fake it (Kamal's choice), and a property test checks it.
  - Round 12: 1 blocker and 3 should-fix, fixed: in code, a tab shows a dim `⇥`, a space that ends a row a dim `␣` (Kamal's choice, like vim's listchars), and wrapped rows of the info line get the `↪` gutter. A nit fix: an empty log path is an error. Then `cargo mutants` ran again: 535 caught, 1 timeout, and 1 equivalent mutant, now excluded.
  - Round 13: the code reviewer found only nits. The security reviewer found 1 should-fix, fixed: property tests now check the `␣` and the `↪` of info rows, which only snapshots checked. Nit fixes: the nesting bound reads `1-` as text, and `main::action` ignores keys on a screen that is too small, under a test.
  - Round 14: 2 should-fix, fixed: the gutter is `▐`, not `▌` (Kamal's choice), and the space after the `│` of a quote has no style, so that a terminal that draws glyphs of ambiguous width wide cannot shift code; code escapes `␣` and `⇥`, which passed for the marks. A nit fix: the code of a block always ends with a line feed.
  - Round 15: the security reviewer found nothing. The code reviewer found 1 should-fix: characters of ambiguous width in code text shift code too. Kamal chose to require a terminal that draws them 1 column wide (spec 11), and the space after a quote's `│` gets its style back. A snapshot now shows the escapes of `␣` and `⇥`.
  - Round 16: the security reviewer found nothing. The code reviewer found 1 should-fix, fixed: code escapes U+1D159, which fonts draw as a blank, as it does U+2800.
  - Round 17: the security reviewer found no defect. The code reviewer found 1 should-fix, fixed: a property test now checks that comments never show dim, which only snapshots checked. Two new snapshots cover the wrap of HTML lines and the escapes of blank glyphs.
  - Round 18: the code reviewer found only nits in the docs, fixed. The security reviewer found 1 should-fix, fixed: U+0000 becomes U+FFFD, as CommonMark says, since bash drops it: `r^@m` would have run as `rm`.
  - Round 19: nothing to fix. The security reviewer found nothing, and the code reviewer 2 nits in comments and docs, fixed. The loop converged.
- `cargo mutants` after the loop: 541 caught, 0 missed, 18 unviable and 1 timeout (`+=` to `*=` in `fitted_prefixes` loops forever, so the tests catch it).
- The check in tmux on 2026-11-10: the largest runbook, the `▐` gutter, `↪` rows, `⇥` and `␣`, the moves, the 39 by 8 message, and `q`, which restores the terminal with exit code 0.

### Kamal's manual checklist

What tmux cannot show, in herdr in Ghostty:

1. `stepbook ~/dev/claude/runbooks/<a runbook>.md`: the headings, the code highlighting, the gutters, the dimmed text and the cyan selection read well in your theme.
2. Select text with the mouse: herdr copies it, because stepbook does not capture the mouse.
3. `j`, `k`, `PageDown`, `End`, `Home`, then `q`: the shell prompt comes back clean.
4. Ambiguous width (spec 11): `printf '→→→→|\n····|\n|||||\n'` shows the three `|` of the right edge in one column, so your terminal draws these characters 1 column wide. `~/dev/claude/runbooks/2026-11-10-stepbook-ambiguous-width.md` shows the same check in stepbook.

Kamal ran the checklist on 2026-11-10: the four checks pass.

### Review notes for the next milestones

Findings that the review loop of milestone 1 rejected or deferred, so that later rounds do not raise them again without a new argument:

- Rejected: tab stops in prose count from the start of the line (spec 6.5). Prose can draw `▐` and the style of the selection: a Markdown viewer shows what the author writes. A table too wide for the view keeps its clipped last column. Code does not escape look-alikes of the marks, such as `⌴`, only `␣` and `⇥`. Code does not escape characters of ambiguous width: spec 11 requires a terminal that draws them narrow.
- Deferred nits, display only: a loose list whose first item has no paragraph gets no blank line after item 1; in prose, a combining mark after a styled span is dropped; in code, a tab after an escaped grapheme, or after a grapheme that syntect splits, stops short of the next multiple of 8; a tab whose padding ends a row of code shows `␣` in its last column.
- Deferred nits, other: `read_runbook` checks the path, then opens it, so a link swapped to a FIFO in between blocks the open, before raw mode; the palette lives in both `doc.rs` and `view.rs`.

## Milestone 2: running blocks

The plan, one commit per line:

1. `feat`: bracketed paste is on, and a paste does nothing in the document view: a line feed in it would run the selected block (7.1). Milestone 3 sends a paste to the program (7.2).
2. `feat`: if `j` or `k` would scroll to show the selected block, `Enter` only does that scroll. So `Enter` never runs code that is off the screen, such as after the startup or a resize (7.1).
3. `refactor`: the event loop reads two channels, and handles control events (keys, paste, resize, the tick, the end of a run) before output. A thread reads the terminal events (13). Adds `crossbeam-channel`.
4. `refactor`: the view caches the layout of the document per width, and gives the selected block its style when it draws. Output then redraws without a new layout of the whole file.
5. `feat`: the workspace and its modes. stepbook deletes it on exit, after a panic, and on SIGTERM or SIGHUP (8.3). Adds `signal-hook`.
6. `feat`: the session state: the start state, the state file split on NUL bytes, the diff and the merge (9).
7. `feat`: a run in its own PTY: the script file, the wrapper, the environment, the check of the session directory, a reader thread per run, and the exit code (8.2, 9.2). The integration tests of 14.3 come with it, except the log tests. Adds `rustix`.
8. `feat`: the terminal of a run: a vt100 screen with 5,000 lines of scrollback, and its last 15 rows as lines, with the escapes of 6.5 and the UTF-8 characters split across two reads (6.3, `term.rs`). Adds `vt100`.
9. `feat`: `Enter` runs the selected block. The output shows under its code, the PTY follows the size of the view, and the view follows the output of the selected block (6.3, 8.1).
10. `feat`: the block states in the sidebar and on the info line, with the run time (6.2).
11. `feat`: the re-run dialog, the messages of `Enter`, and the footer notifications (6.1, 7.4, 8.1).
12. `feat`: quit with active runs: the dialog, SIGHUP, then SIGKILL after 2 seconds (11).

Done when (spec 16): the integration tests of 14.3 pass, except the log tests, and in tmux a block that runs `export` and `cd` changes what the next block sees.

The 12 commits are in, with three more: a fix, since a hangup of the terminal could abort stepbook with a core dump; a chore that ignores the pending snapshots of insta; and a fix of the wrapper, since `set -x` in a block printed the value of each exported variable at its exit. The check in tmux of commit 9: a block that runs `export` and `cd` changes what the next block sees.

Kamal signed off the plan and these choices on 2026-11-10. The spec records them (19).

- `rustix`, not `portable-pty`, opens the PTY (about 80 lines of our own). portable-pty reports a death by signal only as the text of `strsignal`, such as `Interrupt`, so `rc=130 SIGINT` would parse that text. Its kill reaches only the shell, and spec 11 sends SIGKILL to every process of the run. It also brings about 10 crates. rustix is already in the tree through crossterm.
- A run that cannot start fails: its block shows `✘` and the reason, and `Enter` can run it again. Spec 6.2 listed "cannot start" under "Cannot run", a state that only a reload ends. "Cannot run" keeps the problems of the file, such as a duplicate name.
- The re-run and quit questions show in the footer, in place of the keys, so that they hide no part of the document.
- Program output keeps its colors, 256 colors and RGB included, as in a terminal. The look and feel review can change that.
- stepbook blinks the `●` of a running block itself, every 0.5 s: terminals differ on the blink attribute.

### After the 12 commits

- `cargo mutants`: 654 caught, 18 missed, 72 timeouts and 47 unviable, in 33 minutes. The tests that wait for a run had no time limit, so a mutant that lost an event hung them: they now fail after 10 s. The misses got tests (the file descriptors of a run, a key release with a question open, the end of a row of output, the layout cache, the top of a block under output), or an exclusion with its reason in `.cargo/mutants.toml`.
- `/simplify`, with 4 cleanup agents: 2 commits. The altitude agent found 2 bugs of the wrapper, fixed: a handler that chained `trap -p EXIT` called the wrapper's handler without end, and under `set -a` the state took `BASH_TRAPSIG` on most exit paths. The efficiency agent measured 7 MB of output 12 times faster once the rows of output are made once per frame, not once per read of the PTY.

- Review loop, with a code reviewer and a security reviewer each round. Accepted findings per round: 6, 4, 5, 4, 1, 5, 3, 4, 2, 2, 2, 3, 2, 1, 0, all fixed, in the commits from 85eb695 to 49e7b43. Round 15 found no blocker and no should-fix: the loop converged on 2026-11-11. Among the findings:
  - Round 1: crafted output made vt100 panic, which ended stepbook (caught now); no frame during a flood of output; keys typed ahead; POSIX mode broke the wrapper; `set -x` showed the wrapper.
  - Round 2: a run ended only when the PTY closed (now when the wrapper exits); `timeout` escaped the signals of a quit (now the whole session); REP stalled the main thread; a relative `TMPDIR`.
  - Round 3: vt100 moved to the reader thread of each run, so that crafted output slows only its run; jobs that `timeout` or `set -m` moved got no SIGHUP; the logical `PWD`; exported arrays.
  - Round 4: vt100 without its `std` feature (an OSC that never ends grew the memory); the runs end before the workspace goes; the wrapper writes only into its own state file.
  - Round 5: `/proc/<pid>/stat` read as bytes.
  - Round 6: the `tee` of the old logging header lost the end of its output to the kernel's SIGHUP (the wrapper now closes its files and waits up to 1 s); variables of 4 KiB or more go in a file; a bash without `compgen` saves no state.
  - Round 7: the end of the wrapper runs free of the script's options and traps.
  - Round 8: a bare `exit` in the script's handler lost the status of the script; a handler that turned `set -x` on traced each variable; under `set -T`, a `DEBUG` trap made the wrapper call itself without end; the locals of a function reached the session; SIGINT left the workspace behind. With Kamal's answers to two open points: a background job outlives its block, until stepbook ends, and the session directory stays a path.
  - Round 9: a job whose parent had ended got the kernel's SIGHUP (the wrapper now always hands the PTY off); under `localvar_unset`, the save dropped variables; a bare `exit` in a command substitution of the handler.
  - Round 10: the hand-off fired an `ERR` trap of the script, so every block ended with rc=1 under `set -e; trap ... ERR`; a job whose main thread had ended counted as a zombie.
  - Round 11: a subshell lost an EXIT trap equal to the script's handler; stepbook trusted the workspace by its path for the whole session (now locked against systemd-tmpfiles, checked before each run, and its parent checked at startup).
  - Round 12: the drain's timer could send the state into a workspace that another user made again; stepbook read the state file by its path, where a FIFO blocked it; a workspace left by SIGKILL failed each later start with the same pid. Kamal chose the counter suffix `stepbook-<pid>-<n>`.
  - Round 13: the check skipped the directories above the workspace; in a user namespace, it refused `/tmp`, owned there by the overflow uid; an `XDG_RUNTIME_DIR` that named a file. Kamal kept the counter names, though another user can squat them (denial of service only).
  - Round 14: the `GPG_TTY` of a run named the terminal of stepbook, where pinentry drew over the view and took its keys.

## Next

1. `cargo mutants` again, since the code changed after the first run, then the check in tmux and Kamal's checklist. Then the merge into `main`. The log of the review loop, with its Decisions, Follow-ups and Nits deferred, is `~/.cache/stepbook-review/m2-review-loop-log.md`.
2. The look and feel review, before milestone 3.

Kamal pushes later: the repository has no remote yet (see the first open point).

## Open points

- Before the first push, Kamal creates `KamalF/stepbook` on GitHub and adds the remote. The sandbox cannot write `.git/config`.
- Kamal runs `cargo publish`: it needs his crates.io token. The name `stepbook` was free on crates.io on 2026-11-09. mdame held its name early with a 0.0.1 release; stepbook can do the same after milestone 1.
- Look and feel review, between milestones 2 and 3, once blocks show their output and state (Kamal, 2026-11-10). From the first screenshot: prose lines of about 200 characters on a wide terminal (a maximum text width of about 100 columns?); the selection in terminal cyan, which Kamal's theme draws green; the contrast between a block that runs and other code. From the review of milestone 2: output rows have no mark of their own, so output can draw a fake info line and gutter in the column of a nested block. A question or a notice is cut at the right edge of the footer: at 45 columns, "1 block running. Stop it and quit?" loses its "(y/N)". During a flood of output, the rows under the block can stay old for a second or more.
- Follow-ups of the review loop of milestone 2, for milestone 3: a run's terminal answers no query (DSR, DA, OSC 10/11), so programs that ask wait for their timeout; sudo asks for the password in each block, since each run has its own PTY; vt100 keeps 5,000 lines of scrollback at full width, about 29 MB per block at 180 columns; in a block, `$0` and `BASH_SOURCE` name files of the workspace; gpg-agent's ssh support shows pinentry on its startup tty, which can be stepbook's terminal.
- Follow-ups of the review loop: bash highlighting takes 0.1 to 0.2 s before the first frame on the largest runbook (syntect's onig backend, or highlighting on first view, would be faster: a choice for Kamal). Terminals that apply the bidi algorithm on their own (VTE, Konsole) can still reorder code with right-to-left letters. Ghostty does not.
- Before milestone 4 writes the log: the default log of a FILE named `x.log` is the runbook itself. Spec 4 needs a rule.
- Claude drives stepbook in tmux from the sandbox with `scripts/tui.sh` and one steps file per check: it starts tmux, sends the keys and captures the pane. The sandbox stops the tmux server when a command ends. Checked on 2026-11-09.
