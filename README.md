# xlurm

A minimal **Linux single-host, multi-user** GPU / Huawei Ascend scheduler. Written in Rust, it runs tasks directly through process executors without tmux. One shared scheduler allocates whole-machine devices, and each task runs with the submitting user's identity.

中文文档：[README_zh.md](README_zh.md)

Install all commands with one command from the repository root:

```bash
cargo install --path . --locked
```

This installs `xlurm`, `xrun`, `xbatch`, `xqueue`, `xcancel`, and `xinfo` to `~/.cargo/bin` (which must be in `PATH`). There is no need to specify each `--bin` or add `--bins`. `--locked` installs dependencies according to the repository lock file and may be omitted; installation uses a release build by default.

On a shared server, an administrator installs the binaries system-wide and starts the shared scheduler:

```bash
# Build all binaries and install them to /usr/local/bin using sudo
./install.sh
sudo xlurm start

# Run as a regular user, without sudo
xinfo                                    # Show devices
xrun -g 1 python train.py                 # Queue, run, stream logs, and return the task exit code
xbatch -g 2 train.sh                     # Submit a bash script in the background and return a job ID
xqueue                                   # Show queued and running tasks
xcancel 3                                # Cancel a task and its process group
```

`install.sh` requires Cargo and `sudo`. It builds with `--release --locked`, then installs all six binaries as root-owned files with mode 0755. It does not start or restart the scheduler.

## Commands

| Command | Purpose |
| --- | --- |
| `xrun [OPTIONS] COMMAND [ARGS...]` | Wait for a task in the foreground, forward stdout/stderr, and cancel on Ctrl-C |
| `xbatch [OPTIONS] SCRIPT [ARGS...]` | Snapshot and submit a script without waiting for resources |
| `xqueue [JOB_ID]` | Show your latest 100 active tasks with wait/run times; with an ID, show detailed results for your task |
| `xcancel JOB_ID` | Cancel a task and its process group; equivalent to `xqueue --cancel JOB_ID` |
| `xinfo` | Show devices, external usage, and allocations |
| `sudo xlurm clean` | Remove all logs while the scheduler is stopped and no task is running |

There are only four task options: `-g/--gpus N` (also `--devices`, default `1`), `--device nvidia|ascend`, `-n/--name NAME`, and `-t/--time-limit SECONDS`. `-g` applies to both device types; `-g 0` submits a CPU task.

```bash
xrun --device ascend -g 2 python train_npu.py
xrun --device nvidia -g 1 -t 3600 python train.py --epochs 10
xrun -g 0 bash -c 'echo hello; exit 7'     # xrun also returns 7
xbatch --device ascend -g 2 train.sh --epochs 10
xbatch -g 1 --wrap 'python prepare.py && python train.py'
xqueue --all                            # Include completed tasks
xqueue 3                                # Show your task's status and exit code
xqueue 3 --log                          # Read your task's output
xqueue --all --json
xinfo --json
```

`xqueue` displays at most the latest 100 matching tasks, including with `--all`; root sees tasks from every user. Durations use `HH:MM:SS`, or `D-HH:MM:SS` after 24 hours. A pending job's wait time and a running job's run time continue increasing. Jobs cancelled before starting show `-` for run time.

Options for `xrun` go before the command name. Starting at the command name, all later arguments are passed to the task, including options such as `--help` and `--gpus`. The `--` separator is optional, so the older form `xrun -g 1 -- python train.py` remains supported.

Arguments are passed directly to the process; they are not assembled into a shell command. To use pipes, redirection, or variable expansion, explicitly use `bash -c` or `xbatch --wrap`. Batch scripts always run under bash and receive arguments unchanged; directives such as `#SBATCH` are not parsed. Tasks retain the working directory and environment from submission, so you can activate conda or a virtual environment before submitting.

`xrun` (or `xlurm run`) prints status messages to stderr with a UTC timestamp and Job ID. After waiting for more than one second it prints `Waiting for resources...` once; when execution starts it prints `Task started.` once. Tasks that start immediately also receive the start message. Task logs continue to be written normally, while execution errors such as failure to start a task are reported to stderr. After a background submission, `xbatch` only returns the task ID for later inspection or cancellation.

