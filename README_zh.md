# xlurm

极简的 **Linux 单机、多用户** GPU / 华为 Ascend 调度器。Rust 编写，直接用进程 Executor 执行任务，不依赖 tmux。一个共享调度器统一分配整机设备，每个任务以提交用户自己的身份执行。

在仓库目录中，一条命令即可安装全部命令：

```bash
cargo install --path . --locked
```

默认安装 `xlurm`、`xrun`、`xbatch`、`xqueue`、`xcancel`、`xinfo` 到 `~/.cargo/bin`（需在 `PATH` 中），无需逐个指定 `--bin` 或加 `--bins`。`--locked` 用于按仓库的锁文件安装依赖，也可以省略；安装默认使用 release 构建。

多人共用的服务器由管理员安装到系统目录并启动共享调度器：

```bash
# 构建全部二进制，并通过 sudo 安装到 /usr/local/bin
./install.sh
sudo xlurm start

# 以下由普通用户执行，不使用 sudo
xinfo                                    # 看设备
xrun -g 1 python train.py                 # 排队、运行、输出日志，返回任务退出码
xbatch -g 2 train.sh                     # 后台提交 bash 脚本，返回 job ID
xqueue                                   # 看排队和运行中的任务
xcancel 3                                # 取消任务及其进程组
```

`install.sh` 需要 Cargo 和 `sudo`。脚本使用 `--release --locked` 构建，将 6 个二进制安装为 root 所有、权限 0755；它不会启动或重启调度器。已在运行的调度器及其后续启动的 worker 会继续使用旧版可执行文件，直到调度器重启。

## 命令

| 命令 | 用途 |
| --- | --- |
| `xrun [选项] COMMAND [ARGS...]` | 前台等待任务，转发 stdout/stderr，Ctrl-C 取消任务 |
| `xbatch [选项] SCRIPT [ARGS...]` | 保存脚本快照后提交，不必等资源空闲 |
| `xqueue [JOB_ID]` | 查看自己最新 100 条活动任务及启动时间、等待/执行时长；传 ID 查看自己任务的详细结果 |
| `xcancel JOB_ID` | 取消任务及其进程组，等价于 `xqueue --cancel JOB_ID` |
| `xinfo` | 显示设备、外部占用、分配情况 |
| `sudo xlurm clean` | 调度器已停止且没有运行中任务时删除全部日志 |

任务选项只有四个：`-g/--gpus N`（也可写 `--devices`，默认 1）、`--device nvidia|ascend`、`-n/--name NAME`、`-t/--time-limit SECONDS`。`-g` 对两类卡均适用；`-g 0` 提交 CPU 任务。

```bash
xrun --device ascend -g 2 python train_npu.py
xrun --device nvidia -g 1 -t 3600 python train.py --epochs 10
xrun -g 0 bash -c 'echo hello; exit 7'     # xrun 也返回 7
xbatch --device ascend -g 2 train.sh --epochs 10
xbatch -g 1 --wrap 'python prepare.py && python train.py'
xqueue --all                            # 包含已结束任务
xqueue 3                                # 查看自己任务的状态、退出码
xqueue 3 --log                          # 读取自己任务的输出
xqueue --all --json
xinfo --json
```

`xqueue` 最多显示符合条件的最新 100 条任务，使用 `--all` 时也一样；所有用户都能查看全主机队列摘要。`DEVICES` 列对排队任务显示申请的设备类型和数量（例如 `ascend:2 requested`、`auto:1 requested` 或 `cpu`）；任务启动后显示已分配设备，同一厂商的多个 ID 合并为 `ascend:[0,1]` 或 `nvidia:[0,1]`。启动时间使用 UTC+8：默认列表显示 `MM-DD HH:MM:SS`，使用 `--all` 时显示 `YYYY-MM-DD HH:MM:SS`；任务详情保留年份。时长格式为 `HH:MM:SS`，超过 24 小时后为 `D-HH:MM:SS`。排队中任务的等待时间、运行中任务的执行时间会持续增加；尚未启动的任务以 `-` 显示启动时间和执行时长。

`xrun` 的调度选项写在命令名前；从命令名开始，后续参数均传给任务程序，包括 `--help`、`--gpus` 等同名选项。分隔符 `--` 可省略，原来的 `xrun -g 1 -- python train.py` 写法也兼容。

