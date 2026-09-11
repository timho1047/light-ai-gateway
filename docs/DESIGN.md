# light-ai-gateway 设计评审 v5

> v5 定稿。已确认的决策：供应商 = xKiro / DeepSeek / Command Code；全 OpenAI chat completions；只做**同供应商同模型**多账号切换；`allow_metered = false`；不做 hold-until-reset；硬闸门余量 = `max(在途成本 p95, 单请求预估值)`。
> 本轮要点：**账号数不是固定的 2 个，会增长** → 架构必须 N-agnostic，v4 里砍掉的东西要留好扩展缝（§7）。

---

## 1. 账号数会增长，所以先给你一个容量公式

与其猜「够不够」，不如算。soft 阈值 80% 下，**号池能持续承载的燃烧率**：

~~~text
最大可持续燃烧率 = N × soft / 窗口时长

5 小时窗口（soft=0.8）:           7 天窗口（soft=0.8）:
  N=2 → 0.32 窗口/小时             N=2 → 0.0095 窗口/小时
  N=3 → 0.48 窗口/小时             N=3 → 0.0143 窗口/小时
  N=4 → 0.64 窗口/小时             N=4 → 0.0190 窗口/小时
  N=5 → 0.80 窗口/小时             N=5 → 0.0238 窗口/小时
~~~

含义：**增长是线性扩容，不是「越加越划算」。** 加一个号，可持续燃烧率就加 `soft / 窗口时长`。什么时候该加号，用这个式子一算就清楚。

### N=2 的问题会随 N 增大自动消失

我按你的场景做了仿真（燃烧 0.5 窗口/小时，观察 48h）：

~~~text
N=2  → 空闲（无可用账号）占比 19.3%
N=3  → 0.0%
N=4  → 0.0%
~~~

原因：每个账号到 80% 就换，一个号实际只交 80% 的量。N=2 时 1.6 个窗口/5h，扛不住 0.5/h；**N=3 就到 2.4 个窗口/5h，空窗直接消失**。所以「万一不够就加号」这条路是通的——**但那也意味着 N=2 不是设计目标，只是一个特例**。

### 关键原则：不要靠调阈值扩容

号不够时的本能反应是把 soft 从 80% 提到 95%。**不要这么做。** soft 的 80% 不是保守，而是**硬闸门的安全储备来源**——它同时承担了「在途请求可能越过阈值」的缓冲。把 soft 提到 1.0 等于把储备清零，直接暴露在溢出到 metered 的风险下。

**正确做法：soft 固定 80%，容量靠加号解决。** 这也让策略在所有 N 下行为一致，不会因为某天改了阈值而出现未知行为。

---

## 2. 选择规则：用仿真选，不靠直觉

我原本想当然认为「消耗优先（consume-first）」更好。仿真的结论是：**它对吞吐量几乎没有影响，但对切换次数影响巨大。**

~~~text
（燃烧 0.3 窗口/小时，72 小时，统计切换次数）
N   规则            切换次数
2   spread（最空优先）   1421
2   consume（最满优先）    27
3   spread（最空优先）   1419
3   consume（最满优先）    27
5   spread（最空优先）   1426
5   consume（最满优先）    27
~~~

**spread 会疯狂左右横跳**：每次请求都选「最空的」，用完 A 之后 A 略高于 B，下次就选 B，无限抖动。切换次数 ≈ prefix cache 失效次数 ≈ 你为 input token 全价重付的次数。**1421 次 vs 27 次，这不是小差别。**

### 结论：主选择用 consume-first，兜底用 least-utilized

~~~text
1. 候选 = 硬闸门通过的账号
2. 若候选中有 covered_util < 80% 的 → 取其中**利用率最高**的（consume-first）
   → 榨干一个再换，天然形成错峰，切换最少、cache 最稳
3. 若全部 ≥ 80% → 取**利用率最低**的（把最后一段摊平，最大化存活时间）
4. 已有粘性会话若其账号仍过硬闸门 → 直接沿用（覆盖以上规则）
~~~

### 更根本的一条：不要每请求重新决策

横跳的根因是「每个请求都重新选一次」。真实系统里应该**事件驱动**：

~~~text
重新决策只在: 账号跨过 soft 阈值 / 账号被 Parked / 窗口重置 / 发生 429 / 新会话建立
其余情况:      沿用当前选择（sticky）
~~~

这一条比选哪个规则更重要——它把「抖动」这个失败模式从根上消掉。

---

## 3. 本地记账为主，轮询只做校准（你要求低频查询 → 这是必须的）

「低频轮询」和「到 80% 就切」直接矛盾：**15 分钟足够把 5h 窗口从 50% 推到 100%**，你的 80% 根本来不及触发，结果直接溢出到 metered 花真钱。

