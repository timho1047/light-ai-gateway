# 04：控制台入口壳与最小看板

**要构建什么：** 跑 `lgw serve` 时，同一个监听器上也会提供浏览器控制台。打开根地址可以看到控制台外壳：导航、每个 Account 一行的看板（provider、label、状态、Covered 摘要、Metered）、默认浅色并带每浏览器记住的夜间开关、英文与简体中文文案且切换随链接携带。访问只限 loopback，响应带安全头，不设 cookie，启动时记录控制台 URL。

**依赖：** 02（把 HTTP 层从 Gateway 中分离）。

- [ ] 每条控制台路径对非 loopback 对端都回 404，与是否配置 `local_key` 无关。
- [ ] 外来的 `Host`、变更请求上外来的 `Origin`、或非 JSON 的变更 content type 会被拒绝且什么都不改。
- [ ] 响应带限制性的 CSP、禁止嵌入、nosniff 和 no-store；任何情况下都不发 `Access-Control-*` 头。
- [ ] 看板行来自内存里的池子快照，渲染过程不访问 provider。
- [ ] 夜间模式跨导航保持；`?lang=zh` 渲染中文且链接携带参数；不设 cookie。
- [ ] 资产内嵌进二进制，同时提供从磁盘读取的开发覆盖开关。
- [ ] 启动日志在监听地址上记录控制台 URL。
- [ ] `cargo test`、`clippy --all-targets -- -D warnings`、`fmt --check` 通过。
