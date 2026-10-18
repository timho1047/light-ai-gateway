//! `lgw doctor`: check config, credentials, and probe reachability.

use super::{ConfigOrigin, probe_client};
use crate::ansi::Palette;
use crate::config::{Config, QuotaAdapter, SecretRef};
use crate::quota::probe_for;
use crate::secrets::{CredentialStore, LOCAL_KEY_ACCOUNT, LOCAL_KEY_PROVIDER};
use crate::store;
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// Whether a listen address is confined to this machine.
///
/// The address is a string in the config, so this has to be a textual check:
/// anything that does not name the loopback interface is treated as reachable
/// by others, which is the safe direction.
fn listens_on_loopback(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    matches!(host, "127.0.0.1" | "localhost" | "::1") || host.starts_with("127.")
}

/// A price entry that no route can reach.
struct UnreachablePrice {
    provider: String,
    model: String,
    /// Configured routes whose name is close to it: the likely typo.
    closest: Vec<String>,
}

/// Price entries no route points at.
///
/// The price table is keyed by the model id the PROVIDER sees, which is the route
/// name. An entry whose model no route maps to that provider is dead config --
/// and worse than dead: the route it was written for is priced by the conservative
/// default, so real traffic is sized at 15/75 per million instead of the rate
/// card. That is exactly how a wrong vendor prefix in [routes] hides, which is why
/// doctor says it out loud.
fn unreachable_prices(config: &Config) -> Vec<UnreachablePrice> {
    let mut out = Vec::new();
    for (provider, models) in &config.prices {
        for model in models.keys() {
            if config.routes.get(model).is_some_and(|p| p == provider) {
                continue;
            }
            out.push(UnreachablePrice {
                provider: provider.clone(),
                model: model.clone(),
                closest: crate::closest_names(config.routes.keys().map(String::as_str), model, 3)
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            });
        }
    }
    out
}

/// The configured routes, grouped by provider and ordered by model name.
fn routes_by_provider(config: &Config) -> BTreeMap<&str, Vec<&str>> {
    let mut out: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (model, provider) in &config.routes {
        out.entry(provider.as_str()).or_default().push(model);
    }
    out
}

