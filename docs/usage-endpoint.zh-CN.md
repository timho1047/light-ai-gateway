# GET /v1/usage

**状态：** 已实现，代码在 `src/quota_report.rs` 和 `src/proxy.rs` 里的 `usage` 处理器。本文件是这两者要守的契约：下面这些路径是公开的，挂件模板一旦钉在上面，路径一动就会坏。

[English](usage-endpoint.md) | **简体中文**

读取账号池最近一次已知的配额，形状专门给 JSON-path 客户端用 —— 尤其是
[DeepSeek-Balance-Whale-Widget](https://github.com/MeteorNOX/DeepSeek-Balance-Whale-Widget)
—— 让它能显示滚动窗口和剩余余额，而不必理解本网关的账号模型。

数字就是 `lgw status` 已经有的那些。要做的是投影，不是一次新的探测。

## 为什么要有它

网关把若干 provider 账号汇在一把 `local_key` 后面。鲸鱼把一把 API key 当成一个
厂商账号，用 `Authorization: Bearer {key}` 发 GET，再按路径从 JSON 里挑字段。

这两套视图要对上，这个端点必须：

1. 说鲸鱼现有解析器的话（`quota.json.windows` 和 `balance.json`）。
2. 把一个池子聚成鲸鱼已经会渲染的三个窗口名（`rolling` / `weekly` /
   `monthly`），外加一个剩余金额。
3. 绝不编造一个零，绝不混用币种，绝不返回一把可用的上游账号 key。

第二个 URL、第二把 token、或鲸鱼专用的 `/api/whale/*`，都是第二份契约。不要加。

## 消费者契约

鲸鱼的宿主（`DeepSeek-Balance-Whale-Widget/lib/index.js` 里的
`fetchModelQuota` / `fetchModelBalance`）是 JSON-path 客户端，不是 schema
客户端。兼容靠下面这些规则，不靠一份写成文档的 OpenAPI。

| 规则 | 原因 |
|---|---|
| 方法是 GET | `apiFetchJson` 从不 POST |
| `Authorization: Bearer <token>` | 鲸鱼默认的 `auth: 'Bearer {key}'` |
| 成功时 HTTP 200 | 其它状态会被当成 `HTTP {n}` 抛出 |
| `percent` 是 **已用** 0–100，不是剩余 | OpenCode Go 和鲸鱼都按已用理解 |
| `resetsAt` 是 RFC 3339，或 unix 时间戳（`< 1e12` 当秒，否则当毫秒） | `apiPlanResetMs` |
| 多窗口字段是 **具名路径**，不是 `windows[i]` | 模板不能遍历 |
| UI 已经认得的窗口键：`rolling`、`weekly`、`monthly` | `BUBBLE_PLAN_WIN_OPTS` |
| 200 的 body 不能长得像业务错误 | `ok:false`、`success:false`、`error.message`、或非 0/200 的数字 `code` 再加 `msg`，都会让解析失败 |

配额 URL 只来自鲸鱼 **模板**（`tpl.quota`）。模型行不能覆盖它们。内置
`custom` 模板只有 `balance`，所以窗口需要一份 `kind: 'quota'` 模板（见「鲸鱼模板」一节）。
`balance` 和配额可以打同一个 URL；鲸鱼会 GET 两次。

回环和局域网 URL 允许。鲸鱼存的 token 是本网关的 `local_key`，绝不是上游账号 key。

## 端点

    GET /v1/usage
    GET /v1/usage?provider=<provider-id>
    GET /v1/usage?account=<provider>/<account-id>

注册在 `/v1/models` 旁边，同一条 axum router，同一层 `require_local_key`
中间件后面。

### Query

| 参数 | 含义 |
|---|---|
| `provider` | 快照只保留该 provider id（一个 `[providers.*]` 键）。 |
| `account` | 只保留一个账号。必须是 **限定** id（`xkiro/acc-1`）。裸的 `acc-1` 一律拒绝：账号 id 只在同一个 provider 内唯一。 |

两个都给时，该账号必须属于那个 provider；否则 404。

鲸鱼不会自己发明查询参数。把过滤条件写进模板 URL
（`{base}/v1/usage?provider=xkiro`）。一头鲸鱼模型对应一个 provider，是预期的配对。

### Auth

与 `/v1/chat/completions` 和 `/v1/models` 相同：

- 配置了 `server.local_key` 时：`Authorization: Bearer <local_key>`。
- 没配置 `local_key`：端口敞开，警告与现在一样。
- 配置了 `local_key` 但读不到：拒绝每一个请求（失败即关闭）。

这里不要接受上游账号 key。这个端点谈的是账号池，而本地 key 已经打开的就是这个池。

## 响应（200）

一个对象里两层。

- `usage` 和 `balance` 是给鲸鱼的投影。它们的路径是冻结的。
- `accounts` 是原生视图，与 `lgw status` 对齐。鲸鱼会忽略未知字段；这一层可以往后长。

`plan` **不属于** 第一期实现。`ProbeOutcome.plan` 目前会被 `Pool::apply_probe`
丢掉。不要编一个套餐名，也不要为了填这个字段去扩张池子的 schema。

```json
{
  "object": "usage",
  "checked_at": "2026-04-08T12:04:11Z",
  "stale": false,
  "currency": "USD",
  "usage": {
    "rolling": {
      "percent": 47.5,
      "resetsAt": "2026-04-08T16:30:00Z"
    },
    "weekly": {
      "percent": 22.0,
      "resetsAt": "2026-04-13T00:00:00Z"
    },
    "monthly": {
      "percent": 18.5,
      "resetsAt": "2026-05-01T00:00:00Z"
    }
  },
  "balance": {
    "remaining": 5.00,
    "used": 0,
    "total": 5.00,
    "currency": "USD"
  },
  "accounts": [
    {
      "id": "xkiro/acc-1",
      "provider": "xkiro",
      "label": "acc-1",
      "state": "active",
      "currency": "USD",
      "stale": false,
      "last_probe_at": "2026-04-08T12:04:11Z",
      "windows": {
        "rolling": {
          "used": "9.50",
          "cap": "20.00",
          "resetsAt": "2026-04-08T16:30:00Z"
        },
        "weekly": {
          "used": "17.60",
          "cap": "80.00",
          "resetsAt": "2026-04-13T00:00:00Z"
        },
        "monthly": {
          "used": "37.00",
          "cap": "200.00",
          "resetsAt": "2026-05-01T00:00:00Z"
        }
      },
      "pots": [
        {
          "kind": "wallet",
          "remaining": "5.00",
          "used": "0"
        }
      ]
    }
  ]
}
```

### 顶层字段

| 字段 | 含义 |
|---|---|
| `object` | 永远是 `"usage"`。 |
| `checked_at` | RFC 3339。参与投影的账号里最晚的 `last_probe_at`。 |
| `stale` | 任一参与投影的 gate 过期则为 `true`。 |
| `currency` | 仅当所有贡献进来的金额是同一种货币时才出现。否则省略。 |
| `usage` | 仅当该范围能干净地投影时才出现（见「聚合」）。 |
| `usage.<window>` | 没有可用 gate 就省略。**绝不**用 `percent: 0` 凑数。 |
| `usage.<window>.percent` | JSON number。已用 × 100，四舍五入到 1 位小数，钳在 0–100。 |
| `usage.<window>.resetsAt` | RFC 3339。没有就省略。 |
| `balance` | 仅当至少有一个计量罐，且只有一种货币时才出现。 |
| `accounts` | 永远在。是 DTO。**绝不要**对 `domain::Account` 做 serde（那个类型里有 `ApiKey`）。 |

### 窗口键映射

| 鲸鱼键 | `GateKind` | 说明 |
|---|---|---|
| `rolling` | `FiveHour` | Covered。 |
| `weekly` | `Weekly` | Covered。 |
| `monthly` | `Monthly` | Covered。 |
| — | `FreeTokens` | 不是鲸鱼窗口。从 `usage` 和 `accounts[].windows` 里都省略。 |
| — | `Rpm` | 不展示。 |

同一张表同时用于 `usage` 和 `accounts[].windows`。

### Account 对象

| 字段 | 含义 |
|---|---|
| `id` | `qualified_id`（`xkiro/acc-1`）。 |
| `provider` | `[providers.*]` 键。 |
| `label` | 给 CLI 看的名字。 |
| `state` | snake_case 的 `AccountState`：`active`、`draining`、`parked`、`disabled`、`unknown`。 |
| `currency` | 该账号的货币。 |
| `stale` | 该账号任一 gate 过期则为 `true`。 |
| `last_probe_at` | RFC 3339。 |
| `last_error` | 可选。已经打码。 |
| `available_at` | 可选。账号 parked 时出现。 |
| `windows` | 具名对象，不是数组。键与鲸鱼映射相同。没有可用 gate 的键就省略。 |
| `windows.<k>.used` | Decimal 文本。 |
| `windows.<k>.cap` | Decimal 文本。 |
| `windows.<k>.resetsAt` | RFC 3339。没有就省略。 |
| `pots` | 只含计量罐（`Wallet`、`PurchasedCredits`）。没有则为空数组。 |
| `pots[].kind` | snake_case 的 `GateKind`：`wallet`、`purchased_credits`。 |
| `pots[].remaining` | Decimal 文本。 |
| `pots[].used` | Decimal 文本。 |

不要输出 `base_url`、`api_key` 或任何 secret 引用。

## 聚合

顶层 `percent` 是 **库存**：

    round1(100 * sum(used) / sum(cap))

对匹配的、`cap > 0` 的 Covered gate 求和，**包括** parked 和 disabled。
没有 gate ⇒ 不贡献（省略该窗口）。`sum(cap) == 0` ⇒ 省略该窗口。

`resetsAt`：先取该 kind 里已耗尽或 parked 的最早一个；否则取该 kind
里最早的一个；再没有就省略。耗尽指该 kind 的 Covered gate 上
`used >= cap`（剩余为零）。

`balance`：只算 `Metered`。

- `remaining` = `sum(remaining)`
- `used` = `sum(used)`
- `total` = `remaining + used`

不要把 Covered 的剩余加进 `balance`。币种混杂则省略 `balance`。

只有同时满足下面全部条件才输出 `usage`：

1. 至少有一个 `cap > 0` 的 Covered 金钱窗口（`FiveHour` / `Weekly` /
   `Monthly`）。
2. 喂进这次投影的金额只有一种货币。
3. 只有一个 provider，**或者** 参与的每个 provider 在该窗口上货币相同。

未过滤的多 provider / 多币种：省略 `usage`（币种不一致时也省略
`balance`）。保留 `accounts`。

从未探测过的范围返回 **503**，不要给一份空投影。「从未探测」指范围内有账号，
但没有一个成功探测过（没有留下 `last_probe_at` 和 gate）。另一种情况才是
200 且 `stale: true`：最近一次探测失败，但旧的 gate 还在。

disabled 账号仍然贡献它们手里的 gate。全部 disabled 但 gate 还在，应返回 200。

这次请求不要做现场探测。

## 错误

非 200 使用现有的 `error_response` 信封：

```json
{
  "error": {
    "message": "...",
    "type": "<kind>",
    "code": "<kind>"
  }
}
```

200 的 body 里绝不能出现 `error.message`、`ok:false` 或 `success:false`。

| 状态 | 何时 |
|---|---|
| 401 | 缺少或错误的 bearer，或配置了 `local_key` 但读不到。与其它路由同一条 `invalid_api_key` 路径。 |
| 404 | 未知的 `provider`、未知的 `account`，或 `account` 不属于给定的 `provider`。 |
| 400 | `account` 不是限定 id（`provider/account-id`）。裸的 `acc-1` 是 400，不是 404。 |
| 503 | 范围内有账号，但没有一个成功探测过。 |
| 200 | 最近一次探测失败但 gate 还在：`stale: true`。 |
| 200 | 全部账号 disabled，但 gate 还在。 |

不要靠现场探测把 503 救回来。

## 禁止

- 这个 handler 里不要调用 provider。
- 不要用客户端传来的 URL 当探测主机。
- 不要对 `domain::Account` 做 serde。
- 不要编造零（用 `percent: 0` 凑数、把空窗口填满）。
- 不要把 Covered 剩余混进 `balance`。
- 不要把不同币种或不同 provider 混进同一个 `percent`。
- 不要带 cookie，body 里不要有上游账号 key。
- 同一次改动里不要加 `/v1/dashboard/billing/*`。

## 实现草图

- `src/usage.rs`：纯函数 `usage_report`，输入是快照加查询过滤，输出响应 DTO
  （或错误）。
- `ProxyBackend::snapshot()`：最近已知的账号。handler 读内存，不探测。
- `proxy.rs`：`GET /v1/usage` 挂在与 `/v1/models` 同一条 router 上，
  `require_local_key` 后面。
- `accounts[]` 的金额字段是 Decimal **文本**，不是 `f64`。

测试按行为命名：

- `pool_used_percent_is_inventory_not_the_next_account`
- `a_parked_account_still_counts_in_the_pool_percent`
- `covered_remaining_is_not_added_to_balance`
- `mixed_currencies_omit_the_top_level_projection`
- `a_missing_window_is_omitted_not_zero`
- `a_stale_contributor_marks_the_report_stale`
- `never_probed_scope_is_unavailable`
- `account_filter_requires_a_qualified_id`
- `the_report_does_not_contain_an_api_key`

HTTP 测试：要求 `local_key`；200 的 body 没有 `error` 对象。

`apply_probe` 不必改。

## 鲸鱼模板

JS 模板 `light_ai_gateway`，`kind: 'quota'`，`needsBaseUrl`，配额 URL 为
`{base}/v1/usage?provider=xkiro`，窗口为 `rolling` / `weekly` / `monthly`，
`balance` 用同一个 URL，`probeUrl` 为 `{base}/v1/models`。鲸鱼存的凭据是本网关的
`local_key`。

```javascript
const light_ai_gateway = {
  id: 'light_ai_gateway',
  kind: 'quota',
  needsBaseUrl: true,
  auth: 'Bearer {key}',
  probeUrl: '{base}/v1/models',
  quota: {
    url: '{base}/v1/usage?provider=xkiro',
    json: {
      windows: {
        rolling:  { percent: 'usage.rolling.percent',  resetsAt: 'usage.rolling.resetsAt' },
        weekly:   { percent: 'usage.weekly.percent',   resetsAt: 'usage.weekly.resetsAt' },
        monthly:  { percent: 'usage.monthly.percent',  resetsAt: 'usage.monthly.resetsAt' },
      },
    },
  },
  balance: {
    url: '{base}/v1/usage?provider=xkiro',
    json: {
      remaining: 'balance.remaining',
      used: 'balance.used',
      total: 'balance.total',
      currency: 'balance.currency',
    },
  },
};
```

在那份模板出现之前，自定义 HTTP 只能绑定 `balance`。

## 验收

用 `Authorization: Bearer <local_key>` 做 `curl`。七项检查：

1. 三个窗口能渲染（`rolling` / `weekly` / `monthly`）。
2. `balance.remaining` 可用。
3. 同一个池里一个耗尽账号加一个全新账号，显示大约 **50%**，不是 100%
   （库存，不是「下一个账号」）。
4. 过期的贡献者仍保留手里的数字（`stale: true`，没有零）。
5. 没有 bearer 是 401；从未探测过的范围是 503；没有一份 200 body 带 `error`
   对象。
6. 同一范围内，`accounts` 与 `lgw status` 对得上。
7. body 里没有 `api_key` 字段，也没有 `sk-` 机密。

## 以后再说

不属于第一份 PR：

- 持久化 `plan`（今天 `ProbeOutcome.plan` 会被丢掉）。
- OneAPI 账单对（分）。
- 按模型的账本支出。

## 冻结路径

鲸鱼模板一旦发出，下面这些就是公开契约：

- `usage.rolling.percent`
- `usage.*.resetsAt`
- `balance.remaining`

加字段可以。改名就是破坏性变更。
