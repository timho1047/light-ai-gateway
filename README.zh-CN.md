# light-ai-gateway

[English](README.md) | **简体中文**

一个非常小的 OpenAI 兼容网关：把同一家 provider 的多个账号汇成一个池子，在配额窗口
用尽时自动换号，省掉手工切换 API key。

任何 OpenAI 客户端指向它即可，模型名照旧；它会给每个请求挑一个账号、在窗口填满时切换、
并把花了多少钱记下来。

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

- **说 OpenAI Chat Completions。** 一个端点，沿用你现有的 SDK，客户端不用改代码。
- **按 provider 汇账号池。** 同一家 provider、同一个模型、多把 key。
- **在你察觉之前就切换。** 用量到 80% 的账号不再接新会话；请求转到下一个账号，窗口重置
  后再转回来。
- **不烧真钱。** 充值余额和钱包的开销被挡在显式开关后面。
- **记录用量。** 输入、输出、缓存和推理 token，TTFT、TPS、成本、缓存命中率，逐次尝试
  分别记。
- **把数字摆出来。** 终端里一块看板：订阅预付额度 vs 真实余额，带重置倒计时。

设计与缘由见 [docs/DESIGN.md](docs/DESIGN.md)（中文）。本文是中文版，英文版见
[README.md](README.md)。

## 安装

要求 Rust **1.88 或更新** —— 写在 `rust-version` 里，旧工具链会收到一句直白的提示，
而不是撞上一堆语法错误。凭据存储接 macOS Keychain、Windows Credential Manager 和
freedesktop Secret Service；只有 macOS 需要下面那一步签名。

产物是一个自包含的二进制：没有运行时、没有解释器、不需要额外安装任何东西。

    cargo install --path . --locked

它会以 release 模式编译，并把 `lgw` 放进 PATH（`~/.cargo/bin`，用 rustup 装的
Rust 本来就在里面）。检查一下：

    lgw --version

**macOS 上请改用这个脚本：**

    scripts/lgw-install.sh

