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
| 2 | Running blocks | Done, merged into `main` on 2026-11-11 |
| 3 | Input and full-screen view | Done, merged into `main` on 2026-11-11 |
| 4 | Log and reload | Done, merged into `main` on 2026-11-12 |
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
- `cargo mutants` after the loop, on the lines changed since ed90e54: 228 mutants, 0 missed in the end. Two mutants widened the processes that stepbook signals to each process of the user: they killed the run twice, inside the sandbox, and outside it they would end the desktop session. `.cargo/mutants.toml` excludes them. Of the 14 misses, 10 got tests (the 0.2 s between SIGHUP and SIGKILL, a job that writes without a pause, the size of a new terminal, `in_vt100`) and 3 are equivalent, excluded with their reason; `wait_for_jobs` lost a dead EINTR branch. 3 mutants time out: each makes the code hang, so the tests catch it. Two baseline tests failed under cargo mutants only, fixed: a `read_dir` order, and the fds that the test process inherits. Other sessions held most of the memory, with 4 GB free, so the run used `-j 2 --jobserver-tasks 4`: 13 minutes.
- The check in tmux on 2026-11-11: a block that runs `export` and `cd` changes what the next block sees; a failed block shows `✘ rc=3` and asks before a re-run; a running block shows `●` and its output as it comes; `q` with a running block asks, and `y` ends stepbook with exit code 0 and no workspace left.

### Kamal's manual checklist of milestone 2

What tmux cannot show, in herdr in Ghostty: open `~/dev/claude/runbooks/2026-11-11-stepbook-running-blocks.md` with `stepbook --no-log`, and follow its five sections:

1. Session state: `carry-over` prints what `setup` set.
2. Colors and states: the colors of `ls --color` read well in your theme, with `✔ rc=0` and `✘ rc=3` on the info lines.
3. A running block: the `●` blinks, the output grows, and Enter on it asks to re-run it.
4. Paste: a paste of `j` and `q` with a line feed moves nothing and quits nothing.
5. Quit with a running block: `q`, then `y`, gives a clean shell prompt.

Kamal ran the checklist on 2026-11-11: the five checks pass.

## Milestone 3: input and full-screen view

The plan, one commit per line:

1. `docs`: this plan, and the limits of a run that stepbook cannot change (8.2): sudo asks for the password in each block, gpg-agent can show the pinentry of an ssh key on the terminal of stepbook, and `$0` and `BASH_SOURCE` name files of the workspace.
2. `fix`: while the reader of a run holds its terminal, it makes the rows that the main thread asked for. During a flood of output, the rows under the block stayed old for a second or more, and the full-screen view would freeze (6.3, 13).
3. `feat`: input mode: `i` on the selected running block, the keys in the encoding of xterm, `ctrl-o` and the end of the run leave it, and the cursor of the program shows in its output (7.2). A writer thread per run writes to the PTY, so that a program that reads no input never blocks stepbook (13).
4. `feat`: in input mode, a paste goes to the program, between `ESC[200~` and `ESC[201~` if the program turned bracketed paste on (7.2).
5. `feat`: the terminal of a run answers the queries of its status and cursor position (DSR) and of its device attributes (DA1) (6.3).
6. `feat`: the full-screen view: `o`, the PTY at the size of the screen while the view is open, the keys to the program while the block runs, the scrollback after the run, and the footer (7.3).
7. `feat`: the switch to the alternate screen: the selected running block opens the full-screen view in input mode, and its return closes it; the info line shows `full-screen program, press o`; the rows under the block stay as they were before the switch (6.3). Round 6 of the review dropped the automatic open, at Kamal's choice: the view opens with `o` or `i`, or from input mode.

Done when (spec 16): a `read -s` prompt takes a password in input mode, and `o` shows `less` in the full-screen view, which closes when less exits. In herdr, a paste reaches the program (manual check).

Kamal's choices on 2026-11-11:

- The full-screen view keeps the footer, with the mode, the block and the keys that go back. The PTY gets the height of the screen minus 1, as under a block.
- The scrollback stays at 5,000 lines per run. It takes memory only when a block prints that much: about 29 MB at 180 columns.
- The terminal of a run answers DSR 5 and 6 and DA1. The answers hold only numbers, never text from the program. The color queries (OSC 10 and 11) stay unanswered: most libraries send DA1 after them, and stop waiting at its answer.
- The look and feel review comes after milestone 3, so that it covers input mode and the full-screen view too.

The 7 commits are in, with a fix that ends the writer thread of a run with the run.

### After the 7 commits

- `cargo mutants` on the diff of the 7 commits: 244 mutants, 226 caught, 3 missed, 3 timeouts and 12 unviable, in 17 minutes. The misses got tests: the cursor under row 15, and a switch of another block in input mode. The third showed a redundant guard in `switch`.
- `/simplify`, with 4 cleanup agents: 1 commit. The view alone keeps whether a program is on the alternate screen, `View::pty_size` covers the full-screen view, the keys after the run go through `action()`, and the loop over the scrollback moves the offset by a screen: 5,000 changes of the offset took 50 ms when `o` opened the view.
- A tests commit: the four checks of a session after SIGKILL wait up to 10 s, as another test does since 3434962. One failed under the load of cargo mutants.
- Review loop, with a code reviewer and a security reviewer each round. Accepted findings per round: 4, 2, 2, 2, 1, 3, 0, 0. Round 7 found no blocker and no should-fix: the loop converged on 2026-11-11. Round 8 checked the changes made after it (the paste with a control character, the tests of the last mutants run), and found only a nit in the spec, fixed. Among the findings, with Kamal's choices:
  - Round 1: a program that left the alternate screen on a screen of 3 rows ended stepbook with a panic, and a crafted file took 10 s to lay out at 16 columns when the view closed; `ctrl-o` in the document view opened the full-screen view; a key that waited for its frame reached a program that went full-screen.
  - Round 2: after `ctrl-c` in input mode ended a block, a second `ctrl-c` quit stepbook and ended the session. Kamal chose a 1 s pause of the keys when stepbook stops sending them to a program on its own.
  - Round 3: a program that toggled the alternate screen kept the pause on forever; no test proved that a write never blocks. Kamal kept the resize of the PTY when the view closes, though vim then redraws over the output of `:!ls`.
  - Round 4: stepbook keeps bracketed paste on, so Ghostty took each paste for safe, and a paste of lines ran in a shell without bracketed paste. Kamal chose a question in the footer before such a paste. Also: SIGHUP ended stepbook with 101, not by the signal, since milestone 1.
  - Round 5: a run that ended on the alternate screen hid its main screen for good. Kamal chose to keep both: stepbook leaves that screen at the end of the run, and the view after the run shows the main screen, a dim mark, then the last screen.
  - Round 6: the automatic open of the full-screen view sent keys typed ahead for the document to the program, and a program that entered the alternate screen each second kept the keys off. Kamal chose to drop the automatic open: the view opens with `o` or `i`, or from input mode. The done-when of spec 16 changed with it.
  - Round 7: no defect. Out of scope, but fixed: a paste with ctrl-o, which readline takes for Enter, now asks too.
  - The log of the loop, with its Decisions, Follow-ups and Nits deferred, is `~/.cache/stepbook-review/m3-review-loop-log.md`.
- `cargo mutants` after the loop, on the diff of the milestone: 292 mutants, 268 caught, 2 missed, 7 timeouts and 15 unviable, in 17 minutes. The 2 misses got tests (a plain `o` in input mode, the PTY size of another block while the view is open). Each timeout makes a test hang, so the tests catch it.
- The check in tmux on 2026-11-11: a `read -s` prompt takes a password in input mode, with the cursor after `Password: `; `o` shows `less`, and `q` closes the view, then the keys pause for 1 s; a paste reaches the program; a paste of two lines asks first; the view after the run scrolls from the end; a run that ends on the alternate screen shows both screens; SIGHUP gives 129, and `q` gives 0 with the cursor shown.

### Kamal's manual checklist of milestone 3