解法：**你是代理，你看到每个请求的真实 usage，不需要靠轮询知道消耗。**

~~~text
主账（本地，逐请求即时更新）:
    spent_local += price(model, tokens) × credit_multiplier

校准（低频轮询，默认 20 分钟）:
    drift = spent_provider - spent_local
    若 |drift| 超阈值 → 用 provider 值重置主账，并修正单价
~~~

好处：**触发 80% 的判定逐请求实时，不依赖轮询频率。** 轮询退化成防漂移的慢速后台任务。

### 价格自学习

不用手工维护价格表：**用两次轮询之间的 `Δspent / Δtokens` 反推每模型有效单价**，之后本地按它记账。自动吸收 Command Code 的模型倍率（4×/2×/免费）和促销；免费模型（`laguna-s-2.1-free`）自然算出单价 0，不触发轮换。冷启动前**宁可保守高估，不要低估**——低估会直接导致花钱。

### 轮询策略

~~~text
默认 20 分钟；有请求在跑 → 5 分钟；空闲 → 60 分钟
手动: ai-gw status --refresh
切换前: 强制探测目标账号一次（便宜的保险，避免切到已满的号）
失败: 保留上次值 + 标 stale，绝不显示成 0（否则会误判额度耗尽）
~~~

N 增大时探测调用线性增长，但 ×N 之后仍然可以忽略（N=10、20 分钟一次 = 30 次/小时）。

---

## 4. 双层阈值：soft 是偏好，hard 是防线

| 层 | 值 | 作用 |
|---|---|---|
| **soft** | 80% | 路由偏好：有新账号低于 80% 就走它；高于的降级但不封禁 |
| **hard** | ≈100%（按成本算） | **防花钱**：`剩余 covered < 本次预估成本 + max(在途成本 p95, 单请求预估值)` 才拒绝 |

**只有 hard 能拒绝请求**，soft 只影响偏好。这样 80% 的日常切换照常工作，而所有号都超 80% 时继续用最空的号，不会因为自己设的规则集体停摆。

注意 hard 不能用百分比硬编码（如 90%）——agent 一个请求可能一次吃掉几个百分点。**必须用「预估成本 + 在途余量」的实际金额来算。**

---

## 5. covered / metered 分层（唯一不能省的东西）

| 来源 | 例子 | 性质 |
|---|---|---|
| **covered**（套餐内） | Command Code `monthlyCredits`、xKiro `windows[]` | 已付月费；不用就作废 |
| **metered**（真金白银） | Command Code `purchasedCredits`、xKiro `wallet`、DeepSeek 全部余额 | 花掉就没了 |

为什么必须留（三个具体理由）：

1. **Command Code 会静默溢出**：窗口用尽时请求**不报错**，自动改用 `purchasedCredits`（官方原话「Command Code spends those first … and the request goes through」）。**没有错误码，只能靠记账发现。**
2. **在途请求会越过阈值**：你在 80% 切走后，Draining 账号上仍有在途请求和粘性会话，可能把剩余 20% 吃穿并溢出。hard 闸门兜最后一段。
3. **DeepSeek 100% 是 metered**：没有任何 covered 层，所以它天然排在最后。

~~~rust
enum GateClass { Covered, Metered }
// 路由：优先 covered 有余量的账号；全无 → 返回 429（allow_metered=false），不静默花钱
~~~

**DeepSeek 的补充说明**：它是纯钱包型，**没有窗口、没有「80%」这个概念**——余额是一条持续下降的线。所以它的告警要按「绝对值 + 燃烧率」定（比如「余额 < $5 或 < 3 天用量」），而不是百分比。

---

## 6. 请求链路里必须做对的四件事

1. **注入 `stream_options.include_usage`**：流式不注入则上游根本不发 usage 帧（xKiro 原话「self-metering records zero forever」）→ **本地记账全是 0 → 整个方案失效**。且 usage 帧是**单独一帧、`choices` 为空数组**，解析器无条件读 `choices[0]` 会崩。
2. **换账号只能发生在「尚未向客户端写出第一个字节」之前**：先连上游拿状态码，2xx 才开始转发。一旦开始输出就不能换。
3. **绝不 buffer body**：`Body::from_stream` 透传，旁路增量解析。客户端断连要 **abort 上游**，否则继续计费。
4. **错误分类用 `type`/`code`，不用 message**：`402 insufficient_quota` → 禁用账号 + 告警、不重试；`429` 要区分「限速」（同账号退避，保 cache）和「窗口满」（换账号）；5xx → 有界重试。

---

## 7. 为 N 增长留的扩展缝（v4 砍掉的东西在这里归位）

