# 控制台

**状态：** 控制台的契约，由 `src/http/console.rs`、`src/console/` 和 `src/runtime.rs`
来守：谁能到达它、它能改什么、改动何时生效。它不是 `/v1` 的契约；那份不动。

[English](web-ui.md) | **简体中文**

## 它是什么

控制台是 `lgw serve` 提供的运维界面：看板、用量报告、最近的请求、CLI 的每一个动作，
以及 `config.toml` 的编辑器。它由与代理相同的进程、相同的监听器在 `/` 上提供。

外壳会利用宽屏：内容上限 1680px，边距随视口伸缩，表格、列表和看板的两栏都会用上
这份宽度，而不是缩在居中的窄栏里。它是单个运维的工具，不是客户端 API，也不是
多用户管理面板。没有登录、没有账号、没有
cookie、没有会话。能到达它的只有运行网关的这台机器。

## 访问

| 界面 | 路径 | 谁能到达 |
|---|---|---|
| 控制台页面 | `GET /`、`/usage`、`/requests`、`/prices`、`/config`、`/keys`、`/doctor` | 只有 loopback 对端 |
| 内嵌资源 | `GET /console/assets/*` | 只有 loopback 对端 |
| 控制台 API | `/console/api/*` | 只有 loopback 对端 |
| OpenAI 兼容 API | `/v1/*` | 不变：`local_key` bearer 规则 |

对端地址不是 loopback 的请求，从每一条控制台路径都拿到 404，配没配 `local_key` 都一样。
监听地址不是 loopback 时，控制台仍然能从 `127.0.0.1` 进来；它不会被暴露到网络上。

控制台不使用 `server.local_key`，也从不接受上游账号 key。它不在 `/v1` 的 bearer 中间件
后面。

### 同源防护

- 会改状态的控制台请求必须是 `application/json`，并带上 `X-LGW-Console: 1`。
- `Host` 必须是 `127.0.0.1:<port>`、`localhost:<port>` 或 `[::1]:<port>`，端口与
  监听器一致。
- 带 `Origin` 的请求，`Origin` 必须是 loopback。
- 控制台从不发 `Access-Control-*` 头，所以跨源预检会失败，别的页面发来的表单 POST 也
  到不了会改状态的端点。
- 每个控制台响应都带 `Content-Security-Policy: default-src 'self'`、
  `X-Frame-Options: DENY`、`X-Content-Type-Options: nosniff` 和
  `Cache-Control: no-store`。
- 读没有副作用；任何改动都是 POST。

### 响应

页面是服务端渲染的 HTML；看板轮询 `/console/partials/board` 的 HTML 片段。
会改状态的动作返回小的结果载荷给页面自己的脚本消费（草稿校验和应用返回 HTML，账号、
价格、凭据动作返回 JSON）。没有 JSON 管理 API，也没有给第三方客户端用的机器接口。

控制台完全不用 cookie；语言和主题记在浏览器里（URL 参数加 `localStorage`），
不是服务端状态。浏览器不是运维和自己机器之间的
边界，但运维访问的任何页面都能向 loopback 发请求。把它们挡在外面的是 `Origin`/`Host`/
自定义头的三重检查；loopback 对端检查则把其它所有主机挡在外面。

## 与 CLI 对等

CLI 能做的每一件事都在控制台上，除了启动自己、以及写一份全新的示例文件。控制台调用的是
同一批操作；它没有第二份规则实现。

| CLI | 控制台 |
|---|---|
| `lgw status [--refresh]` | 看板；`--refresh` 对应诊断页的 Probe all，以及看板每行的 Probe |
| `lgw usage --since --group-by` | 用量页：范围与分组控件 |
| `lgw accounts list/probe/disable/enable` | 看板账号行：Probe、Disable、Enable |
| `lgw tail --limit` | 请求页：时间、状态、尝试与成本，带条数控制 |
| `lgw doctor` | 诊断页：全部检查 + 双语结论 |
| `lgw keys add/list/remove/test` | 密钥页：来源与状态、Store key、Test、Remove |
| `lgw prices`、`--drift-only`、`--forget` | 价格页：两侧价格与漂移；Forget all/provider/model |
| `lgw init` | 不提供：网关已经有配置 |
| 手动编辑 `config.toml` | 配置页：草稿、校验、diff、应用 |

已知差异都是刻意的：`lgw usage` 在用量表后还会打印价格与漂移面板，控制台把它拆成用量页和
价格页；`lgw tail` 接受任意条数，控制台提供 25/50/100/250；`lgw prices --drift-only`
是过滤，控制台只是把显著漂移排在前面而不是隐藏其余；`lgw serve` 负责启动进程，没有控制台
对应项，因为控制台正是由它提供的。控制台和 CLI 也共用账号状态机、provider 探测适配器
和 doctor 检查装配；只有持久化目标不同 —— 控制台还必须更新运行中的池子，而终端写账本行。

网络动作（探测、诊断、密钥测试）在同一个请求里带进度状态和结果跑完，不是后台任务。

## 配置

`config.toml` 是唯一的事实来源。控制台编辑的就是那个文件，不是副本，也不是数据库：

1. 表单编辑的是一份草稿；在应用之前什么都不写。
2. 应用时用 `toml_edit` 把草稿重写进现有的 TOML 文档，注释、键顺序和未知字段都能留下。
3. 整份文档由每个命令都在用的同一段代码解析、校验；第一个错误会点名出错的字段。
4. 替换用的运行时快照在写任何东西之前就构建好（凭据已解析、探测已准备）。
5. 应用之前会显示 TOML diff。文件以 0600 权限原子替换（同目录下的临时文件，再
   rename），快照在这之后才发布。

