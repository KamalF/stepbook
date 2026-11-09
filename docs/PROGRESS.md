# stepbook progress

Where the build stands against the build order in `docs/SPEC.md`, section 16.

## How we work

Claude works on its own. It writes the code, the tests and the commit messages, and commits. Kamal makes the design and product choices, and approves each new dependency. Claude makes the obvious choices itself and records them in the spec or here.

- One branch per milestone, cut from `main` after the merge of the one before.
- Small commits, with conventional prefixes: `feat:`, `fix:`, `docs:`, `refactor:`, `tests:`, `chore:`.
- Each commit passes `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and the tests. The spec and this file change in the same commit as the code that they describe.
- A milestone ends with `cargo mutants`, then the review loop, until a round finds nothing to fix. Claude checks the TUI in tmux. Kamal's manual checklist keeps only what tmux cannot show: copy and paste in herdr and Ghostty, and the look in his terminal. Then Claude merges into `main`, and Kamal pushes.
- `docs/runbook.md`, excluded from git, logs each command that changes the setup, such as `cargo add` or `mise use`.

## Milestones

| # | Milestone | Status |
|---|---|---|
| 1 | Document view | In progress, on `m1-document-view` |
| 2 | Running blocks | — |
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

Then `cargo mutants`, the check in tmux, and the review loop.

## Next

1. Commit 13 of milestone 1: the layout.

## Open points

- Before the first push, Kamal creates `KamalF/stepbook` on GitHub and adds the remote. The sandbox cannot write `.git/config`.
- Kamal runs `cargo publish`: it needs his crates.io token. The name `stepbook` was free on crates.io on 2026-11-09. mdame held its name early with a 0.0.1 release; stepbook can do the same after milestone 1.
- Claude drives stepbook in tmux from the sandbox with one script per check: the script starts tmux, sends the keys and captures the pane. The sandbox stops the tmux server when a command ends. Checked on 2026-11-09.