What tmux cannot show, in herdr in Ghostty: open `~/dev/claude/runbooks/2026-11-11-stepbook-input-full-screen.md` with `stepbook --no-log`, and follow its six sections:

1. A paste with your terminal's paste key reaches the program in input mode.
2. A paste of two lines asks first: `n` cancels it, `y` sends it.
3. A password in input mode: the cursor stands after `Password: `.
4. `o` shows `less`; a mouse selection copies only its text; `q` closes the view.
5. After the run, `o` shows the output from its end, and `k` scrolls back.
6. The look of the footer modes, the cursor, and the dim mark after a run that ended full-screen.

Kamal ran the checklist on 2026-11-12: the six checks pass. Section 5 first scrolled the output of `pager`, which is one line: less draws on the alternate screen, and that screen goes away when less exits. Section 5 now runs `seq 200`, and section 6 explains the alternate screen.

## Look and feel review

Kamal chose from the screens of a prototype, drawn in his theme, on 2026-11-12. What bothered him most: blocks that run blend into the rest of the document, and the `▐` bar alone does not set them apart. His choices:

- Blocks that run get a background darker than the background of the terminal, from the column after the `▐` to the right edge of the text. The selected block gets a darker one. The output keeps the background of the terminal. On his Gruvbox Dark (`#282828`), the prototype drew `#1c1c1c` and `#141414`. stepbook asks the terminal for its background color (OSC 11) at startup: herdr answers it.
- Prose takes at most 100 columns: paragraphs, headings, rules, lists and quotes. Code, output and tables keep the full width of the view.
- The selection is bold blue, not bold cyan: with bold-is-bright, Gruvbox draws bold cyan as light green, close to the green of `✔`.
- The code of other code blocks, which never runs, is dim, as their gutter is.
- Output rows have a dim `▕` in the column of the gutter, in the selection color on the selected block. A program cannot write to that column, so its output can no longer pass for the code of a nested block.

Two fixes need no choice: a question in the footer always shows its `(y/N)`, and the document view gets 1 column of padding at its right, so that the rules under H1 and H2 no longer touch the border.

The plan, one commit per line, on the branch `look-and-feel`:

1. `docs`: this plan.
2. `fix`: 1 column of padding at the right of the document view.
3. `feat`: prose takes at most 100 columns (5.3).
4. `feat`: the selection is bold blue (6.1).
5. `feat`: the code of other code blocks is dim (5.3).
6. `feat`: output rows have a mark in the column of the gutter (6.3).
7. `fix`: a question in the footer always shows its `(y/N)` (7.4).
8. `feat`: blocks that run get a darker background, from the answer of the terminal to OSC 11 (5.3, 6.1).

Done when: each runbook in `~/dev/claude/runbooks/` opens with no panic, the tmux check shows each change, and Kamal checks the look in herdr in Ghostty.

### After the 8 commits

- Two notes on the background: the 30% and 50% of the prototype became fixed steps of 12 and 20 on each RGB value, which give the same colors on `#282828` and stay light on a light theme. On a background whose values are all under 20, the shades go lighter.
- `cargo mutants` on the diff: 84 mutants, 75 caught, 4 missed, 5 unviable. The first run tested nothing: `the_output_of_a_process_substitution_ends_before_the_run` failed the baseline under load, so the runs of this branch skip it (it covers run.rs, which the branch does not change). The misses got a test of an answer that comes late, and a simpler search for the answer to DA1.
- `/simplify`, with 4 cleanup agents: 1 commit. The output gutter takes the selection style from the gutter closure, doc.rs dims other code where it makes its spans, a plain function makes the shades, and the footer adds the `(y/N)` in the arm of the question.
- Review loop, with a code reviewer and a security reviewer each round. Accepted findings per round: 3, 2, 0. It converged on 2026-11-12.
  - Round 1: the startup query read 256 bytes at a time, and could take the start of a bracketed paste: crossterm then read the rest of the paste as keys, and a line feed in it ran the selected block. The query now reads a byte at a time, never past the answer to DA1, and drops a paste that starts before the answers to its end. The spec also said "code that cannot run" for other code blocks, which also covers a block with a duplicate name, and claimed that output rows have their mark alone in the column of the gutter: a block in a quote has its `│` there.
  - Round 2: the query gave up on a paste after a pause of 0.5 s, and could stop inside the start of a paste. It now waits up to 5 s for each byte of a paste, or of the start of one. No test caught a broken test of the end of DA1.
  - Round 3: no defect. Test gaps and a test that could fail under load, fixed.
  - Deferred: a paste of 1 MB at the instant of startup takes about 3 s to drop; a pause of more than 5 s within such a paste still splits it (spec 5.3); on a terminal that keeps the ESC of a paste, a crafted paste can set a wrong shade; in dim code, the escape of a hidden grapheme stays bright yellow.
  - The log of the loop, with its Decisions and Nits deferred, is `~/.cache/stepbook-review/lf/review-loop-log.md`.