pub(super) async fn cmd_doctor(
    path: &Path,
    origin: ConfigOrigin,
    pal: &Palette,
    creds: &CredentialStore,
) -> Result<()> {
    let mut problems = 0u32;
    println!("config path : {} (from {})", path.display(), origin.label());

    let config = match Config::load(path) {
        Ok(c) => {
            println!("config file : {}", pal.green("parsed ok"));
            c
        }
        Err(e) => {
            println!("config file : {}", pal.red("FAILED"));
            println!("              {e:#}");
            bail!("configuration is unusable; nothing else could be checked");
        }
    };

    let accounts: usize = config.providers.values().map(|p| p.accounts.len()).sum();
    println!(
        "providers   : {} ({} accounts, {} routes)",
        config.providers.len(),
        accounts,
        config.routes.len()
    );
    println!(
        "policy      : soft={:.2} allow_metered={} drift={:.2} probe_before_switch={} \
         max_retries={}",
        config.policy.soft_threshold,
        config.policy.allow_metered,
        config.policy.drift_threshold,
        config.policy.probe_before_switch,
        config.policy.max_retries
    );
    if config.policy.allow_metered {
        println!(
            "              {}",
            pal.yellow(
                "WARNING: allow_metered is on; requests may silently spend wallet or purchased credits"
            )
        );
    }
    // A provider that answers for itself is the reason the line above can be
    // off while real money is still being spent.
    for (name, p) in &config.providers {
        if let Some(on) = p.allow_metered {
            println!(
                "              provider {name}: allow_metered={} (overrides [policy])",
                if on {
                    pal.yellow("on")
                } else {
                    pal.green("off")
                }
            );
        }
    }

    // Offline, and about the config rather than a credential: a price the routes
    // cannot reach, which is how a mistyped route name hides.
    let unreachable = unreachable_prices(&config);
    if config.prices.values().all(|models| models.is_empty()) {
        println!("prices      : none configured; every route is sized at the conservative default");
    } else if unreachable.is_empty() {
        println!(
            "prices      : {}",
            pal.green("every entry is reachable from a route")
        );
    } else {
        println!("prices      :");
        for entry in &unreachable {
            let hint = if entry.closest.is_empty() {
                String::new()
            } else {
                format!(" - did you mean {:?}?", entry.closest.join("\" or \""))
            };
            println!(
                "  {}: no route sends this model to {}{hint}",
                pal.red(&format!("[prices.{}.{:?}]", entry.provider, entry.model)),
                entry.provider
            );
            problems += 1;
        }
    }

    let db = config.db_path();
    println!(
        "database    : {} ({})",
        db.display(),
        if store::db_exists(&db) {
            "present"
        } else {
            "not created yet"
        }
    );

    println!("credentials :");
    // Resolve every credential ONCE and reuse the result below.
    //
    // The account list used to be built twice -- once here to report, once in
    // the probe section -- so every key was read twice. On macOS each read of an
    // item the app is not already trusted for is another authorisation prompt,
    // which made "lgw doctor" ask twice per account for a read-only check.
    let (usable, unusable) = config.build_accounts_lenient(creds)?;
    for (provider, p) in &config.providers {
        if p.accounts.is_empty() {
            println!("  {provider}: no accounts configured");
            problems += 1;
        }
        for (id, a) in &p.accounts {
            let secret_ref = a.key.clone().unwrap_or(SecretRef::Keychain);
            let source = secret_ref.describe();
            // The key itself is never printed; ApiKey redacts Debug/Display and
            // this path never formats it at all.
            let qualified = crate::domain::qualified_id(provider, id);
            if usable.iter().any(|acct| acct.qualified() == qualified) {
                println!("  {provider}/{id}: {} ({source})", pal.green("ok"));
                continue;
            }
            let reason = unusable
                .iter()
                .find(|acct| acct.qualified() == qualified)
                .and_then(|acct| acct.last_error.clone())
                .unwrap_or_else(|| "could not be resolved".to_string());
            println!(
                "  {provider}/{id}: {} ({source}) - {reason}",
                pal.red("MISSING")
            );
            problems += 1;
        }
    }

    println!("probes      :");
    let client = probe_client(&config)?;

    let mut targets: HashMap<String, (QuotaAdapter, String)> = HashMap::new();
    for (provider, adapter, base) in config.probe_targets() {
        targets.insert(provider, (adapter, base));
    }

    // Doctor is deliberately strict: it exists to report problems.
    // The accounts were already built above, so this reuses them rather than
    // reading every credential a second time.
    if usable.is_empty() {
        println!("  skipped: no account has a usable credential");
        println!("  (fix the missing credentials above, then re-run doctor)");
    } else {
        {
            for account in &usable {
                let label = format!("{}/{}", account.provider, account.id);
                match targets.get(&account.provider) {
                    None => println!("  {label}: no probe target configured"),
                    Some((QuotaAdapter::None, _)) => {
                        // There is no supported "local ledger only" mode: an
                        // account that is never probed never leaves Unknown, and
                        // Unknown is not selectable. Saying "local ledger only"
                        // made a dead account look like a configured one.
                        println!(
                            "  {label}: {} - adapter = none has no quota source, so this \
                             account can never leave the UNKNOWN state",
                            pal.red("UNUSABLE")
                        );
                        problems += 1;
                    }
                    Some((adapter, base)) => match probe_for(*adapter, base) {
                        None => println!("  {label}: no adapter for {adapter:?}"),
                        Some(probe) => match probe.probe(account, &client).await {
                            Ok(outcome) => println!(
                                "  {label}: {} ({} gates)",
                                pal.green("ok"),
                                outcome.gates.len()
                            ),
                            Err(e) => {
                                println!("  {label}: {} - {}", pal.red("UNREACHABLE"), e.message);
                                problems += 1;
                            }
                        },
                    },
                }
            }
        }
    }

    // What each provider actually publishes. A route the provider does not have is
    // a request it will refuse, and this is the check that catches a mistyped
    // vendor prefix on its own -- the price check above only catches it when a
    // price entry happens to be spelled the other way round.
    println!("catalog     :");
    let by_provider = routes_by_provider(&config);
    if by_provider.is_empty() {
        println!("  no routes configured");
    }
    for (provider, routes) in &by_provider {
        let Some(account) = usable.iter().find(|a| a.provider == *provider) else {
            println!("  {provider}: skipped, no usable credential to ask with");
            continue;
        };
        match crate::http::catalog::fetch_catalog(&client, &account.base_url, &account.api_key)
            .await
        {
            Ok(catalog) => {
                for model in routes {
                    if crate::http::catalog::publishes(&catalog, model) {
                        println!("  {model} ({provider}): {}", pal.green("ok"));
                        continue;
                    }
                    let closest =
                        crate::closest_names(catalog.keys().map(String::as_str), model, 3);
                    let hint = if closest.is_empty() {
                        String::new()
                    } else {
                        format!(" - closest published id: {}", closest.join(", "))
                    };
                    println!(
                        "  {model} ({provider}): {} ({} models published){hint}",
                        pal.red("NOT in the provider's catalog"),
                        catalog.len()
                    );
                    problems += 1;
                }
            }
            // Not a problem the operator can fix: the provider may simply not
            // offer a model list. Say so and move on.
            Err(reason) => println!("  {provider}: could not read its catalog: {reason}"),
        }
    }

    // Two settings whose absence is invisible until it costs money or leaks
    // quota. Neither was checked anywhere: the proxy opened its port silently
    // and doctor reported "no problems found".
    match &config.server.local_key {
        None => {
            println!(
                "local auth  : {} - anything that can reach {} can spend the pooled quota.",
                pal.red("MISSING"),
                config.server.listen
            );
            println!(
                "              {}",
                pal.dim(
                    "fix: lgw keys add local gateway, then local_key = { keychain = true } under [server]"
                )
            );
            problems += 1;
        }
        Some(secret_ref) => {
            // "The config names a key" and "the key is there" are different
            // facts, and serve fails closed on the second one. Doctor is where
            // that shows up instead of at the next restart.
            match secret_ref.resolve(creds, LOCAL_KEY_PROVIDER, LOCAL_KEY_ACCOUNT) {
                Ok(key) => println!(
                    "local auth  : {} ({} from {})",
                    pal.green("configured"),
                    crate::secrets::describe_secret(key.expose()),
                    secret_ref.describe()
                ),
                Err(e) => {
                    println!("local auth  : {} - {e}", pal.red("NOT RESOLVABLE"));
                    problems += 1;
                }
            }
        }
    }
    if !listens_on_loopback(&config.server.listen) {
        println!(
            "listen      : {} is not a loopback address; this process holds paid credentials",
            config.server.listen
        );
        problems += 1;
    }

    if problems == 0 {
        println!("doctor      : no problems found");
        Ok(())
    } else {
        bail!("doctor found {problems} problem(s)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this exists for: the price was written for the id xkiro publishes,
    /// the route was typed with the wrong vendor prefix, and the two never met.
    /// The route then had no price at all, so the hard gate sized it at the
    /// conservative default -- a 100x overestimate that nothing reported.
    #[test]
    fn a_price_no_route_can_reach_is_reported_with_the_route_it_was_meant_for() {
        let config = Config::from_toml_str(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"

[providers.xkiro.accounts.acc-1]
key = "sk-x"

[prices.xkiro."z-ai/glm-5.3-flash"]
input = 0.15
output = 0.5

[routes]
"glm/glm-5.3-flash" = "xkiro"
"#,
        )
        .unwrap();

        let found = unreachable_prices(&config);
        assert_eq!(found.len(), 1, "the price entry cannot be reached");
        assert_eq!(found[0].provider, "xkiro");
        assert_eq!(found[0].model, "z-ai/glm-5.3-flash");
        assert_eq!(
            found[0].closest,
            vec!["glm/glm-5.3-flash"],
            "the hint names the route that probably meant this model"
        );
    }

    #[test]
    fn a_price_a_route_can_reach_is_not_reported() {
        let config = Config::from_toml_str(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"

[providers.xkiro.accounts.acc-1]
key = "sk-x"

[prices.xkiro."z-ai/glm-5.3-flash"]
input = 0.15
output = 0.5

[routes]
"z-ai/glm-5.3-flash" = "xkiro"
"#,
        )
        .unwrap();

        assert!(unreachable_prices(&config).is_empty());
        assert_eq!(
            routes_by_provider(&config).get("xkiro"),
            Some(&vec!["z-ai/glm-5.3-flash"])
        );
    }

    /// The same model name under the wrong provider is still unreachable: a price
    /// is looked up by (provider, model), and the route is what decides provider.
    #[test]
    fn a_price_for_another_provider_is_unreachable() {
        let config = Config::from_toml_str(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"

[providers.xkiro.accounts.acc-1]
key = "sk-x"

[providers.commandcode]
adapter = "command-code"
base_url = "https://api.commandcode.ai/provider/v1"

[providers.commandcode.accounts.acc-1]
key = "sk-c"

[prices.xkiro."deepseek/deepseek-v4.1-flash"]
input = 0.15
output = 0.6

[routes]
"deepseek/deepseek-v4.1-flash" = "commandcode"
"#,
        )
        .unwrap();

        let found = unreachable_prices(&config);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].provider, "xkiro");
        assert_eq!(found[0].model, "deepseek/deepseek-v4.1-flash");
    }

    /// Routes are grouped per provider, each in model order, which is how the
    /// catalog check asks each provider once.
    #[test]
    fn routes_are_grouped_by_provider_in_model_order() {
        let config = Config::from_toml_str(
            r#"
[providers.xkiro]
adapter = "xkiro"
base_url = "https://api.xkiro.com/v1"

[providers.xkiro.accounts.acc-1]
key = "sk-x"

[providers.commandcode]
adapter = "command-code"
base_url = "https://api.commandcode.ai/provider/v1"

[providers.commandcode.accounts.acc-1]
key = "sk-c"

[routes]
"z-ai/glm-5.3-flash" = "xkiro"
"deepseek/deepseek-v4.1-flash" = "commandcode"
"z-ai/glm-5.3-pro" = "xkiro"
"#,
        )
        .unwrap();

        let grouped = routes_by_provider(&config);
        assert_eq!(
            grouped.get("xkiro"),
            Some(&vec!["z-ai/glm-5.3-flash", "z-ai/glm-5.3-pro"])
        );
        assert_eq!(
            grouped.get("commandcode"),
            Some(&vec!["deepseek/deepseek-v4.1-flash"])
        );
        assert_eq!(grouped.len(), 2, "one group per provider that has routes");
    }

    /// Only a loopback address counts as local.
    ///
    /// The proxy holds paid credentials and doctor said nothing about an address
    /// the rest of the network could reach.
    #[test]
    fn only_loopback_addresses_are_treated_as_local() {
        assert!(listens_on_loopback("127.0.0.1:8787"));
        assert!(listens_on_loopback("localhost:8787"));
        assert!(listens_on_loopback("[::1]:8787"));
        assert!(listens_on_loopback("127.0.0.53:8787"));
        assert!(!listens_on_loopback("0.0.0.0:8787"));
        assert!(!listens_on_loopback("[::]:8787"));
        assert!(!listens_on_loopback("192.168.1.10:8787"));
    }
}
