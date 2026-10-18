//! Doctor: every check the operator needs, one structured report.
//!
//! The same checks answer two consumers: the terminal command prints them in its
//! established layout, and the console renders them with verdict labels in the
//! page language. Network checks have bounded timeouts and their errors are
//! flattened to one short line before they reach either surface.

use crate::config::{Config, QuotaAdapter, SecretRef};
use crate::quota::probe_for;
use crate::secrets::{CredentialStore, LOCAL_KEY_ACCOUNT, LOCAL_KEY_PROVIDER};
use crate::store;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    /// The check family, e.g. "credentials", "probes", "catalog".
    pub label: String,
    pub verdict: Verdict,
    /// Technical detail; never contains a credential value.
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    pub problems: usize,
}

impl DoctorReport {
    fn push(&mut self, label: &str, verdict: Verdict, detail: impl Into<String>) {
        if verdict == Verdict::Fail {
            self.problems += 1;
        }
        self.checks.push(DoctorCheck {
            label: label.to_string(),
            verdict,
            detail: detail.into(),
        });
    }

    pub fn has_failures(&self) -> bool {
        self.problems > 0
    }
}

/// Whether a listen address is confined to this machine. Textual on purpose: an
/// address that does not name loopback is treated as reachable by others.
pub fn listens_on_loopback(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    matches!(host, "127.0.0.1" | "localhost" | "::1") || host.starts_with("127.")
}

/// A price entry that no route can reach.
pub struct UnreachablePrice {
    pub provider: String,
    pub model: String,
    /// Routes whose name is close to it: the likely typo.
    pub closest: Vec<String>,
}