- `cargo mutants` after the loop, on the diff of the branch: 91 mutants, 85 caught, 6 unviable, none missed, in 8 minutes.
- The check in tmux on 2026-11-12: tmux answers OSC 11 with `#1c1c1c`, and blocks get `#101010` and `#080808`; the output of a block keeps the background of the terminal, with the `▕` mark; a `toml` block is dim; at 45 columns, the question to quit ends with `… (y/N)`; each of the 94 runbooks of `~/dev/claude/runbooks/` opens, and `q` gives exit code 0.

### Kamal's manual checklist of the look and feel

What tmux cannot show, in herdr in Ghostty: open `~/dev/claude/runbooks/2026-11-12-stepbook-look-and-feel.md` with `stepbook --no-log`, run its blocks, and check:

1. Blocks are `#1c1c1c` and the selected block `#141414`, from the column after the `▐`; the output and the `toml` block keep the background.
2. The selection is blue; the `toml` block is dim; output rows have the thin `▕`.
3. On a wide pane, prose stops at 100 columns, and code and output take the full width.
4. In a pane of about 45 columns, `q` while `logs` runs shows `(y/N)`.

Kamal ran it on 2026-11-12. The look is good, with three changes, on the branch `look-and-feel-2`:

- In Ghostty, the `▕` of output rows stood 5 pixels from the `│` of a quote, in the same dim color: the output of a block could pass for the code of a block in a quote. Kamal chose a dashed `╎`. The demo runbook got a block in a quote, and a block whose output imitates it.
- Bug: while `logs` printed, a block selected under it moved down, and out of the view. The view now keeps the rows under an output in place when that output is above the top of the screen, or above the selected block while the selected block is on the screen (6.3).
- Bug: after a run of the last block, then of the block above it, `j` onto the last block did not show its output, and only `o` did. `j`, `k`, `Home` and `End` now show the whole block, then as much of its output as fits, then the end of its paragraph (7.1).

### After the fixes of the look and feel

- `/simplify`, with 4 cleanup agents: `set_output` built the rows of the whole document on each call; it now builds them only when the row count of an output changes.
- Review loop, with a code reviewer each round. Accepted findings per round: 1, 4, 1, 1, 1, 0. It converged on 2026-11-12. All the findings were edges of the rule that keeps the view in place, and none was in the two behaviours that Kamal asked for:
  - Round 1: the view moved when output grew under the screen, with the selected block off the screen after Page Up onto prose.
  - Round 2 (a blocker): a block that fit in the view, but not with its output, showed from its paragraph, with lines of its code cut off, and Enter ran it. 54 blocks of the runbooks were in this case at 80x24. Also the mirror of round 1, after Page Down, and a test of output that grows on the screen.
  - Round 3: on a document shorter than the view, the kept scroll went past its end, and took effect later.
  - Rounds 4 and 5: the stored scroll stayed too large after output at the end shrank; a test of the stop at the end.
  - Round 6: no defect. The reviewer also ran a property test of 3,000 random documents and keys, and both bugs in tmux.
  - Deferred: a 1-row boundary of `scroll_after` has no test, an output that ends at the top of the screen. Rejected: a wrap of the info line of a block above still shifts the selected block by a row (spec 6.3 names only output).
  - The log of the loop is `~/.cache/stepbook-review/lf/review-loop-log-2.md`.