```text
[xlurm] 2026-09-20 12:34:56 UTC Job 3: Waiting for resources...
[xlurm] 2026-09-20 12:35:10 UTC Job 3: Task started.
```

`xrun` follows foreground logs but does not provide an interactive terminal; task stdin is `/dev/null`, and stdout/stderr are stored together. Use `python -u` when Python must flush logs immediately.

## Scheduler

```bash
sudo xlurm start                             # Start in the background and discover both device types
sudo xlurm start --backend ascend            # Manage only Ascend
sudo xlurm start --backend none              # CPU mode; no device driver required
sudo xlurm daemon --backend auto --max-running 32  # Run in the foreground
sudo xlurm stop                              # Stop scheduling; already-started tasks continue
sudo xlurm restart                           # Stop and start; refuse if any task is running
sudo xlurm clean                             # Remove logs after all tasks have finished
```

`xlurm clean` is an offline administrator action. It refuses to run if the scheduler is active or if persisted state contains a `RUNNING` task. After both checks pass, it removes every task `.log` file and `daemon.log` while preserving queued tasks, task history, results, and other spool files. If a worker finishes while the scheduler is stopped, restart the scheduler once so it can collect the result before stopping it and running `clean`. New tasks and a subsequent background start create new log files.

`start` does not modify an already-running scheduler; stop it first to change the backend or concurrency limit. A subsequent start takes over running tasks, reads results completed while it was offline, and continues queued tasks. After a machine reboot or unexpected worker disappearance, running tasks without results are marked `FAILED` and are not rerun automatically.

`restart` is equivalent to `stop` followed by `start` and accepts the same backend and concurrency options as `start`. Unlike `stop`, it refuses to stop the scheduler while any task is `RUNNING`; queued tasks do not prevent a restart.

The default shared state directory is `/var/lib/xlurm`. All users automatically connect to its Unix socket without starting their own service. Set `XLURM_HOME` to another short absolute path owned by the administrator; all clients and the scheduler must use the same path. There is no configuration file and no network listening port.

The state directory is root-owned with mode 0755, and the socket has mode 0666 so local users can connect. `jobs/` has mode 0700, while state, environment, and logs have mode 0600. **Users can connect to the socket but cannot modify queue files or directly read another user's logs.** All log access is authorized by the server.

```text
$XLURM_HOME/
  xlurm.sock       Local Unix socket
  daemon.lock      Single-scheduler lock
  daemon.log       Diagnostics such as device discovery and startup failures
  state.json       Queue and history
  jobs/            Per-task description, logs, lock, and exit result
```

Completed tasks (successful, failed, cancelled, or timed out) are retained for **7 days** from their completion time. The scheduler cleans them at startup and once per minute while running, deleting expired task logs, descriptions, results, cancellation markers, locks, and temporary files, and removing their records from `state.json`; job IDs are never reused. Queued and running tasks are unaffected.

After cleanup, `xqueue --all` no longer lists the task, and querying its ID or reading its logs returns `job not found`. Export logs that must be retained before cleanup. This policy only removes old tasks; it does not limit the log size of running tasks or clean the task working directory or `daemon.log`.

## Execution and Resources

```text
xrun / xbatch / xqueue / xcancel / xinfo
               │ Unix socket
       SO_PEERCRED authentication → single-threaded Scheduler
               │ Executor: start / poll / cancel
          ProcessExecutor
               │
       Dedicated worker → drop privileges to submitter → task process group
                    Logs + atomic result file
```