v4 按「只有 2 个号」砍掉了一些机制。**它们现在不实现，但必须留缝**，否则 N 涨上去就要重构：

| 机制 | 现在 | N≥3 之后 | 缝在哪 |
|---|---|---|---|
| **选择策略** | consume-first + least-utilized 兜底 | 可能需要预留容量/权重 | 选择逻辑收敛成**一个纯函数** `select(pool, ctx) -> Account`，策略可换 |
| **错峰调度** | 不做 | 显式错峰有价值 | 账号有 `window_started_at`，调度器可读 |
| **燃烧率 / ETA** | 不做 | 可辅助预判 | `usage_daily` 已留历史，指标可后算 |
| **串行化旋转决策** | 一把 `Mutex` 保护池状态 | 并发高时需要 herd-safe（100 并发只切一次） | 决策收敛成**单个串行函数**，后续替换实现 |
| **配额探测** | 3 个内联 adapter | 更多供应商 | `trait QuotaProbe { fn probe(&self, acct) -> Gates }`，新供应商只加实现 |
| **模型名 → provider** | 静态一对一 | 多供应商时 `vendor/model` 会撞名 | 预留 provider 前缀别名（`xkiro/deepseek/...`），v1 不启用 |

三个真正的扩展点：**select 函数、rotation 决策函数、QuotaProbe trait**。把复杂度关在这三个里面，N 增长就不用动核心。

### 扩容的优先级（重要）

~~~text
1. 同一供应商加号  ← 最便宜：不加 adapter、不加别名、不改策略
2. 加新供应商      ← 需要一个 QuotaProbe 实现；若模型名撞名，还需要前缀别名
3. 改策略(权重/错峰) ← 只在 1、2 都做完还不够时才做
~~~

**先加号，再加供应商，最后才动算法。** 绝大多数「不够用」靠第 1 条就解决了。

### 加新供应商时要留意的两点

- **撞名**：xKiro 和 Command Code 都可能有 `deepseek/...`。加第三家时要么保证 model 名全局唯一，要么启用 provider 前缀别名。
- **币种**：DeepSeek 可能返回 `CNY`，其余是 `USD`。**不要把不同币种相加**；额度显示按 provider 分组。新增供应商时先确认它的 `currency` 字段。

---

## 8. 精简后的额度机械（v1 全部实现）

~~~text
每次请求前:
  候选 = 硬闸门通过的账号（含 covered 余量判定）
  沿用 = 该 session 已绑定的账号若仍在候选 → 直接用（保 cache）
  否则 = 候选里 covered_util<80% 中**最满**的；全 ≥80% → 最空的
  reserve(预估成本) → 发请求 → settle(实际成本)

每次请求后:
  spent_local += 实际成本；跨过 80% → 标记 Draining（不接新会话）

错误:
  402 → 禁用 + 告警（不重试）
  429 短 retry-after → 同账号退避重试
  429 窗口满 → Parked，available_at = resetAt → 还有别的号就切

重置:
  available_at 到点 → 回归 Active，spent_local 归零（或等校准）

全部不可用:
  429 + Retry-After = min(available_at) - now  （不做 hold-until-reset）
~~~

---

## 9. 三家接口速查

| 供应商 | 探测接口 | 内容 |
|---|---|---|
| **Command Code** | `GET /alpha/billing/credits`（host 固定 `api.commandcode.ai`） | `credits.monthlyCredits`(covered) / `purchasedCredits`(metered) / `freeCredits`；`windowLimits.fiveHour|weekly.{cap,used,resetAt}`、`.{limited,exceeded}` |
| **Command Code** | `/alpha/billing/subscriptions`、`/alpha/usage/summary?since=`、`/alpha/whoami` | 计费周期、周期花费、`org.id`（团队需 `?orgId=`） |
| **xKiro** | `GET /v1/usage` | `windows[].{kind,window_sec,spent_usd,cap_usd,remaining_usd,resets_in_sec}`(covered)、`wallet.balance_usd`(metered)、`free_tokens` |
| **DeepSeek** | `GET /user/balance` | `is_available`、`balance_infos[].{currency,total_balance,granted_balance,topped_up_balance}`（全 metered） |

注意事项：

- 金额一律 **`rust_decimal::Decimal`，不要 f64**（xKiro 是六位小数字符串 `"0.000356"`）。
- xKiro 的 `window_sec` **按值判断**哪个是 5h/7d，不要假设 `kind` 的含义。
- Command Code 的 `/alpha/*` **不是公开文档化的**接口，可能变 → 探索失败要有降级（纯本地记账 + `Estimated`）。
- Command Code 窗口从**本账号首次请求**开始计时（非固定时钟）→ 各号天然错开，consume-first 会进一步强化错峰。