- `cargo mutants` on the diff of the branch: 14 mutants after the 3 commits, all caught; 37 after the loop, 34 caught, 2 missed, 1 unviable. One miss got a test, a selected block that starts on the row under the screen. The other is equivalent and excluded: a selected block that ends at the top of the screen is under an output above the top too.
- The check in tmux on 2026-11-12: `warn`, selected under `logs`, stays on its row while `logs` prints 15 lines; after `imitation` then `quoted`, `j` shows the output of `imitation`.

What tmux cannot show, in herdr: open the same runbook, and check that the dashed `╎` of output rows reads apart from the `│` of the quote, at the end of the runbook.

Kamal checked it on 2026-11-12: it looks fine.

## Milestone 4: log and reload

Kamal's choices on 2026-11-12: stepbook does not start if the log would be the runbook itself (spec 4); the watcher uses inotify through rustix, not the `notify` crate; the log removes escape sequences with `vte`, a direct dependency at the version of vt100.

The plan, one commit per line, on the branch `m4-log-reload`:

1. `docs`: this plan, and Kamal's choices (spec 4, 10, 13).
2. `feat`: stepbook does not start if the log would be the runbook itself (4).
3. `feat`: the cleaning of the output for the log: escape sequences removed with `vte`, CRLF as LF, a carriage return alone drops the partial line, U+FFFD for bytes that are not UTF-8, and a character split across two reads whole (12.1).
4. `feat`: the log: one section per run, with its `START` and `END` lines, in a file created with mode 0600, and the output as it arrives; one marker line for output on the alternate screen (12.1). A thread of its own writes the log: a slow disk must not block the main thread (13).
5. `feat`: one run writes to the log at a time: a run that starts meanwhile keeps its output in memory, then writes its section with its real start time (12.2). Kamal chose, during the work, that a run that ends meanwhile writes its section at once, and the other section goes on after a `=== CONTINUE <name>` line: a block that never ends held the log.
6. `feat`: a block whose first line starts with the `exec > >(tee -a ` header of old runbooks writes no section (12.3).
7. `feat`: block keys, and the matching of the blocks of two versions of the file (3, 10).
8. `feat`: `ctrl-r` reloads the file: outputs, results and the selection follow the matching; changed blocks show `changed since run`, and removed blocks that run stay until their run ends; the footer gives a summary (10).
9. `feat`: the watcher: inotify on the parent directory, and a reload 300 ms after the last change (10).

Done when (spec 16): `watch.sh` sees the `END` line of a run, and two concurrent runs write contiguous sections. An edit of the runbook in vim during a run keeps each output and marks the changed blocks.

Kamal signed off the plan on 2026-11-12.

The 9 commits are in, with these choices of Kamal during the work:

- The times of the log are in UTC, marked `Z`: the standard library has no time zones, and libc or a time crate would be one more dependency.
- A run that ends while another writes to the log writes its whole section at once, and the other section goes on after a `=== CONTINUE <name>` line: a block that never ends held the log, and `watch.sh` waited for every block run after it.

Two choices of Claude, recorded in the spec: a block that a reload removes while it runs stays at the end of the document, not only in the sidebar, since `i` and its output need a place in the view; the key of an unnamed block is its code, not a hash of it.

The check in tmux on 2026-11-12, with `--log`: `watch.sh` printed the `END` line of a short block at once, while a long block ran, then the `END` of the long block, and exited; the log holds the section of the short block within that of the long block, which goes on after `=== CONTINUE long`; an edit of the runbook with vim during the run kept both outputs, marked the changed block with `~` and `changed since run`, and the footer said `reloaded: 1 changed`.

### After the 9 commits

