# Credentials

**English** | [简体中文](credentials.zh-CN.md)

The gateway reads provider keys from the operating system credential store. This
page covers storage, the local key, and the macOS signing behaviour.

Keys go into the operating system's own credential store (macOS Keychain,
Windows Credential Manager, freedesktop Secret Service) via the keyring crate.
Accounts in the config need no key field at all; omitting it looks the account up
by provider/account name.

    lgw keys add <provider> <account>     # hidden prompt, never an argv flag
    lgw keys add <provider> <account> --stdin
    lgw keys list                         # source and status, never the secret
    lgw keys test <provider> <account>    # prove it by probing the provider
    lgw keys remove <provider> <account>

The key is read from a hidden prompt rather than a command-line argument, because
arguments are visible to every process on the machine and land in shell history.

The gateway also has a token of its own: the one credential with no provider behind
it, which clients must send once anything else can reach the port. Any string you
invent works; it is never forwarded upstream:

    lgw keys add local gateway            # hidden prompt, same as any other key
    # then, under [server] in the config:
    # local_key = { keychain = true }

`local/gateway` is the single reserved name that needs no `[providers.*]` entry.
`lgw keys list` shows it next to the provider keys, `lgw doctor` checks that it
resolves, and `lgw keys test local gateway` reports that it is stored.

Explicit alternatives are still supported when you want them:
key = { keychain = true }, key = { env = "X" }, key = { key_file = "/run/secrets/x" },
or an inline literal (discouraged).

## macOS: why it keeps asking for your password

macOS binds a keychain item's access control to the **code signature** of the
program asking for it. `cargo install` produces an *ad-hoc* signature, whose
identity is the hash of the code itself, so every rebuild looks like a different
application to the keychain and an "Always Allow" grant does not survive it.

A grant is also per ITEM, and an older build kept one item per account: a cold start
asked once per account, and once more per command, because each is a new process
reading the same items.

Two things fix it:

    scripts/lgw-identity.sh       # once: creates and trusts the certificate
    scripts/lgw-install.sh        # cargo install, then signs the result

The first makes the grant stick: the access-control entry then names a certificate
that does not change when the code does. The second signs the binary that gets
installed. Both are optional.

There is also only one item to grant: every key lives in a single `vault` entry.
`lgw keys list` shows which item each key came out of.

If you are upgrading from a build that predates the vault, its per-account items
are no longer read: `lgw keys add <provider> <account>` stores the key in the
vault, and the old item can be deleted in Keychain Access.

Note that a plain `cargo install --path .` replaces the signed binary with an
ad-hoc one and brings the prompts back; use `scripts/lgw-install.sh` instead.
Re-signing restores the existing grant.

If you would rather not use the credential store at all:

    [providers.xkiro.accounts.acc-1]
    key = { key_file = "~/.light-ai-gateway/keys/xkiro-acc-1" }   # chmod 600

A 0600 file keeps other users and accidental commits out, but not a backup or a
disk image -- that is what the credential store adds.

## What this actually protects

- **Protected:** other users on the machine, backups and disk images, accidental
  commits, and anything that reads files but not the credential store.
- **NOT protected:** a process already running as you. It can ask the OS for the
  same secret; macOS unlocks the login keychain for the session.

Encrypting a file with a key compiled into the binary would not cover that second
case: whoever has the binary has the key. To move credentials between machines,
export them from the OS store rather than committing a ciphertext.

Set LGW_CREDENTIAL_BACKEND=memory to force an ephemeral store, which is useful
for tests and dry runs and never touches your real keychain.
