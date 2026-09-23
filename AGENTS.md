# AGENTS.md

## Project Overview

`xlurm` is a small Linux-only, multi-user scheduler for NVIDIA GPUs and Ascend
NPUs. It is written in Rust and communicates over a local Unix socket. Keep the
implementation deliberately small: there is no async runtime, database, HTTP
service, web UI, or tmux integration.

The main binaries are `xlurm`, `xrun`, `xbatch`, `xqueue`, `xcancel`, and
`xinfo`. The wrappers in `src/bin/` all enter the shared CLI implementation.

## Repository Map

- `src/cli.rs`: command-line parsing, client behavior, and human-readable output.
- `src/model.rs`: persisted job data and the client/daemon JSON protocol.
- `src/daemon.rs`: Unix socket server, peer authentication, and request handling.
- `src/scheduler.rs`: queue state, resource allocation, cleanup, and recovery.
- `src/executor.rs`: worker processes, privilege dropping, process groups, and
  result collection.
- `src/device.rs`: NVIDIA and Ascend discovery and occupancy checks.
- `src/auth.rs`: Unix peer credentials, account lookup, and authorization.
- `src/storage.rs`: state paths, permissions, locking, and atomic JSON writes.
- `tests/e2e.rs`: end-to-end behavior using simulated accelerator drivers.
- `tests/multiuser.py`: root-only integration coverage with real local accounts.
- `README.md` and `README_zh.md`: English and Chinese user documentation.

## Development Commands

Run these checks for normal changes:

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Ordinary tests do not require root or accelerator hardware. The executables in
`tests/fixtures/` simulate NVIDIA and Ascend management tools.

For changes to authentication, privilege dropping, file ownership, socket
permissions, or cross-user access, also run the following when root access and
the existing `nobody` and `daemon` accounts are available:

```bash
cargo build --bins --locked
sudo python3 tests/multiuser.py
```

Do not create system accounts or depend on real accelerator devices in tests.

## Required Invariants

- Treat `SO_PEERCRED` as the source of client identity. Never accept ownership
  from request JSON or environment variables.
- Queue summaries may be host-wide, but commands, scripts, environments, job
  details, and logs remain owner-only unless the caller is root.
- Keep daemon state and job spool data private. Preserve the existing directory,
  file, and socket permission model in `storage.rs` and `daemon.rs`.
- Write persistent state and worker results atomically. A crash must not leave a
  partially written JSON file that replaces valid state.
- Persist a job as running before spawning it. Scheduler restart and worker
  adoption must never execute the same job twice.
- Allocate each accelerator exclusively, never mix vendors within one job, and
  fail closed when device occupancy cannot be determined.
- Use NVIDIA UUIDs and Ascend logical IDs for visibility. Physical Ascend card
  and chip identifiers are only for management queries.
- Run each task under its submitting account, including supplementary groups,
  and preserve `no_new_privs`.
- Manage tasks as process groups. Cancellation and normal completion must clean
  up descendants before releasing allocated devices.
- A scheduler restart may adopt running work. Missing results after a worker is
  known to be gone must fail the job rather than rerun it.

## Change Guidelines

- Prefer the standard library and the existing five runtime dependencies. Add a
  dependency only when it removes substantial complexity and has a clear reason.
- Keep blocking, single-threaded scheduler behavior unless a change explicitly
  requires a different architecture.
- Consider `src/model.rs` both a disk schema and a wire protocol. When changing
  serialized types, account for existing `state.json` files and client/daemon
  version skew. Use Serde defaults or migrations where compatibility is needed.
- Keep public queue data intentionally narrow. Review additions for information
  disclosure before extending `JobSummary`.
- Preserve direct argument passing. Do not construct shell command strings for
  `xrun`; shell behavior belongs behind an explicit `bash -c` or `xbatch --wrap`.
- Use saturating time arithmetic for wall-clock timestamps because system time
  can move backwards.
- Keep errors actionable and attach context at filesystem, process, and protocol
  boundaries.
- Update both README files when user-facing commands, output, behavior, security,
  installation, or operational requirements change.

## Testing Expectations

- Put focused pure-logic tests beside the module they cover.
- Extend `tests/e2e.rs` for CLI output, lifecycle behavior, persistence, restart,
  cancellation, timeouts, allocation, or device probing.
- Extend `tests/multiuser.py` for authorization and real UID/GID/group behavior.
- Avoid timing-only assertions. Poll observable state with a deadline, following
  the existing harness patterns.
- Verify both successful behavior and the relevant failure or privacy boundary.
- Keep test commands and fixtures safe for parallel execution.

## Style

- Follow `rustfmt`; keep comments focused on invariants or non-obvious races.
- Avoid unrelated refactors in feature and bug-fix changes.

## Commit Rules

- Create a commit only when the user explicitly asks for one. Completing an
  implementation does not by itself authorize committing or pushing it.
- Inspect `git status` and the relevant diff before staging. Existing unrelated
  changes belong to the user; do not discard, rewrite, or include them.
- Stage files explicitly. Do not use broad staging commands such as `git add .`
  when the worktree contains changes outside the current task.
- Keep each commit focused on one coherent change. Split unrelated code,
  documentation, or cleanup into separate commits when they can stand alone.
- Run the checks appropriate to the change before committing. At minimum run
  `cargo fmt --check` and the relevant tests; use the full command set above for
  changes with broad impact.
- Use concise Conventional Commit subjects consistent with repository history:
  `feat: ...`, `fix: ...`, `docs: ...`, `test: ...`, `refactor: ...`, or
  `chore: ...`. Write the subject in imperative form, without a trailing period.
- When a commit relates to GitHub issues, append their numbers to the subject as
  `(#12)` or `(#12, #34)` for multiple issues.
- Add a commit body when the reason, security consequence, migration behavior,
  or compatibility tradeoff is not clear from the subject and diff.
- Do not commit build artifacts from `target/`. Change `Cargo.lock` only when the
  dependency graph intentionally changes.
- Do not amend, rebase, force-push, or otherwise rewrite existing history unless
  the user explicitly requests that operation.
- After committing, report the commit hash and subject, then check whether any
  tracked or untracked changes remain. Do not push unless explicitly requested.