- `cargo mutants` on the diff of the milestone: 138 mutants, 97 caught, 11 missed, 30 unviable, in 10 minutes. The misses got tests (a plain `r`, a question and the full-screen view that follow their block through a reload, the code that a removed block keeps, and `ESC[?25h`, which the log must not take for the alternate screen) or a simpler watcher; the mutants of the inotify flags are excluded: the bits are distinct.
- `/simplify`, with 4 cleanup agents, on 2026-11-12: their findings are in `~/.cache/stepbook-review/lf/m4-simplify-findings.md`. The triage gave 9 commits, from 4d10a51 to 5828c51:
  - Four defects, fixed: the log reads the alternate screen as vt100 does (modes 47 and 1049, and `ESC c` leaves it), under a property test that compares the two; the summary of a reload counts a removed block that still runs once; a question that a reload moves names its block by its new label; the channel to the log thread holds 64 messages, and while it is full, the main thread reads no output of the runs that write to the log, so that a slow disk slows their programs.
  - Two more bounds on memory: a line of the log longer than 1 MiB is split (Claude's choice), and a run that waits for the log writes what it holds past 1 MiB, then goes on after a `=== CONTINUE <name>` line (Kamal's choice: two `tail -f` blocks held the output of the second one for the whole session).
  - Cleanups: the inverse of the matches of a reload is built once, the writer of the log comes from the order of the run IDs, App drops the state that the session holds, and a reload of the same text does no parse.
  - Skipped, with the reasons in the log of the review loop: the copies on the hot path of the cleaner, a BufWriter on the log, the parse at the end of a removed block, and three more.
- Review loop, with a code reviewer and a security reviewer each round, in night mode. Accepted findings per round: 10, 3, 2, 2, 1, 0. Round 6 found no blocker and no should-fix: the loop converged on 2026-11-12. Among the findings:
  - Round 1 (a blocker): the log followed a link, so a runbook from someone else could come with `run.log -> ~/.ssh/authorized_keys`. The log must now be a regular file of the user, not a link. Also: a failed END write kept the log from later runs; `y` ran a block that a reload gave a problem; a notification hid in input mode; the end waited on a stuck log thread for good; the read of a reload could block on a FIFO or a lease; a program could print an END line for watch.sh; a runbook behind a link never reloaded; an Enter typed as another writer saved the file ran a block that the user had not seen.
  - Round 2: a log with other hard links; a Re-run question that a code change kept; held text kept on a failed CONTINUE write.
  - Round 3: the pause after a reload in the full-screen view after a run; `o` kept a stale notification.
  - Round 4: one report of a full disk could hide behind a question; Decided rows. Rejected: a check of the directory of the log, since whoever can write it can replace the runbook too (spec 12.1 says so).
  - Round 5: changes of the runbook queued reloads ahead of the keys and the signals; the watcher now merges them.
  - The log of the loop, with its Decisions and Follow-ups, is `~/.cache/stepbook-review/lf/m4-review-loop-log.md`.
- The budget script of the review loop reads the cache of claude-statusbar, which has not changed since 2026-10-11: it showed 0% and no reset time. Night mode ran on an hourly resume job in its place, which the end of the loop deleted.
- `cargo mutants` after the loop, on the diff of the milestone: 231 mutants, 180 caught, 13 missed, 1 timeout and 37 unviable, in 12 minutes. The misses got tests (the select of `App::next` with a full log and a tick, the drain at the end, the time of the pause, the selection after a reload of a runbook that had no block), a simpler question remap, or an exclusion with its reason. A second run on the functions with misses: 52 mutants, 46 caught, 5 unviable, 1 timeout (a hang that the tests catch).
- The check in tmux on 2026-11-12, with `--log`: `watch.sh` printed the END line of `short` while `long` ran, and exited 0; an edit with `vim -Es` during the run kept both outputs, marked `short` with `~` and `changed since run`, and the footer said `reloaded: 1 changed; keys off for 1 s`; a forged END line got a space first in the log; the log has mode 0600; a log that is a link stopped the start with exit code 1, and its dangling target stayed absent; `q` gave exit code 0. Each of the 95 runbooks of `~/dev/claude/runbooks/` opens with no panic.

