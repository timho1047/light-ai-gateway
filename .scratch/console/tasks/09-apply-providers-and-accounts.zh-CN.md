# 09：应用 provider 与 account 改动

**要构建什么：** provider 与 account 的改动应用到运行中的池子，且不丢工作。新增 Account 以 unknown 进入池子并异步探测；移除 Account 会退役它（不接新工作，在途尝试结算完，账本行保留）；改 base URL、货币、倍率或限额会更新运行中的 Account；改 provider 的 adapter 或 base URL 会把它名下的 Account 重置为 unknown 并重新探测。保留的 Account 保住闸门、读数、粘性绑定、在途占用和限速窗口。

**依赖：** 08（应用 policy 改动到运行中的网关）。

- [ ] 新增 Account 以 unknown 出现，探测成功前不服务任何请求。
- [ ] 移除 Account 立即退出选择；已在途的尝试正常结算，账本行保留。
- [ ] 改 Account 的配置派生字段会改变下一次尝试的行为；粘性绑定和读数保留。
- [ ] 改 provider 的 adapter 或 base URL 会把它的 Account 重置为 unknown 并排队重探。
- [ ] 这些区段应用失败时，池子和文件都不变。
- [ ] provider 与 account 区段在表单中可编辑，并与 policy 走同一条应用路径。
- [ ] `cargo test`、`clippy --all-targets -- -D warnings`、`fmt --check` 通过。