参数直接传给进程，不拼接成 shell 命令；需要管道、重定向、变量展开时显式用 `bash -c` 或 `xbatch --wrap`。批处理脚本统一由 bash 执行，参数原样传入；不解析 `#SBATCH` 等脚本指令。任务保留提交时的工作目录和环境，因此先激活 conda/venv 再提交即可。

`xrun`（或 `xlurm run`）向 stderr 打印带 UTC 时间戳和 Job ID 的状态提示。排队超过 1 秒时打印一次 `Waiting for resources...`；开始运行时打印一次 `Task started.`，立即执行的任务也会打印启动提示。任务日志照常输出，无法启动任务等执行错误仍会报到 stderr。`xbatch` 后台提交后只返回任务 ID，供后续查询或取消。

```text
[xlurm] 2026-09-20 12:34:56 UTC Job 3: Waiting for resources...
[xlurm] 2026-09-20 12:35:10 UTC Job 3: Task started.
```

`xrun` 是前台日志跟随，不提供交互终端，任务 stdin 为 `/dev/null`；stdout/stderr 合并保存。Python 如需立即输出日志，可使用 `python -u`。

## 调度器

```bash
sudo xlurm start                             # 后台启动，自动发现两类卡
sudo xlurm start --backend ascend            # 只管理 Ascend
sudo xlurm start --backend none              # CPU 模式，无需驱动
sudo xlurm daemon --backend auto --max-running 32  # 前台运行
sudo xlurm stop                              # 停止调度，已启动任务继续运行
sudo xlurm restart                           # 停止并启动；有任务运行时拒绝操作
sudo xlurm clean                             # 所有任务结束后删除日志
```

`xlurm clean` 是仅供管理员使用的离线操作。调度器仍在运行，或持久化状态中存在 `RUNNING` 任务时，它都会报错退出。两项检查均通过后，命令删除全部任务 `.log` 文件和 `daemon.log`，但保留排队任务、任务历史、结果和其他 spool 文件。如果 worker 在调度器停止期间结束，需要先重启调度器收集结果，再停止并执行 `clean`。后续新任务及后台调度器重启会重新创建日志文件。

`start` 对已运行的调度器不做修改；更换 backend 或并发上限需要先 stop。再次启动会接管运行中的任务、读取离线期间完成的结果并继续排队任务。机器重启或 worker 异常消失后，无结果的运行任务标记为 `FAILED`，不会自动重跑。

`restart` 等价于依次执行 `stop` 和 `start`，并接受与 `start` 相同的 backend 和并发上限参数。与 `stop` 不同，只要存在 `RUNNING` 任务，它就会报错并保持调度器运行；排队中的任务不影响重启。

默认共享状态目录为 `/var/lib/xlurm`。所有用户自动连接这里的 Unix socket，无需用户各自启动服务。可以通过 `XLURM_HOME` 指定其他由管理员持有的短绝对路径，所有客户端和调度器必须使用同一路径。没有配置文件，也没有网络监听端口。

状态目录为 root 所有、0755，socket 为 0666，供本机用户连接；`jobs/` 为 0700，状态、环境、日志为 0600。**所有用户都能查看全主机的队列摘要，但不能修改队列文件或直接读取其他人的日志**。所有日志访问都经过服务端鉴权。

```text
$XLURM_HOME/
  xlurm.sock       本地 Unix socket
  daemon.lock      单调度器锁
  daemon.log       发现设备、启动失败等诊断
  state.json       队列与历史
  jobs/            每个任务的描述、日志、锁和退出结果
```

已结束任务（成功、失败、取消或超时）从结束时间起保留 **7 天**。调度器启动时和运行中每分钟自动清理一次，删除过期任务的日志、描述、结果、取消标记、锁和临时文件，并从 `state.json` 移除记录；任务 ID 不会复用。排队中、运行中的任务不受影响。

清理后 `xqueue --all` 不再列出该任务，按 ID 查询或读取日志会返回 `job not found`。需要长期保存的日志应在清理前自行导出。此策略只清理旧任务，不限制运行中任务的日志大小，也不清理任务工作目录或 `daemon.log`。

## 执行与资源

```text
xrun / xbatch / xqueue / xcancel / xinfo
               │ Unix socket
       SO_PEERCRED 鉴权 → 单线程 Scheduler
               │ Executor: start / poll / cancel
          ProcessExecutor
               │
       独立 worker → 降权到提交用户 → 任务进程组
                    日志 + 原子结果文件
```

