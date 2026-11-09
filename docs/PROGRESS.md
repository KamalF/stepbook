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
| 1 | Document view | Next |
| 2 | Running blocks | — |
| 3 | Input and full-screen view | — |
| 4 | Log and reload | — |
| 5 | Migration | — |

## Next

1. Milestone 1: its plan of commits in this file, then its branch `m1-document-view`.

Kamal signed off the build order and the new dependencies on 2026-11-09.

## Open points

- Before the first push, Kamal creates `KamalF/stepbook` on GitHub and adds the remote. The sandbox cannot write `.git/config`.
- Kamal runs `cargo publish`: it needs his crates.io token. The name `stepbook` was free on crates.io on 2026-11-09. mdame held its name early with a 0.0.1 release; stepbook can do the same after milestone 1.
- To check in milestone 1: how Claude drives stepbook in tmux from the sandbox. If a sandboxed command stops its child processes when it ends, one script must start tmux, send the keys and capture the pane.