表单在屏幕上分成六个区：服务、策略、Provider 与账号、路由与别名、价格，以及
只读的原始 TOML。左侧粘性导航切换当前区，并显示该区未应用的修改数；草稿被拒绝时
会自动切到第一个错误所在的区。六个区共享同一份草稿和同一次应用。没有 JavaScript
时所有区都可见，导航退化为页内跳转链接。

如果表单加载之后文件在磁盘上变了，应用会返回 409 并要求重新加载；它绝不覆盖在编辑器里
做过的改动。校验或构建快照失败的改动，什么都不变。

控制台之外的编辑不会被忽略：watcher 按文件内容轮询，新字节走同一套应用规则和同一条
串行路径。读不出来的文件会保留旧快照，并把失败显示在配置页和日志里；修好文件后无需重启
或控制台操作即可生效。控制台自己写入的文件不会再被应用第二次。

密钥不是可编辑的值。key 字段显示的是它的来源（凭据库、环境变量、文件，或存在内联
字面量）；新增或轮换 key 只会写进操作系统的凭据库。任何响应、日志或页面都不会带上 key。

## 改动对运行中的网关做了什么

一次应用会发布一份新的不可变运行时快照。请求从开始到结算一直用它启动时那份快照，重试
也算在内；之后才开始的请求看到的是新的。没有排空，也没有任何等待。

池子的实时状态会保留下来：闸门、读数、粘性会话绑定、在途占用和限速窗口属于进程，不属于
快照。

| 立即生效 | 需要重启 |
|---|---|
| `[policy]` 的值 | `server.listen` |
| providers：adapter、base URL、货币、限额、metered 开关 | `server.db_path` |
| accounts：新增、移除、标签、限额 | `policy.max_body_bytes` |
| `[routes]` 和 provider 前缀别名 | |
| `[prices]` 的配置值和档位表 | |
| `server.local_key` | |

监听地址与请求体上限字段在表单里是只读的；构造出来的草稿若试图改它们，会按字段名被拒绝。
新账号以 `unknown` 进入，探测成功之前不能服务。被移除的账号会退役：它不再接新工作，最后
几笔在途尝试仍对着它进来时那个池子结算。改一家 provider 的 adapter 或 base URL 会把它
的账号重置成 `unknown` 并重新探测；来自旧端点的读数绝不保留。

校验失败的应用什么都不变：旧快照继续服务，控制台返回错误。

## 用量数据

看板是实时池子：它读进程内存，从不探测 provider。用量页通过只读连接读账本，从不写入。

- 范围：24 小时、7 天、30 天、90 天、全部。
- 金额保持账本的 Decimal 精度；控制台从不把它当浮点数重新相加。
- 尝试级明细在 30 天后被清理，所以延迟数字（TTFT p50/p95、TPS）到 30 天为止；花销、
  token 和请求数不会。
- 看板每 10 秒重新取一次自己的片段；其它页面在加载时和动作后刷新。没有 websocket，
  也没有流式 tail。

## 语言

文案有英文和简体中文。`?lang=zh` 选择中文，默认是英文；链接会带上这个参数，顶栏的切换
也能改它。切换会把选择记进 `localStorage`（绝不是 cookie）；主题（明亮/暗色，默认
明亮）也用同样的方式记忆。主题和语言在同一个两段式控件里：两者都显示当前状态，
并在可访问名称里说明点击后会变成什么。

## 禁止

- 控制台任何路径都不回应非 loopback 对端。
- 没有 cookie、会话或登录。
- 永远不发 `Access-Control-*` 头。
- 页面、API 响应或日志里没有凭据值。
- 不改 `/v1/*` 的形状、顺序或鉴权。
- 不写没有经过整体校验的 `config.toml`。
- 读端点不做现场探测。
- 不出现只让一半新配置生效的应用。
- CLI 同步跑的动作不变成后台任务。

## 测试

集成测试，按它们断言的行为命名：

- `a_foreign_host_does_not_see_the_console`
- `the_console_is_not_behind_the_local_key`
- `console_writes_need_json_and_the_console_header`
- `a_stale_apply_leaves_the_newer_file_untouched`
- `an_invalid_draft_is_a_field_error_in_both_languages`
- `a_draft_keeps_comments_order_and_unknown_fields`
- `a_removed_account_retires_after_its_in_flight_attempt_settles`
- `a_published_snapshot_does_not_reprice_an_in_flight_request`
- `a_restart_only_apply_is_refused_by_name_and_writes_nothing`
- `an_invalid_apply_changes_neither_the_file_nor_the_snapshot`
- `an_external_edit_is_applied_once_through_the_watcher`
- `an_invalid_external_edit_keeps_serving_and_surfaces_the_failure`
- `a_console_save_is_not_applied_again_by_the_watcher`
- `the_usage_page_shows_the_same_numbers_in_both_languages`
- `the_requests_page_shows_attempts_and_filters`
- `the_prices_page_shows_both_rates_and_forgets_them`
- `the_doctor_page_reports_every_check_in_both_languages`
- `the_form_is_split_into_linked_panels`
- `theme_and_language_are_one_consistent_control_group`
- `the_keys_page_lists_sources_and_manages_the_store`
- `probing_and_operator_state_work_through_the_console`