### Choices made during the night, for Kamal to confirm

The review loop ran in night mode, and Claude made these choices alone. The spec records each one; any of them can change on a new branch.

1. A notification shows in input mode and in the full-screen view too, in place of the keys; `i`, and `o` on a running block, end a notification of the document view (6.1).
2. The log must be a regular file of the user, with no other hard link, and not a symbolic link (12.1). `--log /dev/null` now fails: `--no-log` does that job.
3. A line of output that starts with `=== START `, `=== END ` or `=== CONTINUE ` gets a space first in the log (12.1).
4. After a reload that moves the selection or changes the code of the selected block, keys pause for 1 s, unless they go to a program (10). A Re-run question ends if the reload changes the code of its block.
5. Writes to the log that fail show in the footer at most once every 10 s (12.1).
6. Open, a choice of look: the NORMAL footer does not show `^r reload`, which the example of spec 6.1 shows.

### Kamal's manual checklist of milestone 4

What tmux cannot show, in herdr in Ghostty: open `~/dev/claude/runbooks/2026-11-12-stepbook-log-reload.md` with `stepbook` (with its log), keep a second pane for vim and `tail -f` of the log, and follow its five sections:

1. Two runs in the log: the section of `short` comes whole, then `=== CONTINUE long`.
2. An edit in vim with your config during a run: both outputs stay, and `~` and `changed since run` read well in your theme.
3. The pause after a reload of the selected block: `keys off for 1 s`.
4. A notification in input mode, in place of the keys, for 3 s.
5. A forged END line gets a space first in the log.

## Next

1. Kamal pushes `main`.
2. Kamal runs the manual checklist of milestone 4, and confirms or changes the choices made during the night (above).
3. Milestone 5: migration.

Kamal pushes later: the repository has no remote yet (see the first open point).

## Open points

- Follow-ups of the review loop of milestone 4: invisible format characters (U+200B, the bidi controls) reach the log, where a human could take a forged line for an END line; a hung network mount can still block the read of a reload on the main thread; an existing log keeps its mode, such as 0644 from the old tee header; a reload shifts the `#<n>` labels, so two open sections can share a name; a block with the old logging header writes raw output into the same log, and one NUL makes `rg` in watch.sh skip the whole file (`rg -a` would fix it; milestone 5 drops the header).

- Before the first push, Kamal creates `KamalF/stepbook` on GitHub and adds the remote. The sandbox cannot write `.git/config`.
- Kamal runs `cargo publish`: it needs his crates.io token. The name `stepbook` was free on crates.io on 2026-11-09. mdame held its name early with a 0.0.1 release; stepbook can do the same after milestone 1.
- Follow-ups of the review loop of milestone 3: the input channel of a run holds up to 256 writes, pastes included, while a program reads no input; the full-screen view of an ended block makes the spans of its whole scrollback when it opens, 380 ms with crafted output that changes color on each cell at 180 columns; the answer to DSR 6 ignores origin mode, which vt100 does not expose.
- Follow-ups of the review loop: bash highlighting takes 0.1 to 0.2 s before the first frame on the largest runbook (syntect's onig backend, or highlighting on first view, would be faster: a choice for Kamal). Terminals that apply the bidi algorithm on their own (VTE, Konsole) can still reorder code with right-to-left letters. Ghostty does not.
- `run::tests::a_program_gets_the_position_of_its_cursor_from_the_terminal` failed under load too, once, during milestone 4, and passed alone.
- `the_output_of_a_process_substitution_ends_before_the_run` fails its 900 ms bound under heavy load, such as two test suites at once: the reader of the run parses 20,000 lines of output in a debug build, and tee waits on the PTY. It did so before the last changes of milestone 2 too (checked on c7e5ca9, 2026-11-11). A single suite passes.
- Claude drives stepbook in tmux from the sandbox with `scripts/tui.sh` and one steps file per check: it starts tmux, sends the keys and captures the pane. The sandbox stops the tmux server when a command ends. Checked on 2026-11-09.