pub fn unreachable_prices(config: &Config) -> Vec<UnreachablePrice> {
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

pub fn routes_by_provider(config: &Config) -> BTreeMap<&str, Vec<&str>> {
    let mut out: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (model, provider) in &config.routes {
        out.entry(provider.as_str()).or_default().push(model);
    }
    out
}

/// One line, no control characters, at most 300 characters. The same shape is
/// used for logs and for the page, so a provider cannot inject layout through an
/// error string.
pub fn sanitise(message: &str) -> String {
    let flat: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clean = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() > 300 {
        clean.chars().take(300).collect()
    } else {
        clean
    }
}

pub async fn run(
    config: &Config,
    creds: &CredentialStore,
    client: &reqwest::Client,
    timeout: Duration,
) -> DoctorReport {
    let mut report = DoctorReport::default();
    let (usable, unusable) = match config.build_accounts_lenient(creds) {
        Ok(pair) => pair,
        Err(error) => {
            report.push("credentials", Verdict::Fail, sanitise(&error.to_string()));
            return report;
        }
    };

    let accounts: usize = config.providers.values().map(|p| p.accounts.len()).sum();
    report.push(
        "providers",
        Verdict::Ok,
        format!(
            "{} providers, {} accounts, {} routes",
            config.providers.len(),
            accounts,
            config.routes.len()
        ),
    );
    report.push(
        "policy",
        if config.policy.allow_metered {
            Verdict::Warn
        } else {
            Verdict::Ok
        },
        format!(
            "soft={:.2} allow_metered={} drift={:.2} probe_before_switch={} max_retries={}",
            config.policy.soft_threshold,
            config.policy.allow_metered,
            config.policy.drift_threshold,
            config.policy.probe_before_switch,
            config.policy.max_retries
        ),
    );
    for (name, provider) in &config.providers {
        if let Some(on) = provider.allow_metered {
            report.push(
                "policy",
                Verdict::Warn,
                format!(
                    "provider {name}: allow_metered={} (overrides [policy])",
                    if on { "on" } else { "off" }
                ),
            );
        }
    }

    let unreachable = unreachable_prices(config);
    if config.prices.values().all(|models| models.is_empty()) {
        report.push(
            "prices",
            Verdict::Skip,
            "none configured; every route is sized at the conservative default",
        );
    } else if unreachable.is_empty() {
        report.push(
            "prices",
            Verdict::Ok,
            "every entry is reachable from a route",
        );
    } else {
        for entry in &unreachable {
            let hint = if entry.closest.is_empty() {
                String::new()
            } else {
                format!(" - did you mean {:?}?", entry.closest.join("\" or \""))
            };
            report.push(
                "prices",
                Verdict::Fail,
                format!("no route sends {} to {}{hint}", entry.model, entry.provider),
            );
        }
    }

    let db = config.db_path();
    report.push(
        "database",
        Verdict::Ok,
        if store::db_exists(&db) {
            format!("{} (present)", db.display())
        } else {
            format!("{} (not created yet)", db.display())
        },
    );

    for (provider, p) in &config.providers {
        if p.accounts.is_empty() {
            report.push(
                "credentials",
                Verdict::Fail,
                format!("{provider}: no accounts configured"),
            );
        }
        for (id, a) in &p.accounts {
            let secret_ref = a.key.clone().unwrap_or(SecretRef::Keychain);
            let source = secret_ref.describe();
            let qualified = crate::domain::qualified_id(provider, id);
            if usable
                .iter()
                .any(|account| account.qualified() == qualified)
            {
                report.push("credentials", Verdict::Ok, format!("{qualified}: {source}"));
                continue;
            }
            let reason = unusable
                .iter()
                .find(|account| account.qualified() == qualified)
                .and_then(|account| account.last_error.clone())
                .unwrap_or_else(|| "could not be resolved".to_string());
            report.push(
                "credentials",
                Verdict::Fail,
                format!("{qualified}: MISSING ({source}) - {reason}"),
            );
        }
    }

    let mut targets: HashMap<String, (QuotaAdapter, String)> = HashMap::new();
    for (provider, adapter, base) in config.probe_targets() {
        targets.insert(provider, (adapter, base));
    }
    if usable.is_empty() {
        report.push(
            "probes",
            Verdict::Skip,
            "skipped: no account has a usable credential",
        );
    } else {
        for account in &usable {
            let label = format!("{}/{}", account.provider, account.id);
            match targets.get(&account.provider) {
                None => report.push("probes", Verdict::Skip, format!("{label}: no probe target")),
                Some((QuotaAdapter::None, _)) => report.push(
                    "probes",
                    Verdict::Fail,
                    format!(
                        "{label}: adapter = none has no quota source, so this account can never leave UNKNOWN"
                    ),
                ),
                Some((adapter, base)) => match probe_for(*adapter, base) {
                    None => report.push(
                        "probes",
                        Verdict::Skip,
                        format!("{label}: no adapter for {adapter:?}"),
                    ),
                    Some(probe) => {
                        match tokio::time::timeout(timeout, probe.probe(account, client)).await {
                            Err(_) => report.push(
                                "probes",
                                Verdict::Fail,
                                format!("{label}: probe timed out"),
                            ),
                            Ok(Err(error)) => report.push(
                                "probes",
                                Verdict::Fail,
                                format!("{label}: UNREACHABLE - {}", sanitise(&error.message)),
                            ),
                            Ok(Ok(outcome)) => report.push(
                                "probes",
                                Verdict::Ok,
                                format!("{label}: ok ({} gates)", outcome.gates.len()),
                            ),
                        }
                    }
                },
            }
        }
    }

    let by_provider = routes_by_provider(config);
    if by_provider.is_empty() {
        report.push("catalog", Verdict::Skip, "no routes configured");
    }
    for (provider, routes) in &by_provider {
        let Some(account) = usable.iter().find(|account| account.provider == *provider) else {
            report.push(
                "catalog",
                Verdict::Skip,
                format!("{provider}: skipped, no usable credential to ask with"),
            );
            continue;
        };
        match tokio::time::timeout(
            timeout,
            crate::http::catalog::fetch_catalog(client, &account.base_url, &account.api_key),
        )
        .await
        {
            Err(_) => report.push(
                "catalog",
                Verdict::Skip,
                format!("{provider}: catalog request timed out"),
            ),
            Ok(Err(reason)) => report.push(
                "catalog",
                Verdict::Skip,
                format!(
                    "{provider}: could not read its catalog: {}",
                    sanitise(&reason)
                ),
            ),
            Ok(Ok(catalog)) => {
                for model in routes {
                    if crate::http::catalog::publishes(&catalog, model) {
                        report.push("catalog", Verdict::Ok, format!("{model} ({provider}): ok"));
                        continue;
                    }
                    let closest =
                        crate::closest_names(catalog.keys().map(String::as_str), model, 3);
                    let hint = if closest.is_empty() {
                        String::new()
                    } else {
                        format!(" - closest published id: {}", closest.join(", "))
                    };
                    report.push(
                        "catalog",
                        Verdict::Fail,
                        format!(
                            "{model} ({provider}): NOT in the provider's catalog ({} models published){hint}",
                            catalog.len()
                        ),
                    );
                }
            }
        }
    }

    match &config.server.local_key {
        None => report.push(
            "local auth",
            Verdict::Fail,
            format!(
                "MISSING - anything that can reach {} can spend the pooled quota",
                config.server.listen
            ),
        ),
        Some(secret_ref) => {
            match secret_ref.resolve(creds, LOCAL_KEY_PROVIDER, LOCAL_KEY_ACCOUNT) {
                Ok(_) => report.push(
                    "local auth",
                    Verdict::Ok,
                    format!("configured (from {})", secret_ref.describe()),
                ),
                Err(error) => report.push(
                    "local auth",
                    Verdict::Fail,
                    format!("NOT RESOLVABLE - {}", sanitise(&error.to_string())),
                ),
            }
        }
    }

    if !listens_on_loopback(&config.server.listen) {
        report.push(
            "listen",
            Verdict::Fail,
            format!(
                "{} is not a loopback address; this process holds paid credentials",
                config.server.listen
            ),
        );
    }

    report
}
