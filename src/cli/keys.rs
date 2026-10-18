//! `lgw keys`: register, list, test, and remove provider credentials.

use super::probe_client;
use super::status::board_amount;
use super::{KeysArgs, KeysCommand};
use crate::ansi::Palette;
use crate::config::Config;
use crate::domain::{Gate, GateClass, GateKind};
use crate::secrets::{CredentialStore, LOCAL_KEY_ACCOUNT, LOCAL_KEY_PROVIDER};
use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;

pub(super) async fn cmd_keys(
    config: &Config,
    creds: &CredentialStore,
    args: KeysArgs,
    pal: &Palette,
) -> Result<()> {
    match args.command {
        KeysCommand::Add {
            provider,
            account,
            stdin,
        } => cmd_keys_add(config, creds, &provider, &account, stdin, pal),
        KeysCommand::List => cmd_keys_list(config, creds, pal),
        KeysCommand::Remove { provider, account } => cmd_keys_remove(creds, &provider, &account),
        KeysCommand::Test { provider, account } => {
            cmd_keys_test(config, creds, &provider, &account, pal).await
        }
    }
}

/// Read the secret from a hidden prompt, or from stdin when asked.
///
/// Deliberately not a command-line flag: an argument is visible to every process
/// on the machine via ps, and is written to shell history.
fn read_secret(provider: &str, account: &str, from_stdin: bool) -> Result<String> {
    let secret = if from_stdin {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading the key from stdin")?;
        line
    } else {
        rpassword::prompt_password(format!("API key for {provider}/{account} (input hidden): "))
            .context("reading the key from the terminal")?
    };
    let trimmed = secret.trim().to_string();
    if trimmed.is_empty() {
        bail!("no key was entered");
    }
    Ok(trimmed)
}

/// The shared validator owns the rule; the terminal keeps the wording.
fn keys_add_target(config: &Config, provider: &str, account: &str) -> Result<()> {
    crate::keys::add_target(config, provider, account)
}

fn cmd_keys_add(
    config: &Config,
    creds: &CredentialStore,
    provider: &str,
    account: &str,
    from_stdin: bool,
    pal: &Palette,
) -> Result<()> {
    keys_add_target(config, provider, account)?;

    let secret = read_secret(provider, account, from_stdin)?;
    crate::keys::store(config, creds, provider, account, &secret)?;
    println!(
        "{} {provider}/{account} in {} ({})",
        pal.green("stored"),
        creds.backend_name(),
        crate::secrets::describe_secret(&secret)
    );
    if provider == LOCAL_KEY_PROVIDER && account == LOCAL_KEY_ACCOUNT {
        // The command doctor prints has to end somewhere useful: storing the
        // token is half of it, the config line and the restart are the other
        // half.
        println!(
            "{}",
            pal.dim(
                "next: set local_key = { keychain = true } under [server], then restart lgw serve"
            )
        );
    }
    Ok(())
}

/// The shared report owns the rows; the terminal owns the table.
fn credential_rows(config: &Config, creds: &CredentialStore) -> Vec<crate::keys::CredentialRow> {
    crate::keys::rows(config, creds)
}

fn cmd_keys_list(config: &Config, creds: &CredentialStore, pal: &Palette) -> Result<()> {
    println!("backend: {}", creds.backend_name());
    let rows = credential_rows(config, creds);
    if creds.is_vaulted() {
        println!("vault   : every key in one credential-store item");
    }
    println!();
    let mut table = crate::table::Table::new(&[
        ("PROVIDER", crate::table::Align::Left),
        ("ACCOUNT", crate::table::Align::Left),
        ("SOURCE", crate::table::Align::Left),
        ("STATUS", crate::table::Align::Left),
    ]);
    for row in &rows {
        // A credential that resolves is the quiet case; one that does not is the
        // reason anyone ran this command.
        let status = if row.resolves {
            pal.green(&row.status)
        } else {
            pal.red(&row.status)
        };
        table.push(vec![
            row.provider.clone(),
            row.account.clone(),
            row.source.clone(),
            status,
        ]);
    }
    print!("{}", table.render(pal));
    println!();
    println!(
        "{}",
        pal.dim("Register one with: lgw keys add <provider> <account>")
    );
    Ok(())
}

fn cmd_keys_remove(creds: &CredentialStore, provider: &str, account: &str) -> Result<()> {
    if crate::keys::remove(creds, provider, account)? {
        println!("removed {provider}/{account} from {}", creds.backend_name());
    } else {
        println!("nothing stored for {provider}/{account}");
    }
    Ok(())
}

