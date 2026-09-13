# light-ai-gateway

[English](README.md) | **简体中文**

一个很小的 OpenAI 兼容网关。它把同一家 provider 的多个账号汇成一个池子，配额窗口用尽时
自动换号，省得你手工切 API key。

任何 OpenAI 客户端指过来即可，模型名照旧。它会替每个请求挑账号，窗口满了就切，并把花了
多少钱记下来。

**目录**

- [它做什么](#它做什么)
- [安装](#安装)
- [快速开始](#快速开始)
- [凭据](#凭据)
- [命令](#命令)
- [读懂输出](#读懂输出)
- [账号恢复](#账号恢复)
- [价格](#价格)
- [两种钱](#两种钱)
- [账号是怎么选的](#账号是怎么选的)
- [失败处理](#失败处理)
- [指标](#指标)
- [出问题的时候](#出问题的时候)
- [明确不做](#明确不做)
- [开发](#开发)
- [许可证](#许可证)
- [安全说明](#安全说明)

## 它做什么

- **说 OpenAI Chat Completions。** 只有一个端点，继续用你现有的 SDK，不用改代码。
- **按 provider 汇账号池。** 同一家 provider、同一个模型，可以挂好几把 key。
- **在你察觉之前就换号。** 用量到 80% 的账号不再接新会话，请求转给下一个；窗口重置后它
  再回来。
- **不烧真钱。** 充值余额和钱包支出默认锁死，只有你显式打开才会动用。
- **把用量记全。** 输入、输出、缓存、推理 token，加 TTFT、TPS、成本和缓存命中率，逐次
  尝试分别记。
- **把数字摆出来。** 终端里一块看板：订阅额度还剩多少、真钱余额还剩多少，都带重置倒计时。

## 安装

要求 Rust **1.88 或更新**。凭据存在操作系统自带的凭据库里（macOS 钥匙串、Windows
Credential Manager、freedesktop Secret Service），只有 macOS 需要下面那一步签名。

编译产物就是一个自包含的二进制：没有运行时，没有解释器。

    cargo install --path . --locked

它以 release 模式编译，并把 `lgw` 装进 PATH（`~/.cargo/bin`）。验证一下：

    lgw --version

**macOS 上请改用这个脚本：**

    scripts/lgw-install.sh

它做的是同一条 `cargo install`，然后给产物签名。跳过这步，钥匙串会在每次运行时按每个
已存的 key 弹一次密码框 —— 见 [凭据](#凭据)。第一次还需要先跑
`scripts/lgw-identity.sh` 生成证书。

拿到同一个二进制的其它办法：

    # 自己编译，再拷到任意位置；它没有依赖
    cargo build --release && cp target/release/lgw /usr/local/bin/

    # 直接从 git 远端装（等这个仓库推到某处之后）
    cargo install --git <repo-url> --locked

卸载：

    cargo uninstall light-ai-gateway

拉到新代码后重新 `cargo install` 就行，它会原地替换掉旧二进制。

## 快速开始

    lgw init                                      # 生成 ~/.light-ai-gateway/config.toml
    $EDITOR ~/.light-ai-gateway/config.toml         # 填 providers、accounts、routes

    lgw keys add xkiro acc-1                      # 隐藏输入；每个账号一次
    lgw keys add xkiro acc-2
    lgw keys list                                 # 确认都能解析出密钥

    lgw doctor                                    # 配置、凭据、探测连通性
    lgw serve

各文件的位置：

    ~/.cargo/bin/lgw                          二进制本体
    ~/.light-ai-gateway/config.toml           配置
    ~/.light-ai-gateway/gateway.db            用量账本（SQLite）
    操作系统的凭据库                          API key

配置文件按这个顺序查找：`--config PATH` → `$LGW_CONFIG` → `./lgw.toml` →
`~/.light-ai-gateway/config.toml`。`lgw doctor` 会告诉你它用的是哪一个。

然后把客户端指过来：

    export OPENAI_BASE_URL=http://127.0.0.1:8787/v1
    export OPENAI_API_KEY=unused          # 或者你配的 local_key

### 端点

    POST /v1/chat/completions     代理：重试、换号、记账
    GET  /v1/models               本网关会服务的模型，由 provider 描述

`/v1/models` 返回的就是你的 `[routes]`，一个不多：provider 在卖、但你没路由过去的模型
不会出现 —— 请求它只会拿到「no route for model」。其余字段（上下文长度、价格、能力标志）
原样来自该 provider 的 `/v1/models`；只有 `id`、`object` 是我们写的，`owned_by` 也
只在上游自己没给的时候才填 provider 名：

    curl http://127.0.0.1:8787/v1/models                     # 所有路由
    curl "http://127.0.0.1:8787/v1/models?provider=xkiro"    # 某个 provider 的路由

每个 provider 每次请求只问一次上游，用的是有资格账号的 key；模型列表不花 token。哪家的
目录读不到，也不会让整个列表失败：它的路由照常返回，只是没有那家的字段，原因写进日志。
路由不在 provider 目录里也照常列出 —— 路由才是真正能用的东西 —— 开 `RUST_LOG=debug` 时，
日志会打出最接近的已发布 id；vendor 前缀写错就是这么查出来的（`glm/glm-5.3-flash`，而
xkiro 发布的是 `z-ai/glm-5.3-flash`）。

## 凭据

密钥存进操作系统自带的凭据库（macOS 钥匙串、Windows Credential Manager、freedesktop
Secret Service），走的是 keyring crate。配置里的账号一个 key 字段都不用写；不写就是按
provider/account 名字去查。

    lgw keys add <provider> <account>     # 隐藏输入，绝不作为命令行参数
    lgw keys add <provider> <account> --stdin
    lgw keys list                         # 显示来源和状态，从不显示密钥
    lgw keys test <provider> <account>    # 真的向上游探一次
    lgw keys remove <provider> <account>

密钥走隐藏输入，不作为命令行参数 —— 参数对本机任何进程都可见，还会留在 shell 历史里。

网关自己也有一把 token，它是唯一没有 provider 的凭据：一旦有别的东西能够到这个端口，
客户端就必须带上它。随便编一个字符串即可，它只用于比对，绝不发往上游：

    lgw keys add local gateway            # 隐藏输入，和其它 key 一样
    # 然后在配置的 [server] 下加一行：
    # local_key = { keychain = true }

`local/gateway` 是唯一的保留名，不需要 `[providers.*]` 条目。`lgw keys list` 会把它和
provider 的 key 列在一起；`lgw doctor` 会真的去解析它；`lgw keys test local gateway`
报告它已存储。

想显式指定也可以：`key = { keychain = true }`、`key = { env = "X" }`、
`key = { key_file = "/run/secrets/x" }`，或者内联字面量（不推荐）。

### macOS 为什么它老是问你要密码

macOS 把钥匙串条目的访问控制绑在**调用方的代码签名**上。`cargo install` 产出的是
*ad-hoc* 签名，身份就是代码本身的哈希；于是每次重新编译，在钥匙串眼里都算换了一个应用，
「Always Allow」这种授权活不过一次重建。

授权**按条目**给，而旧版本每个账号一个条目：冷启动按账号各弹一次，每条命令还会再各弹
一次 —— 它们各自是新进程，读的是同一批条目。

两招解决：

    scripts/lgw-identity.sh       # 一次性：创建并信任证书
    scripts/lgw-install.sh        # cargo install，然后给产物签名

第一招让授权留得住：访问控制条目从此指向一张不随代码变化的证书。第二招给真正装上去的
二进制签名。两招都可选。

另一个变化是只剩一个条目需要授权：所有 key 都住在唯一的 `vault` 条目里。`lgw keys list`
会显示每个 key 来自哪个条目。

如果你是从早于 vault 的版本升上来的：那些按账号分开的旧条目不再被读取。重新
`lgw keys add <provider> <account>` 把密钥存进 vault，旧条目可以在「钥匙串访问」里删掉。

注意：直接 `cargo install --path .` 会用 ad-hoc 签名覆盖掉已签名的二进制，弹窗又回来了；
请用 `scripts/lgw-install.sh`。重新签名会恢复原有授权。

如果你干脆不想用凭据库：

    [providers.xkiro.accounts.acc-1]
    key = { key_file = "~/.light-ai-gateway/keys/xkiro-acc-1" }   # chmod 600

0600 的文件能挡住同机其他用户和误提交，但挡不住备份和磁盘镜像 —— 那正是凭据库多出来的
那一件事。

### 这套东西到底保护了什么

- **挡得住：** 机器上的其他用户、备份和磁盘镜像、误提交，以及任何能读文件但读不到凭据库
  的东西。
- **挡不住：** 一个已经以你的身份跑起来的进程。它能直接向系统要同一个密钥；macOS 在会话
  期间会把登录钥匙串解锁。

把密钥编译进二进制再拿去加密文件，挡不住第二种情况：谁拿到二进制，谁就拿到密钥。需要让
凭据在多台机器之间流转，请从系统凭据库里导出，而不是提交一段密文。

设 `LGW_CREDENTIAL_BACKEND=memory` 可以强制使用临时存储，测试和干跑很有用，也绝不碰你
真实的钥匙串。

## 命令

    lgw serve                          启动代理
    lgw status [--refresh]             账号看板
    lgw usage --since 7d --group-by provider,model,account
    lgw prices [--drift-only]          配置价 vs 从真实花费学到的价
    lgw prices --forget [PROVIDER [MODEL]]
                                       丢掉学到的价，回到配置价
    lgw keys add|list|remove|test      provider 凭据
    lgw accounts list|probe|disable    账号维护
    lgw accounts enable <id>           把被停用的账号放回池子
    lgw tail                           最近的请求
    lgw doctor                         配置、凭据、探测连通性
    lgw init                           生成一份示例配置

## 读懂输出

每张表的列宽都是自己量出来的：某个名字变长，不会把后面的列顶歪。数字也按读它的量级打印：
token 数写成 `921.4K`；金额在总额位置保留一位小数，而单次请求的成本会保留足够位数。

标题（表头、节标题、价格面板归属的模型名）用加粗青色。青色没有含义，所以标题不会被误读成
状态 —— 绿是正常、黄是变差、红是出错。暗色行是图例和建议，不是读数。

颜色只标需要看一眼的读数：不是 ACTIVE 的状态、STALE 或只是估算的读数、已经漂移的价格、
花了真钱的请求，别的都不标。颜色不单独承载信息，而且输出不是终端时自动关闭，所以管道、
重定向和日志文件拿到的字节和以前一模一样：

    lgw --color always status     # 给截图或编辑器运行面板上色
    lgw --color never status      # 不管终端怎么说，都不上色

`NO_COLOR=1` 关，`CLICOLOR_FORCE=1` 开，用于那些检测不出终端的场景（`docker run -t`、
会抓走输出的 CI）。

## 账号恢复

有两种原因会把账号踢出池子，解法完全不同。

**失败导致停用** —— 401、403 或 402。池子会一直不用它，直到一次探测证明它既能连通、又有钱：

    lgw keys add <provider> <account>    # 如果问题出在凭据，先做这个
    lgw accounts probe <id>

探测回来还有余量，账号就自动回到池子；探测成功但仍然没钱的则不会：否则它每个轮询周期都会
重新进轮转，然后每次都失败。

**运维手工停用** —— `lgw accounts disable <id>`。这是一个决定，探测不会替你撤销：探测只能
证明凭据可用，证明不了你改了主意。回程得显式走：

    lgw accounts enable <id>

它把账号设成 UNKNOWN 而不是 ACTIVE，所以必须有一次探测确认它才会接流量 —— 和重启同一条
规则。

两者都只影响单个账号：`accounts disable X` 只写 X 那一行，其它账号的配额读数和重置时间
原封不动。

## 价格

网关必须在发请求之前知道这一单要花多少钱：硬闸门拿这个估算值去和剩余额度比。这个数字有
两个来源。

**配置价（configured）**是你自己说的成本，按 provider 分开写，名字沿用价格页的写法，单位
是 USD / 百万 token：

    [prices.xkiro."z-ai/glm-5.3-flash"]
    input = 0.30
    output = 1.20
    "cache read" = 0.03     # 可选；默认按 input / 10

`cache read` 写的是价格，不是比例，所以读法和前两个一样。不写它就按输入的十分之一算，
而不是按零算。

**学到的价（learned）**来自两次配额探测之间的花费差，反映的是实际成本。provider 会按模型
加倍率、做促销、走信用额度，所以观测值更可信；一旦有观测值，计费就改用它，配置值原样保留。

但这个差值要有意义，得同时满足两个条件，而现实中两个都很容易不成立：

- **这个账号只能经过本网关调用。** provider 按账号报花费，不按客户端分。同一个账号如果在
  编辑器里也用过，那部分花费会落进这个区间，被算到网关的 token 上。
- **区间里得有像样的 token 量。** 花费除以一小把 token，就是一个按次收费披着费率的外衣。
  超过 1000 USD/百万的费率会被拒绝入库，从旧账本里读出来的这种行也一样。

只要有一个条件存疑，就把价格写下来，别等它被学出来。坏区间永远覆盖不了配置价；一个说得通
的数字，比一张悄悄卡死硬闸门的表值钱。

    lgw prices

    PROVIDER  MODEL               SOURCE   MIX       EXPECTED  ACTUAL  DRIFT      SAMPLES
    xkiro     z-ai/glm-5.3-flash  learned  79/1/20%  $0.2554   $0.9    +252.3% !       12

两列金额都是**在同一观测混合比下的 USD / 百万 token**：EXPECTED 是把你的配置价套到那个
混合比上的结果，ACTUAL 是观测到的花费折算到同样用量上的结果。合起来读就是：「我以为每百万
$0.26，实际在付 $0.9」。

同样的价格，混合比不同，每百万能差很远（输入为主 $0.2166/M，输出为主 $0.9/M），所以一个
每百万的数字，只有配着它依据的混合比看才有意义。DRIFT 是例外：只要价格对得上就是零，跟
混合比无关。

学到的价是一个混合后的单一数字：除非你的用量混合比会变，输入和输出分不开，所以只有你配置
过的地方才信任这个拆分。

两边对得上，说明什么都没变；差到 20% 或以上，该行会被标记并打一条警告 —— 上游价格不再是
你配置的那个。`--drift-only` 只打印这些行。

既没配价、也没观测值的模型，会按一个刻意定高的默认价计费，所以未定价的模型绝不会显得免费。

学到的价会持久化，重启不会掉回那个默认值。

学到的价是全历史均值，一旦被外部用量污染，就会一直偏高。忘掉它，配置价立刻生效：

    lgw prices --forget                          # 全部学到的行
    lgw prices --forget xkiro                    # 某一个 provider
    lgw prices --forget xkiro z-ai/glm-5.3-flash # 某一个模型

    PROVIDER  MODEL               TIER  RATE     CACHE READ  SAMPLES
    xkiro     z-ai/glm-5.3-flash     0  $0.4071  $0.0081          44

跑着的 `lgw serve` 自己存着一份表，下次探测还会写回来，所以要重启才生效。忘掉一个本来就没
配价的模型，它会掉到那个刻意定高的默认价 —— 要么先把价格写上，要么就别动它。

某个区间里花费涨了，网关这边却没有 token 可以摊，这笔钱会被记成无法归属，并跳过学习：账号
在别处也在用，而这正是学到的价失去意义的时刻。

## 两种钱

每一次配额检查都属于两类之一，网关从不把它们混为一谈：

| 类别 | 例子 | 行为 |
|---|---|---|
| **Covered** | xKiro 的 5h/7d 花费窗口、Command Code 的月度额度 | 订阅已预付。窗口重置即作废。优先使用。 |
| **Metered** | xKiro 钱包、Command Code 购买的 credits、DeepSeek 余额 | 真钱。花掉就不再回来。运维没显式开启就拒绝。 |

第三类不在这张表里：xKiro 每天的**免费 token 额度**。它会被读取、会被存储，但单位是 token
而不是钱，所以不参与路由、不参与硬闸门、也不上看板。它只会以 provider 自己的 429 形式到达
客户端。

这个区分很重要。Command Code 在 5 小时窗口用尽时不报错，它会悄悄从你买的 credits 里扣钱
—— 请求成功了，余额却少了。下面那道硬闸门防的就是这个。

## 账号是怎么选的

对每个请求：

1. **硬闸门。** 只有「订阅侧余量 ≥ 估算成本 + 预留」的账号才是候选。预留取两者中较大的：
   最近请求成本的 p95、该账号已在途占用的额度，再加一个配置的余量。
2. **粘性优先。** 会话已经绑在某个候选账号上，就别动它。搬一次对话会丢掉 provider 的提示
   缓存，整段历史按完整输入价重新计费。
3. **先用尽优先。** 软阈值以下的候选里，挑已用量最大的那个。把一个账号榨干再换，切换次数
   最少，而每次切换都意味着一次缓存未命中。
4. **软阈值是偏好，不是墙。** 所有账号都超过 80% 时，仍然用最少的那个。你配的阈值不能在
   确实还有余量的时候变成一次故障。
5. **所有账号都不可用。** 返回 HTTP 429，带真实的 Retry-After（取最早的窗口重置时间）。
   它不会悄悄跌到另一家 provider，也不会悄悄花掉 metered 余额。

## 失败处理

同一个 429 可能是好几件不同的事，每件要有不同的反应：

| 信号 | 含义 | 动作 |
|---|---|---|
| 429 且 Retry-After 很短 | 限速 | 重试**同一个**账号，保持缓存热 |
| 429 且 Retry-After 很长，或错误码是配额形状 | 窗口用尽 | park 该账号，换下一个 |
| 402 / insufficient_quota | 没钱了 | 停用该账号并告警；绝不重试 |
| 401 / 403 | 凭据坏了 | 停用该账号 |
| 529 / 5xx | 上游故障 | 在**另一个**账号上有界重试 |
| 400 / 404 | 请求本身有问题 | 绝不重试 |

换号只可能发生在**第一个字节到达客户端之前**。响应一旦开始流式传输就原样转发；流到一半的
失败会被如实报告，而不是重试 —— 重试一个已经开始的流，可能被计两次费。

## 指标

TTFT 测到第一个**内容** token，跳过只带 role 和开场白性质的块。TPS 的分母不含 TTFT。每次
上游尝试单独入库，所以重试和换号不会污染计时。

缓存命中率（缓存 token ÷ 提示 token）是最该盯的成本指标：账号切换破坏的正是它。

## 出问题的时候

先跑 `lgw doctor`。它会解析配置、解析每一份凭据、探测每一家 provider，还会检查两件「模型
名会悄悄对不上」的事 —— 没有路由能到达的价格条目，以及 provider 自己的 `/v1/models` 里
没有的路由（它会给出最接近的已发布 id）—— 再标出别的机器能连上的端口，最后以非零退出码
和问题计数结束。

路由的 key 就是发往上游的模型 id，所以 vendor 前缀写错既是上游会拒的请求，也会让为真实 id
写的那条价格变成孤儿，让该路由按保守默认价（输入 15 / 输出 75 每百万）计费。

| 症状 | 往哪看 |
|---|---|
| 每个请求都是 `no eligible account` | `lgw status` —— 账号可能被 park、被停用，或者卡在硬闸门下。429 的响应体会点名余量最大的账号、还剩多少，以及那个没装下的估算值 |
| 修好 key 之后账号仍是 DISABLED | 探测只会恢复同时还有余量的账号，而运维手工停用永远不会被探测撤销 —— 见 [账号恢复](#账号恢复) |
| 每次运行都弹密码框（macOS） | 授权绑在代码签名上，ad-hoc 签名每次重建都会变：跑 `scripts/lgw-install.sh` |
| 价格看着不对 | `lgw prices --drift-only`。学到的价是观测值，不是真理，而配置价永远不会被它覆盖 |
| 学到的价明显高于价目表 | 账号在网关之外也被用过，那部分花费被算到了这些 token 上。`lgw prices --forget <provider> <model>`，然后重启 `lgw serve` |
| 每个请求都是 429，Retry-After 还是几周 | 某个 covered 窗口用尽了，最早的窗口重置就有那么远。`lgw status` 会按闸门显示倒计时 |
| 换过数据库位置后看板是空的 | 账本路径是配置里的 `server.db_path`。空看板的意思是「还没探测过」，不是「没有账号」 |

## 明确不做

跨 provider 兜底、跨模型降级、协议翻译、语义缓存、多用户 RBAC、Web UI —— 都不在范围内，
这是刻意的。一个模型只映射到一家 provider；要改那是产品决定。

## 开发

    cargo test                              # 整个测试套件，几秒钟
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check

仓库里没有 CI 流水线：改动离开本机之前，跑这三条，再加一次 release 构建。也没有 tests/
目录 —— 测试就放在它覆盖的代码旁边、各个模块内部，并且按它断言的行为命名。

各家 provider 的配额适配器在 src/quota。加一家 provider = 实现一个 trait，再声明它的
base URL、凭据和错误词表。`adapter = "none"` 刻意不支持：账号只有探测成功后才进池子，
所以没有配额来源的 provider 永远选不中。请用 xkiro、command-code 或 deepseek。

## 许可证

MIT，见 [LICENSE](LICENSE)。随便用、随便改、随便发布、随便卖 —— 不提供任何担保，而当你把
它配置成会花钱时，它确实会花真钱。

## 安全说明

- 默认只绑 loopback，而它握着付费凭据；只要有别的东西能连到这个端口，就请设一个本地 key。
  缺 `local_key`、监听地址不是 loopback，`lgw doctor` 都会报成问题。
- 密钥来自环境变量或专用文件，从不进日志，在所有调试和展示路径里都会被脱敏。
- 探测主机写死在配置里，重定向一律拒绝，所以凭据不可能被发到请求指定的地址，也不可能被发
  到 Location 头点名的地址。
- 配额探测失败时保留上一次读数并标记为 stale；绝不把读数清零。
- 提示和响应正文从不存储。
