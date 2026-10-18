# 凭据

[English](credentials.md) | **简体中文**

网关从操作系统凭据库读取 provider key。这里说明存储方式、local key，以及 macOS 上的签名行为。

密钥存进操作系统自带的凭据库（macOS 钥匙串、Windows Credential Manager、freedesktop
Secret Service），走的是 keyring crate。配置里的账号一个 key 字段都不用写；不写就是按
provider/account 名字去查。

    lgw keys add <provider> <account>     # 隐藏输入，绝不作为命令行参数
    lgw keys add <provider> <account> --stdin
    lgw keys list                         # 显示来源和状态，从不显示密钥
    lgw keys test <provider> <account>    # 真的向上游探一次
    lgw keys remove <provider> <account>

密钥走隐藏输入，不作为命令行参数 —— 参数对本机任何进程都可见，还会留在 shell 历史里。

网关自己也有一把 token，它是唯一没有 provider 的凭据：一旦有别的东西能够到这个端口，
客户端就必须带上它。随便编一个字符串即可，它只用于比对，绝不发往上游：

    lgw keys add local gateway            # 隐藏输入，和其它 key 一样
    # 然后在配置的 [server] 下加一行：
    # local_key = { keychain = true }

`local/gateway` 是唯一的保留名，不需要 `[providers.*]` 条目。`lgw keys list` 会把它和
provider 的 key 列在一起；`lgw doctor` 会真的去解析它；`lgw keys test local gateway`
报告它已存储。

想显式指定也可以：`key = { keychain = true }`、`key = { env = "X" }`、
`key = { key_file = "/run/secrets/x" }`，或者内联字面量（不推荐）。

## macOS 为什么它老是问你要密码

macOS 把钥匙串条目的访问控制绑在**调用方的代码签名**上。`cargo install` 产出的是
*ad-hoc* 签名，身份就是代码本身的哈希；于是每次重新编译，在钥匙串眼里都算换了一个应用，
「Always Allow」这种授权活不过一次重建。

授权**按条目**给，而旧版本每个账号一个条目：冷启动按账号各弹一次，每条命令还会再各弹
一次 —— 它们各自是新进程，读的是同一批条目。

两招解决：

    scripts/lgw-identity.sh       # 一次性：创建并信任证书
    scripts/lgw-install.sh        # cargo install，然后给产物签名

第一招让授权留得住：访问控制条目从此指向一张不随代码变化的证书。第二招给真正装上去的
二进制签名。两招都可选。

另一个变化是只剩一个条目需要授权：所有 key 都住在唯一的 `vault` 条目里。`lgw keys list`
会显示每个 key 来自哪个条目。

如果你是从早于 vault 的版本升上来的：那些按账号分开的旧条目不再被读取。重新
`lgw keys add <provider> <account>` 把密钥存进 vault，旧条目可以在「钥匙串访问」里删掉。

注意：直接 `cargo install --path .` 会用 ad-hoc 签名覆盖掉已签名的二进制，弹窗又回来了；
请用 `scripts/lgw-install.sh`。重新签名会恢复原有授权。

如果你干脆不想用凭据库：

    [providers.xkiro.accounts.acc-1]
    key = { key_file = "~/.light-ai-gateway/keys/xkiro-acc-1" }   # chmod 600

0600 的文件能挡住同机其他用户和误提交，但挡不住备份和磁盘镜像 —— 那正是凭据库多出来的
那一件事。

## 这套东西到底保护了什么

- **挡得住：** 机器上的其他用户、备份和磁盘镜像、误提交，以及任何能读文件但读不到凭据库
  的东西。
- **挡不住：** 一个已经以你的身份跑起来的进程。它能直接向系统要同一个密钥；macOS 在会话
  期间会把登录钥匙串解锁。

把密钥编译进二进制再拿去加密文件，挡不住第二种情况：谁拿到二进制，谁就拿到密钥。需要让
凭据在多台机器之间流转，请从系统凭据库里导出，而不是提交一段密文。

设 `LGW_CREDENTIAL_BACKEND=memory` 可以强制使用临时存储，测试和干跑很有用，也绝不碰你
真实的钥匙串。
