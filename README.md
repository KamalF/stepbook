# stepbook

stepbook shows a Markdown runbook in a terminal UI, and runs its bash blocks
one by one, as you start them, with the output of each block under its code.

A runbook is a list of steps: a line that says why, then a bash block. You
select a block and press Enter to run it. `export` and `cd` carry over from
one block to the next, as in a terminal. When the file changes on disk,
stepbook reloads it. Each run goes to a log, so that the coding agent that
wrote the runbook can follow your runs.

I built stepbook to replace [upmd](https://github.com/rezigned/upmd) for the
runbooks that Claude Code writes for me: the commands that its sandbox cannot
run, such as `git push`, or a `kubectl` with my credentials. stepbook keeps
only what those runbooks use, and opens upmd runbooks unchanged.

```text
┌ blocks ───┬ clean-logs.md ───────────────────────────────────────────────────────┐
│ ✔ setup   │ Clean the old logs                                                   │
│▶✔ list    │ ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ │
│   clean   │                                                                      │
│           │ Why: make a folder with a few files.                                 │
│           │                                                                      │
│           │ ▐ setup  ✔ rc=0 0:00                                                 │
│           │ ▐ export DEMO=$(mktemp -d)                                           │
│           │ ▐ cd "$DEMO" && touch app.log db.log notes.txt                       │
│           │                                                                      │
│           │ Why: list the logs that the next step removes.                       │
│           │                                                                      │
│           │ ▐ list  ✔ rc=0 0:00                                                  │
│           │ ▐ ls *.log                                                           │
│           │ ╎ app.log  db.log                                                    │
│           │                                                                      │
│           │ Why: Destructive: remove them.                                       │
│           │                                                                      │
│           │ ▐ clean                                                              │
│           │ ▐ rm -v *.log                                                        │
└───────────┴──────────────────────────────────────────────────────────────────────┘
 NORMAL   ↑↓ move  ↵ run  o output  q quit
```

0.1.0 is the first release. stepbook runs on Linux, on x86_64 and aarch64.

## Install

With mise, from the release archives:

```sh
mise use -g github:KamalF/stepbook
```

With Cargo, from the source, with Rust 1.92 or later:

```sh
cargo install --locked stepbook
```

You can also download the archive for your system from the
[latest release](https://github.com/KamalF/stepbook/releases/latest), and put
`stepbook` in a folder of your `PATH`, such as `~/.local/bin`. The binaries
are static, so they run on any distribution.

The archive of each system has an attestation: a record, signed by GitHub,
that the release workflow of this repository built it. To check an archive:

```sh
gh attestation verify stepbook-x86_64-unknown-linux-musl.tar.xz \
  --repo KamalF/stepbook \
  --signer-workflow KamalF/stepbook/.github/workflows/release.yml
```

In fish, an abbreviation gives a shorter command, which expands to `stepbook`
as you type. To keep it, put it in `~/.config/fish/config.fish`:

```fish
abbr -a stbk stepbook
```

## Usage

```text
stepbook [--log PATH | --no-log] FILE
```

stepbook opens the runbook `FILE`, a Markdown file of at most 1 MB. By
default, it writes the log to `FILE` with its extension replaced by `.log`.
`--log PATH` writes it to `PATH`, and `--no-log` writes no log.

A block is a fenced code block whose language is `bash` or `sh`. Both run with
bash. A name, in the syntax of upmd, labels the block in the sidebar and in
the log:

````markdown
```bash [name:restart-api]
systemctl --user restart api
```
````

A block without a name is `#<n>`, where n is its place among the blocks of the
file, from 1. Other code blocks show dimmed and do not run.

- Each run has a terminal of its own. Several blocks can run at the same time.
- The exports and the `cd` of a block carry over to the blocks that start
  after it ends. A background job, such as
  `kubectl port-forward … &`, runs until stepbook ends.
- A block gets `STEPBOOK_FILE`, `STEPBOOK_LOG` and `STEPBOOK_BLOCK`: the path
  of the runbook, the path of the log, and the name of the block.
- When the file changes on disk, stepbook reloads it. A block keeps its
  output if its name stays the same, or, for an unnamed block, if its code
  did not change. A named block whose code changed after its run shows `~`
  and `changed since run`. Any other block is a new block, which has not run.
- stepbook shows a control character of the file as vim does, such as `^[`
  for ESC, so that a runbook cannot hide part of a command. The output of a
  program goes through a terminal screen of stepbook, never raw to your
  terminal. Two kinds of terminals can still draw code in another way than
  bash reads it:
  - A terminal that draws characters of ambiguous width, such as `·`, 2
    columns wide. They shift the rest of the line, and a space can vanish.
    The default is 1 column.
  - A terminal that reorders right-to-left text on its own, such as VTE or
    Konsole. Code with Hebrew or Arabic letters can show in another order.
- If blocks are running, stepbook asks before it stops them and quits.

### Keys

| Key | Action |
|---|---|
| `↓` `j` / `↑` `k` | Select the next / previous block |
| `Home` / `End` | Select the first / last block |
| `PageDown` / `PageUp` | Scroll one page |
| `Enter` | Run the selected block. If the block has run, `y` confirms the new run |
| `i` | Input mode: the keys go to the program of the running block, such as a password prompt |
| `o` | The full-screen view: the terminal of the block at full size, for programs such as `less` |
| `ctrl-o` | Leave input mode or the full-screen view |
| `ctrl-r` | Reload the file now |
| `q`, `ctrl-c` | Quit |

In input mode, every key goes to the program, except `ctrl-o`. After the run,
`j`, `k`, `PageUp` and `PageDown` scroll the full-screen view, and `q` or
`Esc` closes it.

Your terminal does the copy and paste: stepbook never takes the mouse. To
copy output without the sidebar, select it in the full-screen view. In input
mode, a paste goes to the program.

## The log

Each run writes one section to the log:

```text
=== START list 09:37:17Z
app.log  db.log
=== END list rc=0 09:37:17Z
```

- The times are in UTC. The `END` line gives the exit code of the block.
- stepbook removes the escape sequences of the output, and a progress bar logs
  only its last state. The output of a full-screen program, such as `less`,
  stays out of the log, and one line marks it.
- The lines of two runs never mix. If a run ends while another one writes to
  the log, the run that ends writes its whole section at once. The section of
  the other run then goes on after a `=== CONTINUE <name>` line.
- stepbook creates the log with mode 0600. It does not start if the log is a
  link, or a file of another user: a runbook from someone else could come
  with a log that links to your `~/.bashrc`.

A script, or a coding agent, can wait for a block with its `END` line. This
script prints each `END` line that the log gets after the script starts, and
exits after the `END` line of the block that you name. The `END` lines of
earlier runs do not count:

```bash
#!/usr/bin/env bash
# Usage: watch.sh LOG BLOCK
log=$1
# A dot in a block name must not match any character.
block=${2//./[.]}
seen=$(grep -ac '^=== END ' "$log" 2>/dev/null)
while :; do
  new=$(grep -a '^=== END ' "$log" 2>/dev/null | tail -n +$((seen + 1)))
  if [ -n "$new" ]; then
    printf '%s\n' "$new"
    seen=$((seen + $(printf '%s\n' "$new" | wc -l)))
    printf '%s\n' "$new" | grep -q "^=== END $block rc=" && exit 0
  fi
  sleep 1
done
```

My runbook skill for Claude Code runs a script like this one in the
background, and reads the section of each block that ends.

## How stepbook was built

I made the design and product choices, and Claude Code wrote all the code, the
tests and the commit messages, on its own. It works from a spec,
`docs/SPEC.md`, which changes in the same commit as each decision.
`docs/PROGRESS.md` tells how each milestone went.

stepbook was built in five milestones. At the end of each one, fresh Claude
Code sessions reviewed it, in rounds, until a round found nothing to fix.
In a milestone that changed the code, `cargo mutants` ran before and after
the review: it changes the code to find the rules that no test checks. The
tests run real bash in a PTY, with no mocks. Claude checked each feature in
tmux, and I checked in my own terminal what tmux cannot show, such as copy
and paste.

## License

MIT. See `LICENSE`. Each release archive carries the licence texts of the Rust
crates in the binary, in `THIRD-PARTY-LICENSES-RUST.txt`.