---

## 10. 计量与存储

必存：`request_id, ts, session_key, provider, account_id, model, input_tokens, output_tokens, cached_tokens, reasoning_tokens, ttft_ms, total_ms, tps, cost, currency, credit_multiplier, covered_or_metered, status, retries` + 每次尝试一行 `attempts`。

- **TTFT** = 上游请求发出 → 第一个**内容** token（跳过 role-only / 空 delta）。
- **TPS** = `output_tokens / (last_ts - first_ts)`，分母**不含** TTFT。
- **`attempts` 必须存**，否则被重试污染 TTFT/TPS。
- 拿不到 usage → 本地估算 + 标 `measured = false`，不混进真实统计。
- 缓存命中率 `cached_tokens / prompt_tokens` 是核心成本指标（切换次数直接打这个）。

SQLite（`rusqlite` bundled + WAL + `busy_timeout`，单写者 actor 批量提交，DB 不进热路径）：`accounts` / `gates` / `requests` / `attempts` / `usage_daily`。

**本地记账状态必须持久化**（`spent_local`、session 绑定、`available_at`），否则重启后账目归零 → 直接花到 metered。重启后先标 `Unknown`，探测一轮再放行。明细留 30 天，`usage_daily` 永久。默认**不记正文**。

---

## 11. Rust 选型

`tokio` + `axum`/`hyper` + `reqwest`(rustls, 连接池) + `Body::from_stream`；`serde`/`toml`、`clap`、`rusqlite`、**`rust_decimal`**、`tracing`、`jiff`、`secrecy`/`zeroize`、`keyring`、`ratatui`。

**为什么 Rust（理由要换）**：不是「更快」——上游 TTFT 几百 ms 到几秒，网关开销是噪声。真正收益是单静态二进制、常驻内存 10–20MB、每流内存恒定、无 GC 尾延迟抖动。对「常驻本机、几乎不占资源」这个定位才匹配。

---

## 12. Phase 0（约 1 天）

1. `/v1/chat/completions` 透传反代 + model→provider→账号池
2. 本地记账（`include_usage` 注入 + usage 帧解析）
3. 低频轮询校准（三家探测接口）
4. soft(80%) / hard(成本式) 双层阈值 + covered/metered 分层
5. 429/402/5xx 分类 + 未交付前的有界重试
6. 事件驱动的重新决策 + sticky（防抖动，§2）
7. SQLite 落库 + `ai-gw status` / `usage`

**Phase 1**：价格自学习（Δspent/Δtokens）+ 切换前强制探测 + `tail` TUI + `doctor`。

**Phase 2（只有 N 确实涨上来才做）**：错峰调度、权重策略、herd-safe 旋转、新供应商 adapter。

**明确不做**：跨供应商候选链、跨模型兜底、协议翻译、语义缓存、Web UI、多用户/RBAC。

---

## 附录 A：官方 Claude / Codex 订阅额度头（备查，v1 不做）

- **Anthropic**：响应头 `anthropic-ratelimit-unified-*`（`-5h-utilization` / `-7d-utilization` / `-reset` / `-representative-claim` / `-overage-*`），5 个并行 bucket，额度**按成本加权**；`GET /api/oauth/usage`（自带 429，≥5min）；多账号用 `claude setup-token`。
- **Codex**：`x-codex-primary/secondary-used-percent`、`-window-minutes`、`-reset-at`；**gpt-5.4+ 只在 WebSocket 握手响应里**。

## 附录 B：参考来源

- [Command Code Provider API](https://commandcode.ai/docs/provider)、[Usage Limits](https://commandcode.ai/docs/resources/usage-limits)、[Pricing & Limits](https://commandcode.ai/docs/resources/pricing-limits)、[Usage Exceeded](https://commandcode.ai/docs/troubleshooting/errors/usage_exceeded)
- [opencodex PR #1761：Command Code 额度探测实现](https://github.com/lidge-jun/opencodex/pull/1761)
- [xKiro Usage & limits](https://docs.xkiro.com/api/usage/)、[Streaming](https://docs.xkiro.com/guides/streaming/)、[Errors](https://docs.xkiro.com/api/errors/)
- [DeepSeek Get User Balance](https://api-docs.deepseek.com/api/get-user-balance/)
- [dario #1](https://github.com/askalf/dario/discussions/1)、[ccmeter](https://github.com/iteebz/ccmeter/blob/main/docs/source.md)（官方订阅额度头）
- [claude-rotate](https://github.com/doxaras/claude-rotate)（consume-first / herd-safe 决策）、[rolter](https://github.com/rolter-ai/rolter)、[Bifrost](https://github.com/maximhq/bifrost)（Rust / Go 网关先例）
