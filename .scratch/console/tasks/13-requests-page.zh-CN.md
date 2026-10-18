# 13：请求页

**要构建什么：** 请求页展示最近的尝试：时间、状态、provider、account、model、token、TTFT、TPS、coverage 和 cost；带条数控制、错误与 coverage 过滤，以及空状态。`lgw tail` 共用同一份数据装配，终端输出字节不变。

**依赖：** 01（把 CLI 拆成按命令的模块）、04（控制台入口壳与最小看板）。

- [ ] 尝试级行渲染上述字段，并如实反映错误与重试。
- [ ] 条数、状态与 coverage 控件能过滤行。
- [ ] 空账本渲染解释性文案，不是空表。
- [ ] `lgw tail` 输出字节不变，并且现在与页面共用同一份数据装配。
- [ ] 两种语言渲染同样的行。
- [ ] `cargo test`、`clippy --all-targets -- -D warnings`、`fmt --check` 通过。
