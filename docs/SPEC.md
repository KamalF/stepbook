# stepbook specification

stepbook shows a Markdown runbook in a terminal UI and runs its bash blocks one at a time, with each block's output under its code.

Status: draft v1, 2026-11-09.

## 1. Goals

- Run the bash blocks of a Markdown runbook one by one, with the output shown under each block.
- Carry exported variables and the working directory from one block to the next, as a terminal session does.
- Keep the output, status and session state of each block when the file changes or is reloaded.
- Write a log that the runbook skill's watcher (`watch.sh`) can read.
- Open the existing runbooks in `~/dev/claude/runbooks/` without changes.
- Make copy and paste work on Wayland (sway). stepbook uses no clipboard library, and it never reports a copy that did not happen ([upmd issue #55](https://github.com/rezigned/upmd/issues/55)).

## 2. Non-goals for v1

These upmd features are out of scope:

- Languages other than bash.
- Dependencies between blocks (`deps`), workflows, `--all`, `--yes`, CLI mode.
- File picker, stdin input, images, themes, table of contents, markup mode.
- Environment editor, search, goto, help popup, mouse support, in-app copy.
- Configuration file and key remapping.
- Windows. macOS may work but is not a target.

Copy and paste go through the terminal. See 6.4.

## 3. Terms

| Term | Meaning |
|---|---|
| Block | A fenced code block whose language token is `bash` or `sh`. |
| Block key | The block's `name`, or a hash of its code if it has no name. The key identifies the block across reloads. |
| Run | One execution of a block in its own PTY. Each run has an internal ID that is never reused. |
| Session state | The exported variables and the working directory that the next run starts with. |
| Document view | The rendered runbook, with output under each block. |
| Full-screen view | One block's terminal at full size. |
| Input mode | The mode in which keys go to the running program. |

## 4. Usage

```text
stepbook [--log PATH | --no-log] FILE
```

| Option | Effect |
|---|---|
| `FILE` | The Markdown runbook: a regular file of at most 1 MB, whose lines nest at most 1,000 levels. A level is a quote or list marker at the start of a line, or 2 columns of the whitespace at the start of the line, around the markers, with a tab as 4 columns. Required. |
| `--log PATH`, `--log=PATH` | Log file. The default is `FILE` with its extension replaced by `.log`. |
| `--no-log` | Writes no log. |
| `-h`, `--help` / `-V`, `--version` | Prints help or the version. |

- The first run starts in the current directory of the stepbook process.
- Exit code 0: normal quit. Exit code 1: startup error (file unreadable, not a regular file, larger than 1 MB or nested too deep, `bash` not found, stdout not a terminal). Exit code 2: invalid arguments.

## 5. Document

### 5.1 Parsing

- stepbook parses CommonMark with GFM tables, task lists and strikethrough (pulldown-cmark).
- The metadata-block extensions stay off. If the file starts with a `---` line, stepbook shows the lines up to the next `---` line as dimmed frontmatter. Without that closing line, the file has no frontmatter. A `---` anywhere else is a thematic break or a setext heading, as in CommonMark. This avoids upmd issue K1 (see 18).
- If the file could make more than 200,000 table cells, stepbook shows its tables as text. The bound is its lines, ended by LF or by CR, times one more than the most `|` on one line. pulldown-cmark pads each short row up to the columns of its header, so 1 MB of crafted tables takes gigabytes.
- A fenced code block with the language token `bash` or `sh` is a block. Both run with bash.
- Other code blocks, including indented ones, are shown as code and cannot run.

### 5.2 Attributes

The info string uses the upmd syntax, so existing runbooks open unchanged:

````markdown
```bash [name:restart-api]
systemctl --user restart api
```
````

- The language token is the text before the first `[`.
- The attribute section is `key:value` pairs between `[` and `]`, separated by commas or spaces. A key matches `[A-Za-z0-9_-]+`. A value is either unquoted (`[A-Za-z0-9_+#/.~-]+`) or double-quoted. In quoted values, `\"` and `\\` are decoded.
- stepbook reads the info string as written in the file. CommonMark decodes backslash escapes and entities in info strings, which would change quoted values.
- `name` is the only recognized key. A name must match `[A-Za-z0-9_.-]+`. If `name` is given twice, the first one counts, and the info line shows `name is given twice`.
- An unknown key, a syntax error or an invalid name shows a warning on the block's info line, in the warning color. The block can still run. After a syntax error, the pairs before it still count.
- A warning never quotes free text of the info string, such as a value or the text after `]`: the info line wraps, and a quoted value with spaces could make a line that passes for code. A warning quotes at most 20 characters of a key.
- If several blocks have the same name, all of them show `duplicate name` and cannot run until the file is fixed.

### 5.3 Rendering

| Element | Rendering |
|---|---|
| Heading | Bold. H1 and H2 have a rule under them. |
| Paragraph | Word-wrapped to the view width. A soft line break is a space, and a hard line break is a new line. |
| Emphasis, strong, strikethrough, inline code | Terminal styles. Inline code uses a distinct color. |
| Link | The link text, underlined. |
| List, task list | Bullets `•` and `◦`, which alternate by level, or numbers. A nested list starts under the text of its item, so bullet lists indent 2 columns per level. Task items show `[ ]` or `[x]`. No Nerd Font glyphs. |
| Blockquote | A `│ ` prefix per level. |
| Table | Box-drawing borders and a bold header. If the table is too wide, its widest columns shrink, and their cells end with `…`. If the table is still too wide with all its columns 1 column wide, the columns past the width of the view do not show. |
| Thematic break | A dimmed rule. |
| Raw HTML | Shown dimmed, as source. An HTML block keeps its lines, which wrap at any character. A line end in inline HTML is a space. |
| Image | `[image: <alt text>]`. |
| Block | An info line, the code with a gutter, then the output (see 6.3). Bash code is syntax-highlighted, except in a block with a line longer than 1,000 bytes, or in a block that would bring the highlighted code of the file past 32 KB, or past 4,000,000 for the sum of the squares of its line lengths. Long lines wrap at any character, so the view shows all the code that runs. A long info line wraps too, so its warnings always show. A blank line always follows a code block, even in a tight list: text right under it could pass for code. |
| Other code block | The code with a dimmed gutter, not highlighted. Long lines wrap as in a block. |

All colors come from the terminal's 16-color palette, so they follow the terminal's theme. stepbook sets no background color.

No nesting depth can crash stepbook: it walks the document without recursion (upmd issue S9, see 18). Prefixes show only the innermost 10 levels of blockquotes and list items, and they take at most half the width of the view: the outer levels go first, and the innermost level always stays. The text and the code keep room in the view, and a deep item keeps its marker.

## 6. Screen

### 6.1 Layout

```text
┌─ blocks ───────┬─ 2026-11-09-restart-api.md ─────────────────────┐
│ ✔ check        │ ## Restart the service                          │
│▶● restart      │ Why: apply the new config.                      │
│   logs         │                                                 │
│                │ ▌ restart                    ● running 0:04     │
│                │ ▌ systemctl --user restart api                  │
│                │ ▌ systemctl --user status api                   │
│                │   ● api.service - API                           │
│                │     Active: activating (start)                  │
│                │                                                 │
│                │ ## Check the logs                               │
└────────────────┴─────────────────────────────────────────────────┘
 NORMAL  ↑↓ move  ↵ run  i input  o output  ^r reload  q quit
```

- **Sidebar:** one row per block, with a status glyph and the name. An unnamed block shows `#<n>`, where n is its position among all the blocks of the file, from 1. Its width, with its left border, is the longest name plus 4, between 12 and 30 columns. A longer name ends with `…`. The sidebar scrolls to keep the selected row visible, and it is hidden on terminals under 80 columns.
- **Document view:** the rendered runbook. The selected block has a highlighted gutter and sidebar row.
- **Footer:** the mode, the keys that apply now, and notifications. A notification lasts 3 seconds.
- **Minimum size:** on a terminal smaller than 40 columns or 8 rows, stepbook shows only `Enlarge the terminal to at least 40 columns and 8 rows.`, and only the keys that quit work. Below that size, code under a list marker has no room, and the layout of a crafted file costs too much.

### 6.2 Block states

| State | Glyph | Info line |
|---|---|---|
| Not run | ` ` | name |
| Running | `●` (blinking) | `running m:ss` |
| Succeeded | `✔` | `rc=0 m:ss` |
| Failed | `✘` | `rc=N m:ss`; for a signal, `rc=130 SIGINT` |
| Changed | `~` plus the last result | `changed since run`, then the last result |
| Cannot run | `!` | the reason: duplicate name, cannot start, and so on |

### 6.3 Output under a block

- Each block that has run shows its output under the code. The output stays until the block runs again. Reloads do not clear it.
- stepbook shows the last 15 rows of the block's terminal, scrollback included. Trailing empty rows are trimmed, except the cursor row while the block runs.
- PTY size: the document text width minus the gutter, by the terminal height minus 1. It follows terminal resizes.
- Scrollback keeps 5,000 lines per run.
- A UTF-8 character that is split across two reads of the PTY shows whole (upmd issue K18, see 18).
- When the selected running block switches to the alternate screen (vim, less, top), stepbook opens the full-screen view in input mode. When the program leaves the alternate screen, stepbook returns to the document view. If the block is not selected, its info line shows `full-screen program, press o`.
- When the selected block's output grows past the bottom of the view, the view scrolls to keep its last row visible.

### 6.4 Copy and paste

The terminal handles text selection, copy and paste. They work on every display server that the terminal supports, including Wayland. Terminals and multiplexers that copy on selection (for example herdr in Ghostty) work with no extra step.

To keep this working, stepbook never interferes with the mouse:

- It never turns on mouse reporting in the outer terminal.
- It never writes a program's raw output to the outer terminal. Programs are drawn through the vt100 screen, so a program cannot turn mouse reporting on either.

- A selection in the document view also contains the sidebar and the gutter. To copy output without them, open the full-screen view (`o`) and select there. That view shows only the block's terminal.
- A paste from the terminal (for example `ctrl-shift-v`) goes to the program in input mode. See 7.2.

### 6.5 Control characters

stepbook never sends a control character or a bidi control from the file or from a run to the terminal. Otherwise a runbook could hide part of a command, or write to the clipboard (upmd issue S1, see 18). A bidi control can reorder text in terminals that support right-to-left scripts.

In this section, control characters are the C0 characters, DEL and the C1 characters. Bidi controls are U+061C, U+200E, U+200F, U+202A to U+202E and U+2066 to U+2069.

- stepbook shows each control character as vim does: `^[` for ESC, `^M` for a carriage return, `^?` for DEL, and `<9b>` for a C1 character. It shows a bidi control as its code point, such as `<202e>`. The escapes use the warning color.
- This applies to the text of the file, to its name, to text from a run, such as a directory name in the footer, and to the error messages on stderr.
- A tab is not escaped. It expands to spaces, up to the next multiple of 8 columns.
- A line feed is a line break in the text and the code of the file. In one-line text, such as a file name, it shows as `^J`.
- Code also escapes the default ignorable code points of Unicode, which a terminal draws as nothing, such as U+200B (zero-width space): `"prod<U+200B>"` would look like `"prod"`. In code, a grapheme of several code points keeps its first character and shows the others escaped, and so does a grapheme of width 0, such as a combining mark at the start of a line. A terminal may count the columns of such a grapheme otherwise than ratatui, and shift the rest of the line over a space: `k` and 8 × U+302E is 1 column for ratatui and 17 in tmux. Code also escapes the spaces other than U+0020, such as U+00A0 (no-break space), which bash reads as part of a word: `true<U+00A0>#; curl x | sh` would look like a comment, and run curl. Code also escapes the format characters that join the next character, such as U+070F, the format characters U+FFF9 to U+FFFB and U+13430 to U+1343F, and U+2D7F: a terminal may give them a column that ratatui does not count, and shift the rest of the line. Prose keeps all of them: emoji, some scripts and no-break spaces need them, and there they do not change what runs.
- If the code of a block holds a character that code escapes, other than a tab or a line feed, its info line shows the warning `hidden characters in code`. The block can still run.
- The view and the run use the same code text, so what runs is what the view shows.
- Program output reaches the terminal only through the vt100 screen. See 6.4.

## 7. Keys

### 7.1 Document view

| Key | Action |
|---|---|
| `↓`, `j` / `↑`, `k` | Select the next / previous block, and scroll so that the block and the paragraph above it are visible. |
| `Home` / `End` | Select the first / last block. Without blocks, scroll to the top / bottom. |
| `PageDown` / `PageUp` | Scroll one page. The selection moves to the first block whose info line is in view, else to the first block in view. |
| `Enter` | Run the selected block. See 8.1. |
| `i` | Enter input mode, if the selected block is running. |
| `o` | Open the full-screen view of the selected block, if it has run. |
| `ctrl-r` | Reload the file now. |
| `q`, `ctrl-c` | Quit. See 11. |

### 7.2 Input mode

- All keys go to the program, except `ctrl-o`, which leaves input mode.
- stepbook leaves input mode when the run ends.
- A paste from the outer terminal goes to the program. It is wrapped in `ESC[200~ … ESC[201~` if the program turned on bracketed paste mode.
- Key encoding follows xterm: application cursor mode, modifiers on arrow keys (`ESC[1;5A`), F1 to F12, Insert, Delete, Home, End, Page Up and Page Down.

### 7.3 Full-screen view

| Key | While running | When done |
|---|---|---|
| `ctrl-o` | Back to the document view | Back to the document view |
| Other keys | Go to the program (input mode) | `↑`/`↓`, `PageUp`/`PageDown` scroll the scrollback. `o`, `q` and `Esc` go back. |

While the view is open, the PTY takes the full terminal size. It gets its document size back when the view closes.

### 7.4 Confirmation dialogs

`y` confirms. `n`, `Esc` and every other key cancel.

## 8. Running blocks

### 8.1 Starting a run

`Enter` on the selected block acts on its state:

| Block state | Action |
|---|---|
| Not run | Starts a run. |
| Running | Shows `already running: i to type, o to view`. Never starts a second run of the same block (upmd issue K5, see 18). |
| Succeeded, failed or changed | Asks `Re-run <name>? (y/N)`. On `y`, clears the block's output and starts a run. |
| Cannot run | Shows the reason. |

Several blocks can run at the same time. Each one runs in its own PTY.

### 8.2 Process

1. stepbook checks that the session directory exists. If it does not, the run fails with `cannot start: directory <dir> no longer exists`. The session directory then resets to the directory that stepbook started in, and the footer says so. stepbook never falls back to `$HOME` (upmd issue S6, see 18).
2. stepbook writes the block code to `<workspace>/run-<id>.sh`.
3. It spawns `bash --noprofile --norc <workspace>/wrapper.sh <run files>` in a new PTY, in the session directory.
4. The child gets exactly the session variables, plus:
   - `TERM=xterm-256color`
   - `STEPBOOK_FILE`: the absolute path of the runbook
   - `STEPBOOK_LOG`: the absolute path of the log, or empty
   - `STEPBOOK_BLOCK`: the block name
5. The wrapper sources the block script in its own shell, so `export`, `cd` and shell options work as in a terminal.
6. The run ends when the process exits and its output has been read. Its exit code is the shell's exit code. A shell killed by signal N reports 128+N.

stepbook checks at startup that `bash` is on the `PATH` and fails if it is not. There is no timeout on runs.

### 8.3 Workspace

- The workspace is `$XDG_RUNTIME_DIR/stepbook-<pid>/`, or `<temp dir>/stepbook-<pid>/` if `XDG_RUNTIME_DIR` is not set.
- The directory has mode 0700 and its files have mode 0600 (upmd issue S4, see 18).
- stepbook deletes the workspace when it exits, including after a panic or a SIGTERM or SIGHUP.

## 9. Session state

### 9.1 Contents

The session state is a map of exported variables and one working directory.

- At startup, it is stepbook's own environment and current directory.
- These variables are excluded: `_`, `SHLVL`, `PWD`, `OLDPWD`, and all `STEPBOOK_*`.

### 9.2 Capture

When the block script ends, the wrapper writes the state file `<workspace>/run-<id>.state`:

- the working directory, then each exported variable as `NAME=value`;
- each entry ends with a NUL byte, so values can hold newlines, quotes and any other byte.

stepbook splits the file on NUL bytes. It never parses shell syntax, which removes upmd issues K11 and S3 by design (see 18).

The wrapper saves the state in all these cases:

| How the script ends | State saved |
|---|---|
| Runs to its last line, whatever the exit code | Yes |
| Calls `exit N` | Yes |
| Stops because of `set -e` | Yes |
| Installs its own `EXIT` trap | Yes. The wrapper runs its handler and the script's handler. |
| Stops on `ctrl-c` (SIGINT), SIGTERM or SIGHUP | Yes |
| Killed with SIGKILL, or replaces itself with `exec` | No. The info line shows `state not saved`. |

### 9.3 Applying the state

When a run ends, stepbook compares the captured state with the state the run started with, and applies only the difference to the session:

- Variables that the block set or changed are updated.
- Variables that the block unset are removed, so `unset` works (upmd issue K12, see 18).
- The working directory changes only if the block changed it.
- The state applies whatever the exit code, as in a terminal.

Two blocks that run at the same time do not overwrite each other's unrelated changes. If both change the same variable, the block that ends last wins.

## 10. Reload

stepbook re-reads the file when it changes on disk. It watches the parent directory, because editors often replace the file with a rename, and waits 300 ms after the last change. `ctrl-r` re-reads the file at once.

Blocks are matched by key between the old and the new file:

| After the reload | Result |
|---|---|
| Same key, same code | Keeps its output and state. |
| Same name, changed code | Keeps its last output and result, marked `changed since run`. |
| New key | Not run. |
| Key removed, block not running | Removed from the view. Its output stays in the log. |
| Key removed, block running | The run continues. The block stays in the sidebar, marked `removed`, until the run ends. `i` and `o` still work on it. |

- Session state is never touched by a reload.
- The selection stays on the same block, or moves to the next block if that block was removed.
- The footer shows a summary, for example `reloaded: 2 new, 1 changed`.
- Parse problems never stop a reload. They appear as warnings on the info lines.

## 11. Quit

- If no run is active, `q` and `ctrl-c` quit at once.
- If runs are active, stepbook asks `N blocks running. Stop them and quit? (y/N)`. On `y`:
  1. It closes each PTY, which sends SIGHUP.
  2. It waits up to 2 seconds, then sends SIGKILL to processes still alive.
  3. It writes their `END` lines to the log and quits.
- While it runs, stepbook turns the autowrap of the terminal off: a character that the terminal draws wider than ratatui counts must not wrap into the row below and move what is there.
- On exit, stepbook restores the terminal, autowrap included, and deletes the workspace. A panic hook restores the terminal too.

## 12. Log

### 12.1 Format

stepbook appends to the log file, which it creates with mode 0600. Each run writes one section:

```text
=== START restart 14:03:07
● api.service - API
     Active: active (running)
=== END restart rc=0 14:03:09
```

- The `=== START <name>` and `=== END <name> rc=<N>` lines keep the runbook skill's format. `watch.sh` matches `^=== END <name> rc=` and keeps working.
- An unnamed block uses `#<n>` as its name.
- Output is written as it arrives, with ANSI escape sequences removed and CRLF turned into LF.
- A UTF-8 character that is split across two reads is written whole (upmd issue K18, see 18). Bytes that are not valid UTF-8 are written as U+FFFD.
- A carriage return without a line feed discards the current partial line, as a terminal does. A progress bar therefore logs only its last state.
- Output on the alternate screen is not logged. The line `[full-screen program output not logged]` marks it once per run.

### 12.2 Concurrent runs

Only one run writes to the log at a time, so sections never interleave:

- A run that starts while another run writes to the log buffers its output in memory.
- When the log is free, the waiting run writes its `START` line (with its real start time) and its buffered output, then continues live.

### 12.3 Old runbooks

Old runbooks start each block with the skill's logging header: `exec > >(tee -a <log>) …`. If the first line of a block starts with `exec > >(tee -a `, stepbook does not log that run. The header already writes the section, so this avoids duplicates.

## 13. Architecture

| Module | Role |
|---|---|
| `main.rs` | Arguments, terminal setup and restore, the event loop. |
| `doc.rs` | Markdown to render model and blocks, attribute parsing, block keys, reload matching. |
| `view.rs` | Layout and rendering: sidebar, document view, footer, dialogs, full-screen view. |
| `run.rs` | Workspace, wrapper script, PTY spawn, reader threads, state file, diff and merge. |
| `term.rs` | vt100 screen to ratatui lines, key encoding for the PTY. |
| `log.rs` | Log writer, log ownership queue, ANSI stripping. |
| `watch.rs` | File watcher. |

- Rust, one crate, no async runtime, no component framework. The target size is 2,000 to 3,000 lines without tests.
- Dependencies: `pulldown-cmark`, `ratatui` with `crossterm`, `portable-pty`, `vt100`, `syntect` (bash syntax only), `notify`, `crossbeam-channel`, `anyhow`, `signal-hook` (8.3). For tests only: `insta`, `tempfile`, `proptest`. A new dependency needs Kamal's approval and a line here.
- The main thread owns all state. Other threads (terminal input, one reader per run, the file watcher, a tick timer) only send events.
- Control events (keys, paste, resize, process exit, file change) go to their own channel. The loop handles them before pending output, so keys stay responsive while a block prints a lot.
- Output goes through a bounded channel per run. A slow UI slows the program, and no output is lost.
- Every message from a run carries its run ID. Messages from a run that is no longer current for its block are dropped (upmd issue K5, see 18).

## 14. Tests and tooling

### 14.1 Tooling

mise manages the repository. `mise.toml` pins the Rust toolchain, `cargo-insta` and `cargo-mutants`, and defines the tasks:

| Task | Does |
|---|---|
| `fmt` | `cargo fmt` |
| `lint` | `cargo clippy --all-targets -- -D warnings` |
| `test` | `cargo test --no-fail-fast` |

CI on GitHub Actions runs on Linux: `cargo fmt --check`, then `mise run lint` and `mise run test`. The actions are pinned by SHA.

### 14.2 Rules

Each test states one rule, and its name is that rule.

- **One home per rule.** A rule is tested at the lowest layer that sees it with the real parts: real bash in a PTY, real files in a temporary directory. No mocks. Higher layers test only the wiring.
- **Overlap needs a reason.** Two tests of one rule are allowed only if they check different things, and a comment says what.
- **The oracle follows the input space.** Few inputs: a table of examples. Many inputs, such as info strings, Markdown or PTY output: a property test with `proptest`. Large output: an insta snapshot, but never as the only check of a security rule such as 6.5.
- **Snapshots change on purpose.** Each changed snapshot is reviewed. Never accept all of them at once.
- **Decisions live in pure functions.** The code that touches the terminal, the PTY or the files stays thin. Integration tests, the check in tmux or Kamal's manual checklist cover it.
- **A test stays only if it can fail for a reason that no other test catches.** A bug fix adds a regression test only if no test caught the bug. Libraries are not tested, except for a non-obvious feature that stepbook relies on.
- **Mutation testing measures the suite.** At the end of each milestone, `cargo mutants` runs. A surviving mutant is a rule with no test, or dead code. A mutant that no test should catch goes in `.cargo/mutants.toml`, with a comment that says why.
- **The suite stays under 10 s.**

### 14.3 Cases

- **Unit tests:** attribute parsing, block extraction (frontmatter, `---` in the middle of a file, duplicate names), reload matching, state diff and merge, log line cleaning (ANSI, CR, CRLF, invalid UTF-8), a UTF-8 character split across two reads (on the screen and in the log), a file with 100,000 nested `>`.
- **Integration tests**, which run real bash in a PTY without a terminal:
  - `export`, `unset` and `cd` carry over.
  - `exit 3` saves the state and reports rc 3.
  - `set -e` failure saves the state.
  - A script with its own `EXIT` trap: both handlers run, and the state is saved.
  - Values with newlines, quotes and `$'…'` survive a round trip.
  - A removed working directory makes the run fail and resets the session directory.
  - `ctrl-c` (byte `0x03`) reports rc 130 and saves the state.
  - Two concurrent runs merge their changes.
  - Concurrent runs write contiguous log sections.
  - The old logging header suppresses the native log.
- **Render tests:** snapshots with ratatui's `TestBackend` and `insta`, covering document elements, control characters (in the code, the text, the file name and a directory name), block states, the sidebar and the footer modes.

## 15. Migration

- All the runbooks in `~/dev/claude/runbooks/` open and run unchanged: 62 on 2026-11-09.
- When v1 works, update the runbook skill:
  - Drop the logging header from the block format.
  - Use `stepbook <file>` instead of `upmd <file>`.
  - Replace the `ctrl-r` instruction: the file reloads by itself.
  - The log path (`<file>.log`) and `watch.sh` stay the same.

## 16. Build order

v1 comes in five milestones, built in order. Each milestone ends with `cargo mutants` and a review loop. The next one starts after its merge into `main`.

1. **Document view.** The crate and its tooling (14.1), and the testing rules (14.2). Then the arguments (4), the terminal setup and restore with the panic hook, parsing (5.1, 5.2), rendering (5.3), control characters (6.5), the layout (6.1), and the keys that move and quit (7.1).
   - Done when: each runbook in `~/dev/claude/runbooks/` opens with no panic. Snapshot tests cover each element of 5.3 and each case of 6.5. `q` restores the terminal.
2. **Running blocks.** The workspace (8.3), the wrapper and the PTY (8.2), the output under each block (6.3), the block states (6.2), `Enter` and its dialog (8.1, 7.4), the session state (9), and quit with active runs (11).
   - Done when: the integration tests of 14 pass, except the log tests. In tmux, a block that runs `export` and `cd` changes what the next block sees.
3. **Input and full-screen view.** Input mode (7.2), the full-screen view (7.3), and the switch to the alternate screen (6.3).
   - Done when: a `read -s` prompt takes a password in input mode, and `less` opens the full-screen view and closes it on exit. In herdr, a paste reaches the program (manual check).
4. **Log and reload.** The log (12), the file watcher and the reload (10).
   - Done when: `watch.sh` sees the `END` line of a run, and two concurrent runs write contiguous sections. An edit of the runbook in vim during a run keeps each output and marks the changed blocks.
5. **Migration.** A README, an install with mise from the GitHub repository, and a first release on crates.io. Then the runbook skill moves to stepbook (15).
   - Done when: the runbook skill opens its runbooks with stepbook, and Kamal has run a real runbook with it.

## 17. Later

These ideas are not in v1:

- Ask for confirmation before running a block whose `Why:` line contains `**Destructive:**`.
- Keep the session state and block results across stepbook restarts.
- Search (`/`) and go to block.
- A configuration file for keys and the inline output height.

## 18. upmd issues

stepbook avoids these upmd issues. The K and S IDs come from a review of the upmd code in November 2026: K for bugs, S for security problems. #55 is a GitHub issue of upmd.

| ID | Problem in upmd | See |
|---|---|---|
| #55 | On Wayland, in-app copy says `Copied` but copies nothing. Its clipboard library, arboard, is built without the `wayland-data-control` feature. | 1, 6.4 |
| K1 | A `---` or `+++` metadata block that does not start at byte 0 ends the parse. All later blocks are lost, with no error. A file with a blank line before its frontmatter has no blocks. | 5.1 |
| K5 | `Enter` on a running block starts a second run under the same block ID. The old process keeps sending output under that ID, and its end can end the new run. | 8.1, 13 |
| K11 | The state parser reads shell syntax. It gets bash `$'…'` values, escaped `\$`, zsh `'\''` quoting and fish multi-line values wrong. After a zsh quote, it also drops the remaining variables. | 9.2 |
| K12 | `unset` has no effect on later blocks: the state merge only adds variables. | 9.3 |
| K18 | A UTF-8 character that is split across two 32 KiB reads of the PTY becomes U+FFFD. | 6.3, 12.1 |
| S1 | In CLI mode, upmd writes the text of the file to the terminal with its control characters. A runbook can then hide part of a command with `ESC[2K` and a carriage return, or write to the clipboard with OSC 52. | 6.5 |
| S3 | A newline in the name of the working directory can inject variables. The state file holds the raw `pwd` output, one entry per line. A directory whose name holds a newline and then `NAME=value` sets that variable. | 9.2 |
| S4 | Other local users can read the workspace: the directory has mode 0755 and the scripts 0644. upmd never deletes it, so secrets in block code stay in `/tmp` after exit. | 8.3 |
| S6 | If the working directory no longer exists, the next run starts in `$HOME`, with no warning. After a block deletes its own directory, a following `rm -rf build/*` runs in `$HOME`. | 8.2 |
| S9 | About 20,000 nested `>` overflow the stack. upmd aborts and leaves the terminal in raw mode: a stack overflow runs no panic hook. | 5.3 |

## 19. Decided

This spec is the source of truth. A change of decision updates it in the same commit, with a row here.

| Decision | Date |
|---|---|
| stepbook is a new tool, not a fork of upmd. It keeps only what the runbook skill uses | 2026-11-09 |
| The spec stands alone: section 18 explains each upmd issue that it cites | 2026-11-09 |
| Control characters, deep nesting and a UTF-8 character split across two reads get rules: 5.3, 6.3, 6.5, 12.1 | 2026-11-09 |
| The build order has five milestones (16). Input mode comes before the log and reload, because runbooks that ask for a password need it | 2026-11-09 |
| New dependencies: `signal-hook`, and for tests only `insta`, `tempfile` and `proptest` | 2026-11-09 |
| The repository is `KamalF/stepbook` on GitHub, with the MIT licence and CI on GitHub Actions | 2026-11-09 |
| stepbook installs with mise from the GitHub repository, and from releases on crates.io | 2026-11-09 |
| New dependency: `unicode-width`, for the word wrap. ratatui already depends on it | 2026-11-09 |
| `unicode-width` is dropped. stepbook measures each grapheme with ratatui's `CellWidth`, as the ratatui buffer does: a sum of character widths gets emoji with a variation selector wrong | 2026-11-09 |
| stepbook parses its arguments with its own code, not with clap: it has four options | 2026-11-09 |
| Bidi controls are escaped as control characters are (6.5) | 2026-11-09 |
| A file that could make more than 200,000 table cells shows its tables as text (5.1): pulldown-cmark pads short rows, and 1 MB of crafted tables took gigabytes | 2026-11-09 |
| A line of FILE must nest at most 1,000 levels (4), counting quote and list markers and indent: pulldown-cmark rescans every open container on each line, and 600 KB of deep quotes and lazy lines took 30 s. List items stay open through indent, so indent counts. At 1,000 levels, 1 MB takes 1 s | 2026-11-09 |
| FILE must be a regular file of at most 1 MB (4): a link to /dev/zero filled the memory, and the view lays out the whole file on each frame. The largest runbook has 22 KB | 2026-11-09 |
| The minimum terminal size is 40 columns and 8 rows (6.1), at Kamal's choice: at 6 columns, 1 MB of tabs in code took 2.5 GB per frame | 2026-11-09 |
| A blank line always follows a code block (5.3): in a tight list item, text under a block could draw a gutter with ▌ and pass for code that does not run | 2026-11-09 |
| Warnings never quote free text of the info string, and at most 20 characters of a key (5.2): a value of spaces and "cd /tmp/x" made a wrapped info line that passed for code | 2026-11-09 |
| Autowrap is off while stepbook runs (11): a line of prose that a terminal draws wider than ratatui counts wrapped into the rows below, and scrolled the screen | 2026-11-09 |
| In code, a grapheme of several code points shows escaped after its first character (6.5): terminals count their columns otherwise than ratatui. This replaces the rule of 2026-11-09 for graphemes wider than 2 columns | 2026-11-09 |
| Code also escapes the format characters that join the next character, U+2D7F, U+FFF9 to U+FFFB and U+13430 to U+1343F (6.5): a terminal may give them a column that ratatui does not count | 2026-11-09 |
| Code also escapes the spaces other than U+0020 (6.5), at Kamal's choice: bash reads them as part of a word | 2026-11-09 |
| Code also escapes the default ignorable code points and the graphemes of width 0; prose keeps them (6.5). The warning becomes `hidden characters in code` | 2026-11-09 |
| Highlighting stops at lines longer than 1,000 bytes and after 32 KB of code (5.3): syntect takes 10 s on a line of 80 KB. The largest runbook has 16 KB of bash, with lines of at most 386 bytes | 2026-11-09 |
| Highlighting also stops past 4,000,000 for the sum of the squares of the line lengths (5.3): syntect took 10 s on 32 lines of 1,000 spaces. The runbooks reach 2,000,000 | 2026-11-09 |
