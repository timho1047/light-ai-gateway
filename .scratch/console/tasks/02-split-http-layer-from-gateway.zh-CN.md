# 02：把 HTTP 层从 Gateway 中分离

**要构建什么：** 同样的 `/v1` 路由和行为，但 HTTP 层与 Gateway 分开：一个模块负责装配 router、认证中间件、body limit 和 listener；代理 handler、冻结的用量 handler 和服务装配各在本模块。Gateway 继续持有池子、价格、探测、账本写入者和区间账本。这是准备，不是行为改动：现有集成测试随代码一起移动。

**依赖：** 无（可立即开始）。

- [ ] 应用暴露的路由、认证响应、body limit 和错误信封与之前完全一致。
- [ ] 现有 HTTP 集成测试是移动而不是重写，并且通过。
- [ ] 以后加一条路由只需改一个装配点，而不是改代理 handler 模块。
- [ ] `cargo test`、`clippy --all-targets -- -D warnings`、`fmt --check` 通过。
