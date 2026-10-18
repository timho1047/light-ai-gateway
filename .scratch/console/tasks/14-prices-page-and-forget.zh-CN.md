# 14：价格页与 forget 动作

**要构建什么：** 价格页按 provider、model、档位显示配置价与学到的价、观测 mix 与漂移，并支持带确认的 forget（全部、一家 provider、一个 model）。这是受守卫的变更动作，只写账本，完成后页面刷新以显示配置价重新生效。`lgw prices` 共用同一份数据装配，输出字节不变。

**依赖：** 01（把 CLI 拆成按命令的模块）、04（控制台入口壳与最小看板）。

- [ ] 配置价、学到的价、mix、漂移与样本数按 provider、model、档位渲染。
- [ ] forget 一家 provider、一个 model 或全部，都会先确认，执行与 CLI 相同的账本写入，并刷新页面。
- [ ] 该动作要求控制台的 JSON 与自定义头守卫，外部页面无法触发。
- [ ] `lgw prices` 输出字节不变，并且现在与页面共用同一份数据装配。
- [ ] 两种语言渲染同样的数字。
- [ ] `cargo test`、`clippy --all-targets -- -D warnings`、`fmt --check` 通过。
