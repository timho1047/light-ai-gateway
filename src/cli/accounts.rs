//! `lgw accounts`: list, probe, disable, and enable accounts.

use super::status::{cmd_status, render_board};
use super::{AccountsArgs, AccountsCommand};
use crate::ansi::Palette;
use crate::config::Config;
use crate::domain::{AccountOverview, AccountState};
use crate::router::Gateway;
use crate::store::{self, Ledger};
use anyhow::{Context, Result, bail};
use time::OffsetDateTime;

pub(super) async fn cmd_accounts(config: &Config, args: AccountsArgs, pal: &Palette) -> Result<()> {
    match args.command {
        AccountsCommand::List => cmd_status(config, false, pal).await,
        AccountsCommand::Probe { account } => cmd_accounts_probe(config, account, pal).await,
        AccountsCommand::Disable { account } => cmd_accounts_disable(config, &account, pal),
        AccountsCommand::Enable { account } => cmd_accounts_enable(config, &account, pal),
    }
}

async fn cmd_accounts_probe(config: &Config, account: Option<String>, pal: &Palette) -> Result<()> {
    let target_provider = match &account {
        Some(id) => Some(provider_of(config, id)?),
        None => None,
    };

    // Gateway::new probes every account once, then we probe the named one again
    // so a single-account probe is authoritative rather than a cached reading.
    let gateway = Gateway::new(config.clone()).await?;
    if let (Some(provider), Some(id)) = (&target_provider, &account) {
        println!("{}", pal.dim(&format!("probing {provider}/{id} ...")));
        gateway.probe_account(provider, id).await;
    }
    let rows: Vec<AccountOverview> = gateway
        .snapshot()
        .iter()
        .map(|account| account.overview())
        .collect();
    print!("{}", render_board(&rows, OffsetDateTime::now_utc(), pal));
    let _ = gateway.shutdown();
    Ok(())
}

/// Find the provider that owns an account id, refusing to guess when the id is
/// ambiguous or unknown.
fn provider_of(config: &Config, account_id: &str) -> Result<String> {
    let matches: Vec<&str> = config
        .providers
        .iter()
        .filter(|(_, p)| p.accounts.contains_key(account_id))
        .map(|(name, _)| name.as_str())
        .collect();
    match matches.as_slice() {
        [] => bail!(
            "no account {account_id:?} in the configuration (provider account ids are the \
             table keys under [providers.<name>.accounts])"
        ),
        [only] => Ok((*only).to_string()),
        many => bail!(
            "account id {account_id:?} is ambiguous; it exists under providers: {}",
            many.join(", ")
        ),
    }
}

fn cmd_accounts_disable(config: &Config, account_id: &str, pal: &Palette) -> Result<()> {
    let provider = provider_of(config, account_id)?;
    // Deliberately unresolved: disabling an account must work even when its
    // credential is missing, which is exactly when you want to disable it.
    // Target the canonical identity: the same account id can exist under
    // several providers, and disabling the wrong one would silently take a
    // healthy account out of the pool.
    let qualified = crate::domain::qualified_id(&provider, account_id);
    let accounts = config.build_accounts_unresolved()?;
    let mut account = accounts
        .into_iter()
        .find(|a| a.qualified() == qualified)
        .with_context(|| format!("account {qualified} is not in the configuration"))?;

    let db = config.db_path();

    // Overlay what the ledger already knows before writing anything back.
    //
    // A snapshot built from configuration alone carries no gates and state
    // Unknown, and the upsert would then delete the gate rows of EVERY account
    // in the file and reset their state and reset times: disabling one account
    // would quietly erase the quota readings of all the others -- hours of
    // probing, and on Command Code the difference between routing on real
    // numbers and routing blind.
    if let Ok(ledger) = Ledger::open(&db)
        && let Ok(rows) = ledger.accounts()
        && let Some(row) = rows
            .into_iter()
            .find(|r| crate::domain::qualified_id(&r.provider, &r.account_id) == qualified)
    {
        account.gates = row.gates;
        account.available_at = row.available_at;
        account.window_started_at = row.window_started_at;
    }

    // There is no dedicated "operator disabled" transition in the frozen pool
    // API; an account that can never be billed again is exactly the Disabled
    // state that insufficient credit produces, so it is used here and the
    // reason is rewritten to say what really happened.
    account.state = AccountState::Disabled;
    account.last_error = Some(crate::accounts::OPERATOR_DISABLED.to_string());

    let handle = store::StoreHandle::spawn(&db)?;
    // One account, one row. Never a snapshot.
    handle.save_account(&account);
    handle.flush_blocking()?;

    println!(
        "{provider}/{account_id}: {} (persisted to {})",
        pal.red("DISABLED"),
        db.display()
    );
    println!(
        "{}",
        pal.dim(
            "note: a running lgw process keeps its own in-memory pool; restart lgw serve for this to take effect there"
        )
    );
    Ok(())
}

/// Put a disabled account back in the pool.
///
/// The state becomes Unknown rather than Active on purpose: the next probe has
/// to confirm the account can actually serve before it takes traffic, which is
/// the same rule a restart follows. "accounts enable" means "try again", not
/// "trust me" -- and it is the only thing that can undo an operator's disable,
/// because a successful probe restores the credential, not the decision.
fn cmd_accounts_enable(config: &Config, account_id: &str, pal: &Palette) -> Result<()> {
    let provider = provider_of(config, account_id)?;
    let qualified = crate::domain::qualified_id(&provider, account_id);
    let db = config.db_path();

    let accounts = config.build_accounts_unresolved()?;
    let mut account = accounts
        .into_iter()
        .find(|a| a.qualified() == qualified)
        .with_context(|| format!("account {qualified} is not in the configuration"))?;

    if let Ok(ledger) = Ledger::open(&db)
        && let Ok(rows) = ledger.accounts()
        && let Some(row) = rows
            .into_iter()
            .find(|r| crate::domain::qualified_id(&r.provider, &r.account_id) == qualified)
    {
        if row.state != AccountState::Disabled {
            bail!(
                "{qualified} is {}, not DISABLED; there is nothing to enable",
                row.state.label()
            );
        }
        account.gates = row.gates;
        account.available_at = row.available_at;
        account.window_started_at = row.window_started_at;
        account.last_probe_at = None;
    }

    account.state = AccountState::Unknown;
    account.last_error = None;

    let handle = store::StoreHandle::spawn(&db)?;
    handle.save_account(&account);
    handle.flush_blocking()?;

    println!(
        "{provider}/{account_id}: {} (probed before it serves again)",
        pal.green("ENABLED")
    );
    println!(
        "{}",
        pal.dim(
            "note: restart lgw serve for this to take effect there, or run 'lgw accounts probe' to confirm it now"
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_account_ids_are_refused() {
        let config = Config::from_toml_str(
            r#"
[providers.a]
adapter = "none"
base_url = "https://a.test/v1"
[providers.a.accounts.shared]
key = "sk-a"
[providers.b]
adapter = "none"
base_url = "https://b.test/v1"
[providers.b.accounts.shared]
key = "sk-b"
[providers.b.accounts.only-b]
key = "sk-b2"
[routes]
"m" = "a"
"#,
        )
        .unwrap();
        assert_eq!(provider_of(&config, "only-b").unwrap(), "b");
        let err = provider_of(&config, "shared").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "got: {err}");
        let err = provider_of(&config, "nope").unwrap_err().to_string();
        assert!(err.contains("no account"), "got: {err}");
    }
}
