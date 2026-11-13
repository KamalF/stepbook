# Changelog

## 0.1.0 - 2026-11-13

The first release.

### Added

- A terminal UI that shows a Markdown runbook and runs its `bash` and `sh`
  blocks one at a time, with the output of each block under its code. Each
  run has a terminal of its own, and several blocks can run at the same time.
- Exported variables and the working directory carry over from one block to
  the next.
- Input mode (`i`) sends the keys to a running program, such as a password
  prompt. The full-screen view (`o`) shows the terminal of a block at full
  size, for programs such as `less`.
- When the file changes on disk, stepbook reloads it. A block keeps its output
  if its name stays the same, or, for an unnamed block, if its code did not
  change. A named block whose code changed after its run shows
  `changed since run`.
- A log with one section per run, between `=== START <name>` and
  `=== END <name> rc=<N>` lines, which a script can follow.
- The runbooks of upmd open unchanged: stepbook reads their `[name:…]`
  attributes. A block that starts with an `exec > >(tee -a …)` logging header
  writes no second section to the log.
- A control character of the file or of the output never reaches the
  terminal raw. stepbook shows a control character of the file as vim does,
  such as `^[` for ESC.

### Release

- Static binaries for x86_64 and aarch64 Linux, built with musl, and stripped
  of their symbols.
- Each archive has an attestation of its build, which `gh attestation verify`
  checks. It carries the licence texts of the Rust crates in
  `THIRD-PARTY-LICENSES-RUST.txt`.
