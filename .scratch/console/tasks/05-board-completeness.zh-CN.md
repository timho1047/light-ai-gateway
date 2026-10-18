# 05：看板完整化

**要构建什么：** 看板完整回答运维的第一个问题：每个 Account 的 Covered 窗口 used/cap、Metered 余额、重置倒计时、stale/disabled/parked 细节（含最后错误和返回时间）、每个窗口的池子 inventory、vitals，以及价格漂移摘要。标签页可见时每 10 秒刷新，隐藏时暂停；Refresh 立即重读；新账本渲染「还没探测过」而不是一张空池子表。

**依赖：** 01（把 CLI 拆成按命令的模块）、04（控制台入口壳与最小看板）。

- [ ] 上述每个字段都从内存池子快照渲染，并按 Reading 是否 stale 如实标记。
- [ ] 自动刷新局部更新看板，不整页刷新；隐藏的标签页停止轮询。
- [ ] `lgw status` 输出字节不变，并且现在与看板共用同一份数据装配。
- [ ] 从未探测过的账本和空 tail 渲染解释性文案，不是空表。
- [ ] 两种语言渲染同一组字段和数值。
- [ ] `cargo test`、`clippy --all-targets -- -D warnings`、`fmt --check` 通过。