- 调度器按提交顺序扫描，资源足够就启动；大任务等资源时允许后面的小任务先跑。最多同时运行 32 个任务，可用 `--max-running` 调整。
- 每个设备槽独占分配，单个任务只使用一个厂商的设备池。自动选择优先尝试 NVIDIA，再尝试 Ascend。多芯片 Ascend 卡按独立计算芯片分配。
- NVIDIA 用 `nvidia-smi` 发现与监测，按 GPU UUID 设置 `CUDA_VISIBLE_DEVICES`，避免 CUDA 与管理工具索引顺序不同。
- Ascend 用 `npu-smi info -m` 解析映射，按逻辑 ID 设置 `ASCEND_RT_VISIBLE_DEVICES`。兼容 `Chip Logic ID` 和 Ascend950PR 的 `Chip Phy-ID` 列；不会把多芯片卡的物理卡号误当成逻辑号。物理卡号/芯片号仅用于驱动查询。
- 每两秒检查驱动报告的外部计算进程；有占用的设备暂不分配，查询失败显示 `unknown` 并暂停分配。单次驱动调用最多等待 3 秒。
- worker 持有继承的文件锁，调度器重启后通过锁接管任务；不靠裸 PID 判断任务是否存活。退出结果通过原子文件写入。
- 取消先向进程组发 SIGTERM，一秒后仍未退出则发 SIGKILL。任务主进程结束时清理同组后台子进程，并回收孤儿进程，然后释放设备。

## 多用户权限

| 操作 | 普通用户 | root 管理员 |
| --- | --- | --- |
| `xinfo`、队列概要 | 可看全机 | 可看全机 |
| 提交任务 | 以自己的 UID/GID 执行 | 以 root 执行 |
| 任务详情、命令、环境、日志 | 仅自己的任务 | 所有任务 |
| 取消任务 | 仅自己的任务 | 所有任务 |
| 启停共享调度器 | 不允许 | 允许 |

身份来自内核 `SO_PEERCRED`，客户端无法通过 JSON 或 `USER` 环境变量指定任务属主。执行前重新解析本机账户及附加组，依次设置 supplementary groups、real/effective/saved GID 和 UID，再进入用户工作目录并执行命令。用户拥有的文件按该用户权限读写；Ascend/NVIDIA 驱动所需的用户组权限也得以保留。

任务设置 `no_new_privs`，因此任务内不能依赖 sudo/setuid 程序提权。所有用户都能查看全主机队列概要，其中只含任务名、属主、资源和状态；命令、脚本和环境绝不会出现在概要中。任务详情、日志和取消操作仍仅限任务所有者或 root。调度仍是简单的按提交顺序尝试分配，不增加用户配额、优先级或计费系统。

这里提供的是多用户身份与控制权限；GPU/NPU 使用范围通过可见设备环境变量约定，尚未通过 cgroup 强制限制设备节点访问。绕过调度器直接用卡的程序仍可能竞争设备。不支持 MIG、显存切分或故意脱离进程组的后台服务。CPU 任务会将两类设备可见变量设为 `-1`。

## 开发

只使用 5 个运行时依赖：`anyhow`、`clap`、`libc`、`serde`、`serde_json`。不引入异步运行时、数据库、HTTP、Web UI 或 tmux。

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings

# 真正切换两个系统账户的集成测试：需要 root，不创建账户，不使用真实卡
cargo build --bins --locked
sudo python3 tests/multiuser.py
```

端到端测试通过模拟驱动覆盖 NVIDIA 和 Ascend，验证独占分配、外部占用/查询失败、前台退出码、脚本快照、取消、超时和调度器崩溃接管；权限测试覆盖属主伪造、跨用户查询/读日志/取消、管理员权限和队列隐私。普通 `cargo test` 不需要 root。

`tests/multiuser.py` 使用现有 `nobody`、`daemon` 两个账户，验证真实 UID/GID/附加组切换、输出文件属主、私有队列及越权拒绝；使用临时目录和 CPU 任务，不修改账户配置。开发时可通过 `XLURM_HOME=/tmp/my-xlurm xlurm daemon --backend none` 启动仅当前账户可访问的非 root 测试实例。

接口语义参考：[CUDA 可见设备](https://docs.nvidia.com/deploy/topics/topic_5_2_1.html)、[Ascend 可见设备](https://www.hiascend.com/document/detail/en/canncommercial/850/maintenref/envvar/envref_07_0028.html)、[Linux Unix socket 凭据](https://man7.org/linux/man-pages/man7/unix.7.html)。

本项目深受 [gflow](https://github.com/AndPuQing/gflow) 启发。
