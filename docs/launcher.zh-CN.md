# lgwl：后台启动器

**状态：** `src/bin/lgwl.rs` 与 `src/launcher.rs` 的契约：启动器管什么、把状态记在
哪里，以及什么时候会、什么时候不会给进程发信号。它是一个小型的进程管理员，不是系统服务
管理器。

[English](launcher.md) | **简体中文**

## 用途

`lgw serve` 是前台进程。调试时让它占着终端很有用，但日常运行并不方便：它占一个终端，
终端关掉滚动输出也没了。`lgwl` 用同样的二进制在后台启动网关、记录进程身份、把输出写进
日志，并在需要时把它停掉。

`lgwl` 有意只管理一个实例。第一个还在运行时，再启动一个会被拒绝并显示运行中的 pid。
要在同一台机器上用不同端口跑多个网关，需要按实例分状态目录；目前没有实现。

## 命令

    lgwl launch [--config PATH]     后台启动；已在运行则拒绝
    lgwl status                     显示 pid、启动时间、参数、cwd 和日志路径
    lgwl log [-n 50] [--no-follow]  查看/跟随日志；Ctrl-C 只退出查看器
    lgwl exit                       停止：先 SIGTERM，宽限期后才 SIGKILL
    lgwl update [--repo PATH] [--no-restart]
                                    重新安装；更新前在运行则自动重启

`--config` 原样传给 `lgw`；不传时 `lgw` 按它自己的顺序、相对 `lgwl launch`
执行时的工作目录解析。

## 进程身份

启动器存的不只是 pid。状态记录里还有进程的启动时间（来自 `ps`）、启动参数和工作目录。
之后的 `status`、`exit`、`update` 都会先比较启动时间：pid 被系统复用后，旧记录会被
当作过期记录，而不是网关本身，因此绝不会误杀无关进程。

子进程用 `setsid` 启动，stdin 指向 `/dev/null`，stdout/stderr 追加进日志。关掉终端
或在终端里按 Ctrl-C 都不会停掉网关。

## 停止语义

`lgwl exit` 发送 SIGTERM。`lgw serve` 收到 SIGTERM（以及 SIGINT）后停止监听、让在途
请求走完，然后刷新账本并写最后一条日志。永不结束的流式请求不能把进程永远卡住，所以服务端
在八秒后强制刷新并退出。如果十秒后进程还在，启动器补发 SIGKILL 并明确告知。`update`
换二进制前走同一条停止路径。

只有 `lgwl exit` 是受支持的退出方式。手动 `kill` 当然也能杀掉它；下一次 `launch`
会发现过期记录并替换。

## 状态、日志与覆盖

    ~/.light-ai-gateway/lgwl/state.json       启动记录（pid、启动时间、参数、cwd）
    ~/.light-ai-gateway/lgwl/lgw.log          lgw serve 的 stdout + stderr
    ~/.light-ai-gateway/lgwl/lgwl.lock        串行化 launch/exit/update
    ~/.light-ai-gateway/lgwl/install.conf     scripts/lgw-install.sh 写入

`LGWL_STATE_DIR` 可以改这个目录（测试和隔离 profile 用）。`LGW_BIN` 指定具体的
`lgw` 二进制；默认使用 `lgwl` 旁边的那个，而不是查 PATH。

日志权限 0600，状态目录 0700。日志超过 10 MiB 时在启动前轮转为 `lgw.log.1`；
`lgwl log` 会跟着轮转继续显示新文件。

## 更新

`lgwl update` 不复制安装逻辑，而是运行仓库里的 `scripts/lgw-install.sh`：它编译、
签名（macOS）并安装两个二进制，然后写入 `install.conf`（仓库路径、签名身份、lgw 路径）。

仓库查找顺序：`--repo PATH` → `$LGW_REPO` → 安装器记录的路径 → 当前目录及其父目录。
找不到就报错并给出指令，不猜。

更新前如果网关在运行，先停掉。安装成功后用原来的参数、cwd 和日志重启；`--no-restart`
让它保持停止。安装失败则保持停止并说明原因；除非安装器已经替换，否则旧二进制还在。

## 限制与有意选择

- 仅 Unix：启动器依赖 `setsid`、信号和 `flock`。
- 不是服务管理器：不做开机自启，也不做崩溃自动拉起。那些属于 launchd/systemd；塞进
  这里只会得到一个半成品 supervisor。
- 不管理同一台机器上不同配置的多个网关。
- 日志只保留一代 10 MiB；需要归档就外部处理。