它跑同样的 `cargo install`，然后用一个固定身份签名。跳过这一步，钥匙串就会在每次运行时
按每个已存 key 问一次密码，永远如此 —— 见 [凭据](#凭据)。第一次还需要先跑
`scripts/lgw-identity.sh` 创建证书。

拿到同一个二进制的其它方式：

    # 自己编译再拷到任意位置；它没有依赖
    cargo build --release && cp target/release/lgw /usr/local/bin/

    # 直接从 git 远端装，等这个仓库推到某处之后
    cargo install --git <repo-url> --locked

卸载：

    cargo uninstall light-ai-gateway

拉到新代码后重新 `cargo install` 即可，它会原地替换二进制。

## 快速开始

    lgw init                                      # 写 ~/.light-ai-gateway/config.toml
    $EDITOR ~/.light-ai-gateway/config.toml         # 填 providers、accounts、routes

    lgw keys add xkiro acc-1                      # 隐藏输入；每个账号一次
    lgw keys add xkiro acc-2
    lgw keys list                                 # 检查是否都能解析

    lgw doctor                                    # 配置、凭据、探测可达性
    lgw serve

东西都在哪：

    ~/.cargo/bin/lgw                          二进制本体
    ~/.light-ai-gateway/config.toml           配置
    ~/.light-ai-gateway/gateway.db            用量账本（SQLite）
    操作系统的凭据存储                        API key

配置的查找顺序：`--config PATH`、`$LGW_CONFIG`、`./lgw.toml`、
`~/.light-ai-gateway/config.toml`。`lgw doctor` 会打印它实际用了哪一个。

然后把客户端指过来：

    export OPENAI_BASE_URL=http://127.0.0.1:8787/v1
    export OPENAI_API_KEY=unused          # 或者你配的 local_key

### 端点

    POST /v1/chat/completions     代理：重试、换号、记账
    GET  /v1/models               本网关愿意服务的模型，由 provider 描述

`/v1/models` 只回答你的 `[routes]`，一个不多：provider 卖但你没路由的模型一律不列
—— 要那些模型只会拿到 "no route for model"，一个列满调不通模型的列表，比一个短列表
更糟。字段来自该 provider 自己的 `/v1/models` 响应，原样透传（上下文长度、价格、能力
标志，上游给什么就给什么），只有 `id`、`object`，以及上游自己没有指名归属时的
`owned_by`（填 provider 名）是由我们定的：

    curl http://127.0.0.1:8787/v1/models                     # 所有 route
    curl "http://127.0.0.1:8787/v1/models?provider=xkiro"    # 某个 provider 的 route

每个 provider 每次请求只调一次上游，用的是有资格账号的 key；模型列表不花 token，会把账号
预留立刻归还。某个 provider 的目录读不到也不会让列表失败：它的 route 照样返回，只是没有
那家的字段，原因写进日志。route 若不在 provider 目录里也照样列出 —— route 才是真正能用的
东西，目录里没有是 provider 的事 —— 开 `RUST_LOG=debug` 时日志会打印最接近的已发布 id，
写错的 vendor 前缀就是这么找出来的（我们这份配置里是 `glm/glm-5.3-flash`，而 xkiro 发布的
是 `z-ai/glm-5.3-flash`）。

## 凭据

Key 存进操作系统自己的凭据存储（macOS Keychain、Windows Credential Manager、freedesktop
Secret Service），由 keyring crate 负责。配置里的账号完全不需要写 key 字段；不写就是按
provider/account 名字去查。

    lgw keys add <provider> <account>     # 隐藏输入，绝不作为命令行参数
    lgw keys add <provider> <account> --stdin
    lgw keys list                         # 来源和状态，从不显示密钥
    lgw keys test <provider> <account>    # 真的向上游探测一次
    lgw keys remove <provider> <account>

走隐藏输入而不是命令行参数，是因为参数对本机每个进程都可见，还会进 shell 历史。

网关自己也有一个 token：它是唯一没有 provider 的凭据，当端口可能被别的机器（或别的用户）
访问时，客户端必须带上它。你自己随便编一个字符串即可；它只用来比对，绝不发往上游：

    lgw keys add local gateway            # 隐藏输入，和其它 key 一样
    # 然后在配置的 [server] 下：
    # local_key = { keychain = true }

`local/gateway` 是唯一保留名，不需要 `[providers.*]` 条目。`lgw keys list` 会把它列在
provider 的 key 旁边，`lgw doctor` 会验证它是否真的能解析出来（而不只是配置里提到它），
`lgw keys test local gateway` 报告它已存储 —— 这一个没有上游可探测。

想显式指定也可以：`key = { keychain = true }`、`key = { env = "X" }`、
`key = { key_file = "/run/secrets/x" }`，或者内联字面量（不推荐）。

### macOS 为什么它老是问你要密码

macOS 把钥匙串条目的访问控制绑在**调用程序的代码签名**上。`cargo install` 产生的是
*ad-hoc* 签名，身份就是代码本身的哈希，于是每次重新编译在钥匙串眼里都是一个新应用，
"Always Allow" 授权活不过一次重建。

除此之外，授权是**按条目**给的，而旧版本每个账号一个条目：冷启动按账号各问一次，`status`、
`accounts probe`、`doctor` 还会再问，因为每个都是新进程去读同样的条目。修好之后，
在这个 checkout 上做过一次测试：读完所有凭据 24 毫秒；修之前它等了三轮弹窗。

两件事能修它：

    scripts/lgw-identity.sh       # 一次性：创建并信任证书
    scripts/lgw-install.sh        # cargo install，然后给结果签名

第一件让授权能留住，因为访问控制条目从此指向一张不随代码变化的证书；第二件签名的是真正
装上去的那个二进制。两者都是可选的 —— 不做的话网关行为完全一样，只是会一直问。

另外现在也只有一个条目需要授权：所有 key 都住在唯一的 `vault` 条目里，`lgw keys add`
一直就是往那里写，`lgw keys list` 会显示每个 key 来自哪个条目。没有东西需要迁移，也没有
命令要记。

如果你是从早于 vault 的版本升级过来的，那些按账号分开的旧条目不再被读取：
`lgw keys add <provider> <account>` 会把 key 存进 vault，旧条目可以在"钥匙串访问"里删掉。

注意普通的 `cargo install --path .` 会用 ad-hoc 签名替换掉已签名的二进制，把弹窗带回来；
请改用 `scripts/lgw-install.sh`。重新签名会恢复已有的授权，不需要重新授权。

如果你完全不想用凭据存储：

    [providers.xkiro.accounts.acc-1]
    key = { key_file = "~/.light-ai-gateway/keys/xkiro-acc-1" }   # chmod 600

0600 的文件能挡住同机其他用户和误提交，但挡不住备份和磁盘镜像 —— 那正是凭据存储多出来的
那一件事。

### 这套东西到底保护了什么

- **保护：** 机器上的其他用户、备份和磁盘镜像、误提交，以及任何只读文件、读不到凭据存储
  的东西。
- **不保护：** 已经以你的身份跑起来的进程。它能向操作系统要同一个密钥；macOS 在会话期间
  会把登录钥匙串解锁。

用一个编译进二进制的密钥去加密凭证文件，*看起来* 能覆盖第二种情况，其实不能：谁拿到二进制
谁就拿到密钥。那是混淆不是加密，所以刻意不做。如果你需要凭据在多台机器之间流转，请从操作
系统的存储里导出，而不是提交一段密文。

设 `LGW_CREDENTIAL_BACKEND=memory` 可以强制使用临时存储，用于测试和干跑，绝不碰你真实的
钥匙串。

## 命令

    lgw serve                          启动代理
    lgw status [--refresh]             账号看板
    lgw usage --since 7d --group-by provider,model,account
    lgw prices [--drift-only]          配置价 vs 从真实花费学到的价
    lgw keys add|list|remove|test      provider 凭据
    lgw accounts list|probe|disable    账号维护
    lgw accounts enable <id>           把被停用的账号放回池子
    lgw tail                           最近的请求
    lgw doctor                         配置、凭据、探测可达性
    lgw init                           写一份示例配置

## 读懂输出

每张表都自己量列宽，所以某个模型名变长不会把后面的列顶歪；数字按读它的量级打印：token
数写成 `921.4K`、`1.0M`，金额在总额位置保留一位小数，而单次请求的成本会保留足够
多位 —— 否则一位小数的规则会把它舍成 0.0。

标题（表头、节标题、价格面板所属的模型名）是加粗青色。青色是唯一没有被赋予含义的颜色，
所以标题不会被误读成状态：绿是正常、黄是变弱、红是出错。暗色的行是图例和修复建议，不是
读数。

除此之外，颜色只标少数需要看一眼的读数 —— 不是 ACTIVE 的状态、STALE 或只是估计的读数、
已经漂移的价格、花了真钱的请求 —— 别的都不标。它从不单独承载信息：关掉颜色，文字还是说
同一件事。输出不是终端时颜色自动关闭，所以管道、重定向和日志拿到的字节和以前完全一样：

    lgw --color always status     # 给截图或编辑器运行面板上色
    lgw --color never status      # 不管终端怎么说，都不上色

`NO_COLOR=1` 关闭，`CLICOLOR_FORCE=1` 打开 —— 后者用来救那些检测不出终端的场景
（`docker run -t`、抓取输出的 CI）。

## 账号恢复

有两种不同的原因会把账号踢出池子，它们的解法不一样。

**失败导致停用** —— 401、403 或 402。池子会停用它，直到一次探测证明它既可达又有钱：

    lgw keys add <provider> <account>    # 如果问题出在凭据，先做这个
    lgw accounts probe <id>

探测回来还有余量，账号就自己回到池子。探测成功但仍然显示没钱的，刻意不复位：否则一个
没钱的账号会每个轮询周期回到轮转里，然后每次都失败。

**运维手工停用** —— `lgw accounts disable <id>`。那是一个决定，探测不会把它撤销：探测证明
的是凭据可用，不是你改了主意。回程是显式的：

    lgw accounts enable <id>

它把账号设成 UNKNOWN 而不是 ACTIVE，所以必须有一次探测确认后才会接流量 —— 和重启遵循同一
条规则。

两者都是按账号的：`accounts disable X` 只写 X 那一行，其它账号的配额读数和重置时间一律
不动。

## 价格

网关必须在发出请求之前知道它要花多少钱：这个估算值就是硬闸门拿去和剩余额度比的东西。
有两个来源。

**配置价（configured）**是你说的成本，按 provider 分开，名字沿用价格页的写法，单位是 USD
每百万 token：

    [prices.xkiro."z-ai/glm-5.3-flash"]
    input = 0.30
    output = 1.20
    "cache read" = 0.03     # 可选；默认是 input / 10

`cache read` 是一个价格，不是比例，所以读法与前两个一样。不写它按输入的十分之一算，
而不是零：看起来免费的缓存读会低估成本，而低估正是会花钱的那个方向。

**学到的价（learned）**来自两次配额探测之间的花费差，它说明实际成本是多少。provider 会
施加按模型的倍率、促销和信用额度，所以观测值更可信，一旦有了就接管计费。配置值保持不动。

这个差值要有意义，得同时满足两个条件，而现实中两者都很容易不成立：

- **这个账号只能通过本网关使用。** provider 是按账号报花费，不是按客户端。用同一个账号跑过
  编辑器，它的花费就落进这个区间，被算到网关的 token 上。
- **区间里得有真实的 token 量。** 一笔花费数字除以一小把 token，就是一个固定按次收费
  披着费率的外衣。
  真实发生过一次：Command Code 的某个区间把 47 个 token 和 0.073 credits 算成了每百万
  1559 USD，于是它拒绝了所有超过约 5,400 token 的请求。现在超过 1000 USD 每百万的费率会
  被拒绝入库，从旧账本里读出来的这种行也一样。

任一条件存疑时，把价格写下来，别等它被学出来。配置价永远不会被一个坏区间覆盖，一个说得通
的数比一张悄悄卡死硬闸门的表值钱。

    lgw prices

    PROVIDER  MODEL               SOURCE   MIX       EXPECTED  ACTUAL  DRIFT      SAMPLES
    xkiro     z-ai/glm-5.3-flash  learned  79/1/20%  $0.2554   $0.9    +252.3% !       12

两列金额都是 **在同一个观测混合比下的 USD 每百万 token**：EXPECTED 是把你的配置价套到该
混合比上的结果，ACTUAL 是把观测到的花费折算到同样用量上的结果。合起来读就是"我以为每百万
$0.26，实际在付 $0.9"。

MIX 不是装饰。同样的价格在不同混合比下每百万差很远 —— 实测输入为主的混合比是 $0.2166/M，
输出为主的混合比是 $0.9/M —— 所以每百万的数只有配上它所依据的混合比才有意义。永远不要跨
混合比比较。

DRIFT 是例外：只要价格吻合它就是零，与混合比无关，所以它才是该盯着的东西，金额列是解释。

学到的价是一个混合后的单一数字。一次探测对三个未知价格只能给一个方程，所以除非你的用量
混合比会变化，输入和输出是分不开的；因此只有你自己配置过的地方才信任这个拆分。

这套组合是监控工具，不只是显示。两者吻合说明什么都没变；偏离 20% 或以上就标记该行并打一条
警告，意味着上游价格不再是你被告知的那个 —— 要么是你听说的调价，要么是你没听说的。
`--drift-only` 只打印这些行。

既没有配置价、也没有观测值的模型，会按一个刻意保守的默认价计费，所以未定价的模型永远不会
显得免费。

学到的价会持久化，重启不会掉回那个默认值。

## 两种钱

每一次配额检查都属于两类之一，网关绝不把它们混为一谈：

| 类别 | 例子 | 行为 |
|---|---|---|
| **Covered** | xKiro 的 5h/7d 花费窗口、Command Code 的月度额度 | 订阅已经预付。窗口重置即作废。优先使用。 |
| **Metered** | xKiro 钱包、Command Code 购买的 credits、DeepSeek 余额 | 真钱。花掉就不再回来。除非运维显式开启，否则拒绝。 |

还有第三类不在这张表里：xKiro 每天的**免费 token 额度**。它会被读取和存储，但它以 token
而不是钱计量，所以不参与路由、不参与硬闸门、也不上看板。用尽它只影响免费模型，而免费模型
不在这里路由；让它去停用一个账号，会因为一份没人花的额度而把付费模型下线。它只会以
provider 自己的 429 形式到达客户端。

这个区分不是装饰。Command Code 在 5 小时窗口用尽时不会报错，而是悄悄从你购买的 credits
扣费，于是请求成功了，余额却掉了。只对 429 做反应的网关会花钱而且永远不告诉你。下面的
硬闸门就是那道防线。

## 账号是怎么选的

对每个请求：

1. **硬闸门。** 只有订阅侧的余量能覆盖"估算成本 + 预留"的账号才是候选。预留取两件事里更大
   的那个：最近请求成本的 p95，以及该账号已经在途占用的量，再加一个配置的余量。这就是防止
   突发流量溢进付费 credits 的机制：二十个并发请求不能各自假装另外十九个不存在。
2. **粘性优先。** 会话已经绑在某个候选账号上，就保持不动。搬动一段对话会丢掉 provider 的
   提示缓存，整段历史按完整输入价重新计费。
3. **先用尽优先。** 在软阈值以下的候选里，挑已用量最大的那个。把一个账号榨干再走，能最小化
   切换次数，而每次切换都意味着一次缓存未命中。（模拟 72 小时实测：先用尽优先产生 27 次
   切换，最少使用优先约 1400 次。）
4. **软阈值是偏好，不是墙。** 如果所有账号都在 80% 以上，仍然用最少使用的那个。你配的阈值
   不能在真的还有余量时变成一次故障。
5. **所有账号都不可用。** 返回 HTTP 429，带一个真实的 Retry-After，取自最早的窗口重置时间。
   它不会悄悄跌到另一家 provider，也不会悄悄花掉 metered 余额。

## 失败处理

一个 429 可能是好几件不同的事，每件要有不同的反应：

| 信号 | 含义 | 动作 |
|---|---|---|
| 429 带很短的 Retry-After | 限速 | 重试**同一个**账号；保持缓存热 |
| 429 带很长的 Retry-After，或配额形状的错误码 | 窗口用尽 | park 该账号，换下一个 |
| 402 / insufficient_quota | 没钱了 | 停用该账号并告警；绝不重试 |
| 401 / 403 | 凭据坏了 | 停用该账号 |
| 529 / 5xx | 上游故障 | 在**另一个**账号上有界重试 |
| 400 / 404 | 请求本身有问题 | 绝不重试 |

换号只可能发生在**第一个字节到达客户端之前**。一旦响应开始流式传输，就原样转发；流到一半
的失败会被如实报告而不是重试，因为重试一个已经开始的流可能被计两次费。

## 指标

TTFT 测到第一个**内容** token，跳过只有 role 和前言性质的块。TPS 的分母不含 TTFT。每一次
上游尝试单独存储，所以重试和换号不会污染计时。

缓存命中率（缓存 token 除以提示 token）是最该看的成本指标：账号切换破坏的正是它。

## 出问题的时候

先跑 `lgw doctor`。它会解析配置、解析每一份凭据、探测每一家 provider，检查两件关于模型名
的、会悄悄对不上的事 —— 没有 route 能到达的价格条目，以及 provider 自己的 `/v1/models`
里没有的 route（它会给出最接近的已发布 id）—— 标出别的机器能连上的端口，并以非零退出码和
问题计数结束。

第二项值得展开说：route 的 key 就是发往上游的模型 id，所以写错的 vendor 前缀
（provider 有的是 `z-ai/glm-5.3-flash`，你写了 `glm/glm-5.3-flash`）是一个会被上游拒绝的
请求 —— 它还会让为真实 id 写的那条价格变成孤儿，于是该 route 按保守默认价（输入 15 / 输出
75 每百万）计费。这两半会分别报告，因为任何一半都可能单独出现。

| 症状 | 往哪看 |
|---|---|
| 每个请求都 `no eligible account` | `lgw status` —— 每个账号可能被 park、被停用，或卡在硬闸门下。429 的响应体里会点名余量最大的账号、还剩多少，以及没装下的那个估算值 |
| 修好 key 后账号仍是 DISABLED | 探测只会恢复同时还有余量的账号，而运维手工停用永远不会被探测撤销 —— 见 [账号恢复](#账号恢复) |
| 每次运行都弹密码框（macOS） | 授权绑在代码签名上，ad-hoc 签名每次重建都会变：跑 `scripts/lgw-install.sh` |
| 价格看起来不对 | `lgw prices --drift-only`。学到的价是观测，不是真理，而配置价永远不会被它覆盖 |
| 每个请求都是 429 且 Retry-After 是几周 | 某个 covered 窗口用尽了，最早的窗口重置就有那么远。`lgw status` 会为每个闸门显示倒计时 |
| 换过数据库位置后看板是空的 | 账本路径是配置里的 `server.db_path`。空看板的意思是"还没探测过"，而不是"没有账号" |

## 明确不做

跨 provider 兜底、跨模型降级、协议翻译、语义缓存、多用户 RBAC、Web UI —— 全都刻意不在范围内。
一个模型只映射到一家 provider；想改那是产品决定，不是网关该替你推断的事。

## 开发

    cargo test                              # 整个测试套件，几秒钟
    cargo clippy --all-targets -- -D warnings

两者都在每次 push 的 CI 里跑，另外还有一个 release 构建。没有 tests/ 目录：测试就住在它所
覆盖的代码旁边，在各个模块内，并且按它断言的行为命名。

各家 provider 的配额适配器在 src/quota。新增一家 provider 意味着实现一个 trait，并声明它的
base URL、凭据和错误词表。`adapter = "none"` 刻意不支持：账号只有在一次成功探测之后才会被
接纳进池子，所以没有配额来源的 provider 永远不会被选中。请用 xkiro、command-code 或
deepseek。

## 许可证

MIT。见 [LICENSE](LICENSE)。随便用、改、发布、卖 —— 没有担保，而你把它配置成会花钱时，
它确实会花真钱。

## 安全说明

- 默认只绑 loopback，而它持有付费凭据；如果可能有别的东西能连到端口，请设一个本地 key。
  `lgw doctor` 会把缺失的 `local_key` 和非 loopback 的监听地址都报成问题。
- 密钥来自环境变量或专用文件，从不写日志，在所有调试和展示路径里都会被遮掉。
- 探测主机固定在配置里，重定向一律拒绝，所以凭据不可能被发到请求里给的地址，或某个
  Location 头点名的地址。
- 一次失败的配额探测会保留上一次读数并标记为 stale。它绝不把读数清零 —— 那既会高估可用量，
  也会让 CLI 说一个还有钱的账号是空的。
- 提示和响应正文从不存储。