- The scheduler scans tasks in submission order and starts tasks when resources are available; a later small task may run while a larger task waits. At most 32 tasks run at once, configurable with `--max-running`.
- Each device slot is allocated exclusively, and a task uses only one vendor's device pool. Automatic selection tries NVIDIA first, then Ascend. Multi-chip Ascend cards are allocated by individual compute chip.
- NVIDIA devices are discovered and monitored with `nvidia-smi`; `CUDA_VISIBLE_DEVICES` is set by GPU UUID to avoid differences between CUDA and management-tool index order.
- Ascend devices use `npu-smi info -m` to parse mappings, and `ASCEND_RT_VISIBLE_DEVICES` is set by logical ID. Both `Chip Logic ID` and the `Chip Phy-ID` column used by Ascend950PR are supported; physical card numbers are not mistaken for logical IDs on multi-chip cards. Physical card and chip numbers are used only for driver queries.
- External compute processes reported by the driver are checked every two seconds. Occupied devices are not allocated; query failures are shown as `unknown` and pause allocation. Each driver call waits at most three seconds.
- Workers hold inherited file locks, allowing the scheduler to take over tasks after a restart without relying on bare PIDs to determine liveness. Exit results are written atomically.
- Cancellation first sends SIGTERM to the process group, then sends SIGKILL if it is still running after one second. When the main task process exits, the same group is cleaned up, orphan processes are reaped, and devices are released.

## Multi-user Permissions

| Operation | Regular user | Root administrator |
| --- | --- | --- |
| `xinfo`, queue summary | Device info is host-wide; queue contains only own tasks | Can see the whole host |
| Submit a task | Runs with the user's UID/GID | Runs as root |
| Task details, command, environment, logs | Own tasks only | All tasks |
| Cancel a task | Own tasks only | All tasks |
| Start/stop the shared scheduler | Not allowed | Allowed |

Identity comes from the kernel's `SO_PEERCRED`; clients cannot choose task ownership through JSON or the `USER` environment variable. Before execution, the scheduler re-resolves the local account and supplementary groups, sets supplementary groups plus real/effective/saved GID and UID, then enters the user's working directory and executes the command. Files owned by the user are accessed with that user's permissions, and groups required by Ascend/NVIDIA drivers are preserved.

Tasks set `no_new_privs`, so a task cannot rely on sudo or setuid programs for privilege escalation. Queue summaries contain only the task name, owner, resources, and status; the daemon also restricts non-root callers to their own tasks. Commands, scripts, and environments are never included. Scheduling remains a simple submission-order allocation attempt and does not add quotas, priorities, or billing.

This project provides multi-user identity and control permissions. GPU/NPU access is described through visible-device environment variables and is not yet enforced with cgroups or device-node restrictions. Programs that bypass the scheduler can still compete for devices. MIG, memory partitioning, and intentionally detached background services are unsupported. CPU tasks set both device visibility variables to `-1`.

## Development

There are only five runtime dependencies: `anyhow`, `clap`, `libc`, `serde`, and `serde_json`. The project does not use an async runtime, database, HTTP, web UI, or tmux.

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings

# Integration test with two actual system accounts: requires root,
# creates no accounts, and uses no real devices
cargo build --bins --locked
sudo python3 tests/multiuser.py
```

End-to-end tests use simulated drivers to cover NVIDIA and Ascend, exclusive allocation, external usage and query failures, foreground exit codes, script snapshots, cancellation, timeouts, and scheduler crash takeover. Permission tests cover owner spoofing, cross-user querying/log access/cancellation, administrator permissions, and queue privacy. Ordinary `cargo test` does not require root.

`tests/multiuser.py` uses the existing `nobody` and `daemon` accounts to verify real UID/GID/supplementary-group switching, output-file ownership, private queues, and unauthorized access rejection. It uses temporary directories and CPU tasks without modifying account configuration. For development, start a non-root test instance accessible only to the current user with `XLURM_HOME=/tmp/my-xlurm xlurm daemon --backend none`.

Interface semantics reference [CUDA_VISIBLE_DEVICES](https://docs.nvidia.com/deploy/topics/topic_5_2_1.html), [Ascend visible devices](https://www.hiascend.com/document/detail/en/canncommercial/850/maintenref/envvar/envref_07_0028.html), and [Linux Unix socket credentials](https://man7.org/linux/man-pages/man7/unix.7.html).

This project was strongly inspired by [gflow](https://github.com/AndPuQing/gflow).
