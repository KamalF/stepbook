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
| 2 | Running blocks | Next, on `m2-running-blocks` |
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

## Next

1. Plan milestone 2 in this file, one commit per line, from spec 16 and the sections that it names: 8.3, 8.2, 6.3, 6.2, 8.1, 7.4, 9 and 11. The two open points "before Enter runs blocks" come first. Kamal signs off the plan and each new dependency.
2. Build it, then `cargo mutants`, the review loop, the check in tmux and Kamal's checklist. Then the merge into `main`.
3. The look and feel review, before milestone 3.

Kamal pushes later: the repository has no remote yet (see the first open point).

## Open points

- Before the first push, Kamal creates `KamalF/stepbook` on GitHub and adds the remote. The sandbox cannot write `.git/config`.
- Kamal runs `cargo publish`: it needs his crates.io token. The name `stepbook` was free on crates.io on 2026-11-09. mdame held its name early with a 0.0.1 release; stepbook can do the same after milestone 1.
- Look and feel review, between milestones 2 and 3, once blocks show their output and state (Kamal, 2026-11-10). From the first screenshot: prose lines of about 200 characters on a wide terminal (a maximum text width of about 100 columns?); the selection in terminal cyan, which Kamal's theme draws green; the contrast between a block that runs and other code.
- Follow-ups of the review loop: bash highlighting takes 0.1 to 0.2 s before the first frame on the largest runbook (syntect's onig backend, or highlighting on first view, would be faster: a choice for Kamal); each key lays out the page twice, and each frame lays out the whole document: 1 MB of tabs in code takes about 0.9 s per frame. Milestone 2 needs a layout cache per width to redraw output, which removes both. Terminals that apply the bidi algorithm on their own (VTE, Konsole) can still reorder code with right-to-left letters. Ghostty does not.
- Before Enter runs blocks in milestone 2: nothing keeps the selected block in view, at startup or after a resize. Scroll to the selection, or run only a block in view.
- Before Enter runs blocks in milestone 2: a paste into the document view arrives as key presses, so a pasted line feed would run the selected block. Turn on bracketed paste, and ignore a paste there. Milestone 3 needs bracketed paste anyway (spec 7.2).
- Before milestone 4 writes the log: the default log of a FILE named `x.log` is the runbook itself. Spec 4 needs a rule.
- Claude drives stepbook in tmux from the sandbox with `scripts/tui.sh` and one steps file per check: it starts tmux, sends the keys and captures the pane. The sandbox stops the tmux server when a command ends. Checked on 2026-11-09.