/// Whether the gateway's own access token is stored, as its length in characters.
///
/// The length alone is enough: it catches a truncated paste, and it is the only
/// fact about that token a report may print.
fn local_token_status(creds: &CredentialStore) -> Result<Option<usize>> {
    crate::keys::local_token_status(creds)
}

/// Resolve the stored credential and prove it works against the provider.
///
/// The account model, credential resolution, client policy and bounded probe
/// all live in keys::probe; this function only prints the outcome in the
/// terminal's established layout.
async fn cmd_keys_test(
    config: &Config,
    creds: &CredentialStore,
    provider: &str,
    account: &str,
    pal: &Palette,
) -> Result<()> {
    if provider == LOCAL_KEY_PROVIDER && account == LOCAL_KEY_ACCOUNT {
        return match local_token_status(creds)? {
            Some(length) => {
                println!(
                    "{provider}/{account}: {} - stored ({length} chars); this is the token                      clients must send in Authorization",
                    pal.green("ok")
                );
                Ok(())
            }
            None => bail!("no local gateway token is stored; run: lgw keys add local gateway"),
        };
    }

    let client = probe_client(config)?;
    match crate::keys::probe(
        config,
        creds,
        provider,
        account,
        &client,
        std::time::Duration::from_secs(15),
    )
    .await
    {
        Ok(crate::keys::KeyTestOutcome::LocalToken { chars }) => {
            println!(
                "{provider}/{account}: {} - stored ({chars} chars); this is the token                  clients must send in Authorization",
                pal.green("ok")
            );
            Ok(())
        }
        Ok(crate::keys::KeyTestOutcome::NoProbe { chars }) => {
            println!(
                "{provider}/{account}: credential found ({chars} chars), but this provider                  has no quota probe to test against"
            );
            Ok(())
        }
        Ok(crate::keys::KeyTestOutcome::Probed {
            probe,
            gates,
            currency,
        }) => {
            println!(
                "{provider}/{account}: {} - {probe} credentials accepted, {} gates read",
                pal.green("ok"),
                gates.len()
            );
            println!("{}", gate_reading(&gates, &currency, pal));
            Ok(())
        }
        Err(error) => {
            println!("{provider}/{account}: {} - {error}", pal.red("FAILED"));
            bail!("the stored credential was rejected or the probe endpoint is unreachable")
        }
    }
}

