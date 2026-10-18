//! `lgw doctor`: check config, credentials, and probe reachability.

use super::{ConfigOrigin, probe_client};
use crate::ansi::Palette;
use crate::config::Config;
use crate::secrets::CredentialStore;
use anyhow::{Result, bail};
use std::path::Path;

pub(super) async fn cmd_doctor(
    path: &Path,
    origin: ConfigOrigin,
    pal: &Palette,
    creds: &CredentialStore,
) -> Result<()> {
    println!("config path : {} (from {})", path.display(), origin.label());
    let config = match Config::load(path) {
        Ok(config) => {
            println!("config file : {}", pal.green("parsed ok"));
            config
        }
        Err(error) => {
            println!("config file : {}", pal.red("FAILED"));
            println!("              {error:#}");
            bail!("configuration is unusable; nothing else could be checked");
        }
    };

    let client = probe_client(&config)?;
    let report =
        crate::doctor::run(&config, creds, &client, std::time::Duration::from_secs(6)).await;
    for check in &report.checks {
        let detail = match check.verdict {
            crate::doctor::Verdict::Ok => pal.green(&check.detail),
            crate::doctor::Verdict::Warn => pal.yellow(&check.detail),
            crate::doctor::Verdict::Fail => pal.red(&check.detail),
            crate::doctor::Verdict::Skip => pal.dim(&check.detail),
        };
        println!("{:<12}: {detail}", check.label);
    }
    if report.problems == 0 {
        println!("doctor      : {}", pal.green("no problems found"));
        Ok(())
    } else {
        bail!("doctor found {} problem(s)", report.problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{listens_on_loopback, routes_by_provider, unreachable_prices};

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
