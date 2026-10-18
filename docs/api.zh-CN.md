# HTTP API

[English](api.md) | **简体中文**

网关对外提供的代理接口。 /v1/usage 的完整契约见 [usage-endpoint.zh-CN.md](usage-endpoint.zh-CN.md)。

## 端点

    GET  /                        运维控制台（只绑 loopback）
    POST /v1/chat/completions     代理：重试、换号、记账
    GET  /v1/models               本网关会服务的模型，由 provider 描述
    GET  /v1/usage                池子的额度用掉了多少、什么时候回来

## 认证

/v1 端点使用 server.local_key 配置的 bearer token。设置后客户端必须发送 Authorization: Bearer <token>；这个 token 不会被转发给上游。控制台不走这层认证，它只允许 loopback peer 访问。OpenAI 客户端应指向 http://127.0.0.1:8787/，也就是控制台地址。

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

`/v1/usage` 是给面板用的：把轮询器最后看到的读数换算成百分比。它不碰 provider，所以挂件
按秒刷新也不花钱。窗口是有名字的（`rolling`、`weekly`、`monthly`），不是数组下标，只会
读 JSON 路径的客户端也取得到；没有读数的窗口直接不出现，而不是给个 `0`：

    curl -H "Authorization: Bearer $KEY" http://127.0.0.1:8787/v1/usage
    curl -H "Authorization: Bearer $KEY" "http://127.0.0.1:8787/v1/usage?provider=xkiro"
    curl -H "Authorization: Bearer $KEY" "http://127.0.0.1:8787/v1/usage?account=xkiro/acc-1"

百分比算的是整个范围的存货 —— sum(used) 除以 sum(cap)，parked 的账号也算在分母里 ——
所以一个用光的账号配一个全新的账号，读数是 50%，不是 0%。`usage` 和 `balance` 里是
JSON 数字，方便直接画；`accounts[]` 里同样的数保留精确小数，是文本，方便对账。包月的额度
不会进 `balance`：订阅里剩下的份额不是你能拿去别处花的钱。一次都没探过的范围返回 503 ——
一堆 0 和「额度还没动过」长得一模一样。完整结构见
[usage-endpoint.zh-CN.md](usage-endpoint.zh-CN.md)。