/// Every gate the probe read, one per line, in the shape the board uses.
///
/// The unit is a column rather than an assumption: a free-token allowance is
/// counted in tokens and an rpm gate in requests per minute, so a raw 30000000
/// sitting where a balance goes is how an allowance gets read as money. The class
/// is spelled out in the same words as the board's columns rather than as a Rust
/// enum name.
fn gate_reading(gates: &[Gate], currency: &str, pal: &Palette) -> String {
    use crate::table::{Align, Table};
    use rust_decimal::prelude::ToPrimitive;

    if gates.is_empty() {
        return "    no gates in the response".to_string();
    }
    let mut table = Table::new(&[
        ("GATE", Align::Left),
        ("USED", Align::Right),
        ("CAP", Align::Right),
        ("UNIT", Align::Left),
        ("POT", Align::Left),
    ]);
    for g in gates {
        // A token allowance is read at the scale it is spent at, exactly as the
        // usage tables read it; everything else is money and keeps its decimals.
        let amount = |d: Decimal| match g.kind {
            GateKind::FreeTokens => crate::table::tokens(d.to_u64().unwrap_or_default()),
            _ => board_amount(d),
        };
        table.push(vec![
            g.kind.label().to_string(),
            amount(g.used),
            amount(g.cap),
            gate_unit(g.kind, currency).to_string(),
            class_label(g.class()).to_string(),
        ]);
    }
    table
        .render(pal)
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a gate is counted in.
fn gate_unit(kind: GateKind, currency: &str) -> &str {
    match kind {
        GateKind::FreeTokens => "tokens",
        GateKind::Rpm => "req/min",
        _ => currency,
    }
}

/// A gate class in the words the board uses for its two money columns.
fn class_label(class: GateClass) -> &'static str {
    match class {
        GateClass::Covered => "covered",
        GateClass::Metered => "metered",
        GateClass::TokenAllowance => "token allowance",
        GateClass::RateLimit => "rate limit",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_helpers::{gate, now};
    use crate::domain::Confidence;

    /// A config exercising every credential source except the real keychain.
    /// Uses a key_file rather than an environment variable: mutating process-wide
    /// env in a test is racy across threads and needs unsafe in edition 2024.
    /// A whole command, driven end to end rather than through the renderer it
    /// calls.
    ///
    /// cmd_keys_list takes its Config and its CredentialStore now, so it runs
    /// against an in-memory store instead of the operator's keychain.
    #[test]
    fn a_command_runs_against_an_injected_credential_store() {
        let config = keys_config("secret");
        cmd_keys_list(&config, &CredentialStore::memory(), &Palette::off())
            .expect("listing keys needs no keychain");
    }

    /// The command `lgw doctor` prints has to work: the gateway's own token is the
    /// one credential with no provider behind it.
    #[test]
    fn the_gateway_token_is_the_one_key_that_needs_no_provider() {
        let config = keys_config("/tmp/lgw-unused-key-file");

        assert!(
            keys_add_target(&config, LOCAL_KEY_PROVIDER, LOCAL_KEY_ACCOUNT).is_ok(),
            "doctor tells operators to run exactly this"
        );
        // Anything else under that reserved name is a typo, not a provider.
        let err = keys_add_target(&config, LOCAL_KEY_PROVIDER, "gateway-2")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved"), "{err}");

        assert!(keys_add_target(&config, "nope", "acc").is_err());
        assert!(keys_add_target(&config, "xkiro", "from-store").is_ok());
        let err = keys_add_target(&config, "xkiro", "nope")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown account"), "{err}");
    }

    /// The token is listed once the config says to look for it in the store, and
    /// its absence is reported as the command that fixes it.
    #[test]
    fn the_gateway_token_is_listed_when_the_config_asks_the_store_for_it() {
        let path = temp_key_file("sk-unused");
        let config = Config::from_toml_str(&format!(
            r#"
[server]
local_key = {{ keychain = true }}

[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"

[providers.xkiro.accounts.from-store]
key = {{ key_file = "{}" }}

[routes]
"m" = "xkiro"
"#,
            path.to_str().unwrap()
        ))
        .unwrap();
        let creds = CredentialStore::memory();

        let rows = credential_rows(&config, &creds);
        let local = rows
            .iter()
            .find(|r| r.provider == LOCAL_KEY_PROVIDER)
            .expect("the gateway's own token is part of the credential report");
        assert_eq!(local.account, LOCAL_KEY_ACCOUNT);
        assert!(
            local.source.starts_with("credential-store"),
            "{}",
            local.source
        );
        assert!(!local.resolves);
        assert!(
            local.status.contains("lgw keys add local gateway"),
            "the status names the fix: {}",
            local.status
        );

        creds
            .set(LOCAL_KEY_PROVIDER, LOCAL_KEY_ACCOUNT, "sk-local-token")
            .unwrap();
        let rows = credential_rows(&config, &creds);
        let local = rows
            .iter()
            .find(|r| r.provider == LOCAL_KEY_PROVIDER)
            .unwrap();
        assert!(local.resolves, "{}", local.status);
        assert_eq!(local.status, "ok");
        let _ = std::fs::remove_file(&path);
    }

    /// Nothing that reports a credential may print it, and this one reports only
    /// the length that catches a truncated paste.
    #[test]
    fn the_gateway_token_status_is_its_length_and_nothing_else() {
        let creds = CredentialStore::memory();
        assert_eq!(local_token_status(&creds).unwrap(), None);

        creds
            .set(LOCAL_KEY_PROVIDER, LOCAL_KEY_ACCOUNT, "sk-local-token")
            .unwrap();
        assert_eq!(
            local_token_status(&creds).unwrap(),
            Some("sk-local-token".len())
        );
    }

    #[test]
    fn credential_rows_report_source_and_status_without_the_secret() {
        let path = temp_key_file("sk-envvalue");
        let config = keys_config(path.to_str().unwrap());
        let creds = CredentialStore::memory();
        creds.set("xkiro", "from-store", "sk-abcdefghij").unwrap();

        let rows = credential_rows(&config, &creds);
        assert_eq!(rows.len(), 2);

        let stored = rows.iter().find(|r| r.account == "from-store").unwrap();
        assert_eq!(stored.source, "credential-store");
        assert_eq!(stored.status, "ok");

        let file = rows.iter().find(|r| r.account == "from-file").unwrap();
        assert!(file.source.starts_with("file:"), "got {}", file.source);
        assert_eq!(file.status, "ok");

        // The rendered output must never contain either secret.
        let rendered = format!("{rows:?}");
        assert!(!rendered.contains("sk-abcdefghij"));
        assert!(!rendered.contains("sk-envvalue"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_credential_is_reported_as_missing_not_as_a_pass() {
        let path = temp_key_file("unused");
        let config = keys_config(path.to_str().unwrap());
        let creds = CredentialStore::memory();
        let rows = credential_rows(&config, &creds);
        let stored = rows.iter().find(|r| r.account == "from-store").unwrap();
        assert!(
            stored.status.starts_with("MISSING"),
            "got {}",
            stored.status
        );
        // And it names the exact command that fixes it.
        assert!(stored.status.contains("lgw keys add xkiro from-store"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn keys_add_rejects_an_unknown_provider_before_reading_a_secret() {
        let config = keys_config("/nonexistent");
        let creds = CredentialStore::memory();
        // from_stdin is irrelevant: validation must fail first.
        let err = cmd_keys_add(&config, &creds, "nope", "acc", true, &Palette::off()).unwrap_err();
        assert!(err.to_string().contains("unknown provider"), "got: {err}");
    }

    #[test]
    fn keys_add_rejects_an_unknown_account_and_lists_the_known_ones() {
        let config = keys_config("/nonexistent");
        let creds = CredentialStore::memory();
        let err =
            cmd_keys_add(&config, &creds, "xkiro", "nope", true, &Palette::off()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown account"), "got: {msg}");
        assert!(
            msg.contains("from-store"),
            "should list known accounts: {msg}"
        );
    }

    #[test]
    fn keys_remove_is_idempotent() {
        let creds = CredentialStore::memory();
        creds.set("xkiro", "acc", "sk-x").unwrap();
        assert!(cmd_keys_remove(&creds, "xkiro", "acc").is_ok());
        assert!(cmd_keys_remove(&creds, "xkiro", "acc").is_ok());
        assert!(creds.get("xkiro", "acc").unwrap().is_none());
    }

    /// `lgw keys test` prints one row per gate. The unit column is the point: a
    /// free-token allowance and a balance are not the same kind of number, and a
    /// raw 30000000 next to a currency code is how one gets read as the other.
    #[test]
    fn a_gate_reading_names_its_unit_and_keeps_its_columns_straight() {
        let t = now();
        let gates = vec![
            gate(
                GateKind::FiveHour,
                "100",
                "40",
                None,
                Confidence::Exact,
                false,
                t,
            ),
            gate(
                GateKind::FreeTokens,
                "30000000",
                "368",
                None,
                Confidence::Exact,
                false,
                t,
            ),
            gate(
                GateKind::Rpm,
                "600",
                "12",
                None,
                Confidence::Exact,
                false,
                t,
            ),
            gate(
                GateKind::Wallet,
                "12.34",
                "0",
                None,
                Confidence::Exact,
                false,
                t,
            ),
        ];
        let out = gate_reading(&gates, "USD", &Palette::off());
        assert!(out.contains("30.0M"), "a token allowance is scaled: {out}");
        assert!(out.contains("tokens"), "{out}");
        assert!(out.contains("req/min"), "{out}");
        assert!(!out.contains("Metered"), "no Rust enum names: {out}");
        assert!(!out.contains("RateLimit"), "no Rust enum names: {out}");

        let pot = |line: &str| {
            ["covered", "metered", "token allowance", "rate limit"]
                .iter()
                .filter_map(|word| line.find(*word))
                .next()
                .expect("every row names its pot")
        };
        let columns: Vec<usize> = out.lines().skip(1).map(pot).collect();
        assert_eq!(columns.len(), gates.len(), "{out}");
        assert!(
            columns.iter().all(|c| *c == columns[0]),
            "the pot column drifted: {out}"
        );
    }

    fn keys_config(key_file: &str) -> Config {
        Config::from_toml_str(&format!(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"
[providers.xkiro.accounts.from-store]
[providers.xkiro.accounts.from-file]
key = {{ key_file = "{key_file}" }}
[routes]
"m" = "xkiro"
"#
        ))
        .unwrap()
    }

    fn temp_key_file(contents: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("lgw-key-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&p, contents).unwrap();
        p
    }
}
