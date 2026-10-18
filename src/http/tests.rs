//! Unit tests for the HTTP layer: a mock upstream, a fake backend and the routes.

use super::chat::{Finalizer, RelayStream};
use super::*;
use crate::accounts::{Pool, gate};
use crate::config::{Config, SecretRef};
use crate::domain::{GateClass, GateKind, RequestStatus, now};
use crate::meter::PriceTable;
use crate::runtime::Runtime;
use crate::sse::StreamMeter;
use axum::body::Bytes;
use serde_json::json;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

// ------------------------------------------------------------ mock upstream

#[derive(Clone, Copy, PartialEq, Eq)]
enum MockMode {
    /// First call: HTTP 429 with a quota-shaped body. Later calls: a
    /// normal OpenAI SSE stream including the empty-choices usage frame.
    Rotate429ThenStream,
    /// Always a non-streaming JSON completion.
    JsonOk,
    /// Always HTTP 400 whose body echoes the credential it was sent, the way
    /// a provider with a chatty request error does. 400 rather than 401 on
    /// purpose: an auth failure disables the account, so the request is then
    /// refused at planning time and the upstream text never reaches a
    /// client at all.
    EchoKeyOn400,
    /// Sends the response headers, reports that the request has arrived, then
    /// waits for the test to release the body before the first SSE chunk. Later
    /// calls stream normally, so one mock can hold the first request and serve
    /// the next.
    HoldStream,
}

/// What a provider answers `GET /models` with. The default is the one model
/// the test config routes to; tests about the catalog set their own.
const MOCK_CATALOG: &str = r#"{"object":"list","data":[{"id":"mock/model","object":"model"}]}"#;

struct MockUpstream {
    mode: MockMode,
    calls: AtomicUsize,
    bearers: Mutex<Vec<String>>,
    bodies: Mutex<Vec<Value>>,
    catalog: String,
    /// `HoldStream`: fired once the request has arrived and its headers are
    /// about to be sent.
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// `HoldStream`: waited on before the first body chunk.
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

async fn mock_models(State(m): State<Arc<MockUpstream>>, headers: HeaderMap) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    m.bearers.lock().unwrap().push(bearer);
    let mut response = Response::new(Body::from(m.catalog.clone()));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    response
}

async fn mock_chat(
    State(m): State<Arc<MockUpstream>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    m.bearers.lock().unwrap().push(bearer);
    m.bodies
        .lock()
        .unwrap()
        .push(serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null));
    let call = m.calls.fetch_add(1, Ordering::SeqCst);

    if m.mode == MockMode::Rotate429ThenStream && call == 0 {
        let mut response = Response::new(Body::from(
            r#"{"error":{"message":"window exhausted","type":"insufficient_quota","code":"usage_exceeded"}}"#,
        ));
        *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, "3600".parse().unwrap());
        return response;
    }

    if m.mode == MockMode::EchoKeyOn400 {
        // Read the credential back out of the record rather than keeping a
        // second copy of it.
        let sent = m
            .bearers
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default();
        let body = json!({
            "error": {
                "message": format!("request rejected, the key {sent} is not accepted here"),
                "type": "invalid_request_error",
                "code": "invalid_request_error"
            }
        });
        let mut response = Response::new(Body::from(body.to_string()));
        *response.status_mut() = StatusCode::BAD_REQUEST;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        return response;
    }

    if m.mode == MockMode::JsonOk {
        let body = r#"{"id":"c1","object":"chat.completion","model":"mock/model","choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#;
        let mut response = Response::new(Body::from(body));
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        return response;
    }

    let sse = mock_sse();
    let body = if m.mode == MockMode::HoldStream {
        // Headers go out with the response; the first body chunk waits for the
        // test. That makes "in flight" observable: the request has been admitted
        // and relayed, and nothing has settled yet.
        if let Some(started) = m.started.lock().unwrap().take() {
            let _ = started.send(());
        }
        let release = m.release.lock().unwrap().take();
        Body::from_stream(futures_util::stream::once(async move {
            if let Some(release) = release {
                let _ = release.await;
            }
            Ok::<Bytes, std::io::Error>(Bytes::from(sse))
        }))
    } else {
        Body::from(sse)
    };
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    response
}

/// The SSE body every successful streaming response carries: hello, finish and
/// the usage frame the settlement assertions read.
fn mock_sse() -> String {
    let mut sse = String::new();
    for chunk in [
        json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}),
        json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}),
        json!({"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"id":"c1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}),
    ] {
        sse.push_str("data: ");
        sse.push_str(&chunk.to_string());
        sse.push_str("\n\n");
    }
    sse.push_str("data: [DONE]\n\n");
    sse
}

async fn spawn_mock(mode: MockMode) -> (String, Arc<MockUpstream>) {
    spawn_mock_with_catalog(mode, MOCK_CATALOG).await
}

/// A mock provider whose `/models` answers with the given catalog.
async fn spawn_mock_with_catalog(mode: MockMode, catalog: &str) -> (String, Arc<MockUpstream>) {
    let state = Arc::new(MockUpstream {
        mode,
        calls: AtomicUsize::new(0),
        bearers: Mutex::new(Vec::new()),
        bodies: Mutex::new(Vec::new()),
        catalog: catalog.to_string(),
        started: Mutex::new(None),
        release: Mutex::new(None),
    });
    spawn_mock_state(state).await
}

/// A mock that reports when a chat request has arrived, then holds its first SSE
/// chunk until the test releases it.
async fn spawn_held_mock() -> (
    String,
    Arc<MockUpstream>,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let state = Arc::new(MockUpstream {
        mode: MockMode::HoldStream,
        calls: AtomicUsize::new(0),
        bearers: Mutex::new(Vec::new()),
        bodies: Mutex::new(Vec::new()),
        catalog: MOCK_CATALOG.to_string(),
        started: Mutex::new(Some(started)),
        release: Mutex::new(Some(release_rx)),
    });
    let (url, state) = spawn_mock_state(state).await;
    (url, state, started_rx, release)
}

async fn spawn_mock_state(state: Arc<MockUpstream>) -> (String, Arc<MockUpstream>) {
    let app = Router::new()
        .route("/chat/completions", post(mock_chat))
        .route("/models", get(mock_models))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state)
}

// ------------------------------------------------------------ fake backend

struct RecordedSuccess {
    plan: AttemptPlan,
    usage: TokenUsage,
    cost: Decimal,
    attempt: AttemptRecord,
    record: RequestRecord,
}

struct RecordedFailure {
    account_id: String,
    class: UpstreamErrorClass,
    attempt: AttemptRecord,
}

struct FakeBackend {
    runtime: RwLock<Arc<Runtime>>,
    pool: Pool,
    successes: Mutex<Vec<RecordedSuccess>>,
    failures: Mutex<Vec<RecordedFailure>>,
    /// The last configuration published through the apply seam, if any.
    applied: Mutex<Option<Config>>,
    /// How many times a configuration was published, so a test can prove the
    /// watcher did not apply the same bytes twice.
    applied_count: std::sync::atomic::AtomicUsize,
    /// A report the usage tests put here instead of opening a real ledger.
    usage: Mutex<Option<crate::store::UsageReport>>,
    /// Requests the page tests put here instead of opening a real ledger.
    history: Mutex<Option<Vec<crate::store::RequestHistory>>>,
    /// The credentials the Keys page reads and writes in tests.
    credentials: crate::secrets::CredentialStore,
}

impl FakeBackend {
    fn new(config: Config, pool: Pool) -> Self {
        FakeBackend {
            runtime: RwLock::new(runtime_for(&config)),
            pool,
            successes: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
            applied: Mutex::new(None),
            applied_count: std::sync::atomic::AtomicUsize::new(0),
            usage: Mutex::new(None),
            history: Mutex::new(None),
            credentials: crate::secrets::CredentialStore::memory(),
        }
    }

    /// The publication the production gateway exposes to the apply engine:
    /// requests admitted after this call use the new snapshot.
    fn install_runtime(&self, runtime: Arc<Runtime>) {
        *self.runtime.write().unwrap() = runtime;
    }
}

/// The snapshot a test backend admits under: configured prices seeded the way
/// `Gateway::with_credentials` seeds them.
fn runtime_for(config: &Config) -> Arc<Runtime> {
    let mut prices = PriceTable::new();
    prices.seed_from_config(&config.prices);
    Arc::new(Runtime::new(Arc::new(config.clone()), Arc::new(prices)).unwrap())
}

impl RuntimeSource for FakeBackend {
    fn runtime(&self) -> Arc<Runtime> {
        self.runtime.read().unwrap().clone()
    }
}

impl RequestBackend for FakeBackend {
    fn resolve(&self, runtime: &Runtime, client_model: &str) -> Result<ResolvedRoute> {
        runtime.resolve(client_model)
    }

    fn plan(
        &self,
        runtime: &Runtime,
        client_model: &str,
        session_key: Option<&str>,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
        exclude: &[String],
    ) -> std::result::Result<(ResolvedRoute, Reservation), PlanError> {
        let route = runtime
            .resolve(client_model)
            .map_err(|e| PlanError::UnknownProvider(e.to_string()))?;
        let plan = self.pool.select(
            &route.provider,
            &route.model,
            session_key,
            est_cost,
            request_id,
            attempt_no,
            exclude,
        )?;
        Ok((route, plan))
    }

    fn pick_account(
        &self,
        provider: &str,
        est_cost: Decimal,
        request_id: &str,
        attempt_no: u32,
    ) -> std::result::Result<AttemptPlan, PlanError> {
        let reservation =
            self.pool
                .select(provider, "", None, est_cost, request_id, attempt_no, &[])?;
        // Settled immediately: see the production impl above.
        let plan = reservation.plan().clone();
        reservation.settle(Decimal::ZERO, false);
        Ok(plan)
    }

    fn account(&self, account_id: &str) -> Option<Account> {
        self.pool.account(account_id)
    }

    fn retry_after_for(&self, provider: &str) -> Option<Duration> {
        self.pool.retry_after_for(provider)
    }

    fn http(&self, runtime: &Runtime) -> reqwest::Client {
        runtime.http().clone()
    }

    fn tier_at(&self, _runtime: &Runtime, _provider: &str, _model: &str) -> TierKey {
        TierKey::Default
    }

    fn estimate_cost(
        &self,
        runtime: &Runtime,
        provider: &str,
        model: &str,
        tier: &TierKey,
        usage: &TokenUsage,
        multiplier: Decimal,
    ) -> Option<Decimal> {
        runtime
            .prices()
            .estimate(provider, model, tier, usage, multiplier)
    }

    fn record_success(
        &self,
        reservation: Reservation,
        usage: &TokenUsage,
        cost: Decimal,
        attempt: AttemptRecord,
        record: RequestRecord,
    ) {
        let plan = reservation.plan().clone();
        reservation.settle(cost, true);
        self.successes.lock().unwrap().push(RecordedSuccess {
            plan,
            usage: usage.clone(),
            cost,
            attempt,
            record,
        });
    }

    fn record_failure(
        &self,
        reservation: Reservation,
        class: UpstreamErrorClass,
        retry_after: Option<Duration>,
        window_reset: Option<OffsetDateTime>,
        attempt: AttemptRecord,
    ) {
        let qualified = reservation.qualified_account();
        let account_id = reservation.account_id.clone();
        reservation.settle(Decimal::ZERO, false);
        self.pool
            .note_error(&qualified, class, retry_after, window_reset);
        self.failures.lock().unwrap().push(RecordedFailure {
            account_id,
            class,
            attempt,
        });
    }
}

impl OperatorBackend for FakeBackend {
    fn snapshot(&self) -> Vec<Account> {
        self.pool.snapshot()
    }

    fn credentials(&self) -> Option<crate::secrets::CredentialStore> {
        Some(self.credentials.clone())
    }

    fn keys_report(&self) -> Option<crate::keys::KeysReport> {
        Some(crate::keys::report(
            self.runtime().config(),
            &self.credentials,
        ))
    }

    fn key_add(
        &self,
        provider: &str,
        account: &str,
        secret: &str,
    ) -> std::result::Result<(), crate::http::ApplyError> {
        let config = self.runtime().config().clone();
        crate::keys::store(&config, &self.credentials, provider, account, secret)
            .map_err(|error| crate::http::ApplyError(error.to_string()))
    }

    fn key_remove(
        &self,
        provider: &str,
        account: &str,
    ) -> std::result::Result<bool, crate::http::ApplyError> {
        crate::keys::remove(&self.credentials, provider, account)
            .map_err(|error| crate::http::ApplyError(error.to_string()))
    }

    fn key_test<'a>(
        &'a self,
        provider: &'a str,
        account: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = std::result::Result<
                        crate::keys::KeyTestOutcome,
                        crate::http::ApplyError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            if provider == crate::secrets::LOCAL_KEY_PROVIDER
                && account == crate::secrets::LOCAL_KEY_ACCOUNT
            {
                return match crate::keys::local_token_status(&self.credentials) {
                    Ok(Some(chars)) => Ok(crate::keys::KeyTestOutcome::LocalToken { chars }),
                    _ => Err(crate::http::ApplyError(
                        "no local gateway token is stored".to_string(),
                    )),
                };
            }
            match self.credentials.get(provider, account) {
                Ok(Some(_)) => Ok(crate::keys::KeyTestOutcome::Probed {
                    probe: "mock".to_string(),
                    gates: Vec::new(),
                    currency: "USD".to_string(),
                }),
                _ => Err(crate::http::ApplyError(
                    "the credential is missing or could not be resolved".to_string(),
                )),
            }
        })
    }

    fn probe_accounts<'a>(
        &'a self,
        provider: Option<&'a str>,
        account: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<(), crate::http::ApplyError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let gates = || {
                vec![gate(
                    crate::domain::GateKind::FiveHour,
                    Decimal::from(100),
                    Decimal::from(5),
                    None,
                )]
            };
            match (provider, account) {
                (Some(_), Some("broken")) => {
                    return Err(crate::http::ApplyError(
                        "the provider refused the probe: connection refused".to_string(),
                    ));
                }
                (Some(provider), Some(account)) => {
                    let qualified = crate::domain::qualified_id(provider, account);
                    self.pool
                        .apply_probe(&qualified, gates(), crate::domain::now());
                }
                (None, None) => {
                    for account in self.pool.snapshot() {
                        self.pool
                            .apply_probe(&account.qualified(), gates(), crate::domain::now());
                    }
                }
                _ => {
                    return Err(crate::http::ApplyError(
                        "a probe needs both a provider and an account, or neither".to_string(),
                    ));
                }
            }
            Ok(())
        })
    }

    fn set_account_enabled(
        &self,
        provider: &str,
        account: &str,
        enabled: bool,
    ) -> std::result::Result<(), crate::http::ApplyError> {
        let qualified = crate::domain::qualified_id(provider, account);
        self.pool
            .set_operator_state(&qualified, enabled)
            .map(|_| ())
            .map_err(crate::http::ApplyError)
    }

    fn price_table(&self) -> Option<crate::meter::PriceTable> {
        Some(self.runtime().prices().clone())
    }

    fn forget_prices(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> std::result::Result<usize, crate::http::ApplyError> {
        let current = self.runtime();
        let learned = current.prices().learned_rows();
        let in_scope = |row: &crate::meter::PersistedPrice| {
            provider.is_none_or(|provider| row.provider == provider)
                && model.is_none_or(|model| row.model == model)
        };
        let forgotten = learned.iter().filter(|row| in_scope(row)).count();
        let keep: Vec<crate::meter::PersistedPrice> =
            learned.into_iter().filter(|row| !in_scope(row)).collect();
        let mut next = crate::meter::PriceTable::new();
        next.load_learned(keep);
        next.seed_from_config(&current.config().prices);
        self.install_runtime(current.with_prices(std::sync::Arc::new(next)));
        Ok(forgotten)
    }

    fn request_history(
        &self,
        _limit: u32,
        _errors_only: bool,
        _coverage: Option<crate::domain::GateClass>,
    ) -> Option<Vec<crate::store::RequestHistory>> {
        self.history.lock().unwrap().clone()
    }

    fn usage_report(
        &self,
        _now: OffsetDateTime,
        _since: OffsetDateTime,
        _dims: &[crate::domain::GroupBy],
    ) -> Option<crate::store::UsageReport> {
        self.usage.lock().unwrap().clone()
    }

    fn apply_config(
        &self,
        config: Config,
        write: &mut dyn FnMut() -> std::io::Result<()>,
    ) -> std::result::Result<(), crate::http::ApplyError> {
        let reset_providers =
            crate::http::providers_needing_reset(self.runtime().config(), &config);
        let prices = Arc::new(self.runtime().prices().reseeded(&config.prices));
        let runtime = Arc::new(Runtime::new(Arc::new(config.clone()), prices).unwrap());
        let (mut accounts, unusable) = config
            .build_accounts_lenient(&crate::secrets::CredentialStore::memory())
            .map_err(|error| crate::http::ApplyError(error.to_string()))?;
        accounts.extend(unusable);
        write().map_err(|error| crate::http::ApplyError(error.to_string()))?;
        self.pool.retune(&config.policy);
        self.install_runtime(runtime);
        self.pool.reconfigure(accounts, &reset_providers);
        *self.applied.lock().unwrap() = Some(config);
        self.applied_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

// ------------------------------------------------------------ helpers

fn test_config(base_url: &str) -> Config {
    let toml = format!(
        r#"
[server]
listen = "127.0.0.1:0"

[policy]
max_retries = 3

[providers.mock]
adapter = "none"
base_url = "{base_url}"

[providers.mock.accounts.a1]
key = "sk-a1"

[providers.mock.accounts.a2]
key = "sk-a2"

[routes]
"mock/model" = "mock"
"#
    );
    Config::from_toml_str(&toml).unwrap()
}

/// Give an account a probed window. Takes the canonical provider/id, since
/// a bare id is ambiguous across providers.
fn activate(pool: &Pool, qualified_id: &str, used: i64) {
    let gates = vec![gate(
        GateKind::FiveHour,
        Decimal::from(100),
        Decimal::from(used),
        None,
    )];
    pool.apply_probe(qualified_id, gates, now());
}

/// A config whose mock provider publishes a per-million price, so settlement can
/// be attributed to the snapshot a request admitted under.
fn priced_toml(base_url: &str, input: f64, output: f64) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:0"

[policy]
max_retries = 3

[providers.mock]
adapter = "none"
base_url = "{base_url}"

[providers.mock.accounts.a1]
key = "sk-a1"

[providers.mock.accounts.a2]
key = "sk-a2"

[prices.mock."mock/model"]
input = {input}
output = {output}

[routes]
"mock/model" = "mock"
"#
    )
}

fn priced_config(base_url: &str, input: f64, output: f64) -> Config {
    Config::from_toml_str(&priced_toml(base_url, input, output)).unwrap()
}

fn table_for(config: &Config) -> PriceTable {
    let mut table = PriceTable::new();
    table.seed_from_config(&config.prices);
    table
}

fn fake_backend(base_url: &str) -> Arc<FakeBackend> {
    fake_backend_with_config(test_config(base_url))
}

/// A fake backend with both mock accounts probed, built from any config.
fn fake_backend_with_config(config: Config) -> Arc<FakeBackend> {
    let accounts = config
        .build_accounts(&crate::secrets::CredentialStore::memory())
        .unwrap();
    let pool = Pool::new(accounts, &config.policy);
    activate(&pool, "mock/a1", 1);
    activate(&pool, "mock/a2", 0);
    Arc::new(FakeBackend::new(config, pool))
}

/// Two providers with one route each: what a model list has to fan out over.
fn fake_backend_two(base_a: &str, base_b: &str) -> Arc<FakeBackend> {
    let toml = format!(
        r#"
[server]
listen = "127.0.0.1:0"

[providers.mock]
adapter = "none"
base_url = "{base_a}"

[providers.mock.accounts.a1]
key = "sk-a1"

[providers.other]
adapter = "none"
base_url = "{base_b}"

[providers.other.accounts.b1]
key = "sk-b1"

[routes]
"mock/model" = "mock"
"other/model" = "other"
"#
    );
    let config = Config::from_toml_str(&toml).unwrap();
    let accounts = config
        .build_accounts(&crate::secrets::CredentialStore::memory())
        .unwrap();
    let pool = Pool::new(accounts, &config.policy);
    activate(&pool, "mock/a1", 0);
    activate(&pool, "other/b1", 0);
    Arc::new(FakeBackend::new(config, pool))
}

async fn spawn_router(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn wait_for_successes(fake: &Arc<FakeBackend>, want: usize) {
    for _ in 0..400 {
        if fake.successes.lock().unwrap().len() >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {want} recorded successes");
}

/// A provider that echoes the key back must not hand it to the client.
///
/// The upstream message is relayed on purpose -- the client needs to know
/// why the request failed -- so redacting the presented key is the only
/// thing between a chatty authentication error and a credential on the
/// client's screen.
#[tokio::test]
async fn an_upstream_error_that_echoes_the_key_never_reaches_the_client() {
    let (upstream_url, _mock) = spawn_mock(MockMode::EchoKeyOn400).await;
    let addr = spawn_router(build_router_with(fake_backend(&upstream_url))).await;

    let response = reqwest::Client::new()
        .post(format!("{addr}/v1/chat/completions"))
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();

    let status = response.status();
    let text = response.text().await.unwrap();
    assert_eq!(
        status, 400,
        "the upstream's own status is what the client hears: {text}"
    );
    assert!(
        !text.contains("sk-a1") && !text.contains("sk-a2"),
        "the provider's key was relayed back: {text}"
    );
    assert!(
        text.contains("***"),
        "the upstream message is still relayed, with the key masked: {text}"
    );
}

fn chat_body(stream: bool) -> Value {
    json!({
        "model": "mock/model",
        "stream": stream,
        "messages": [{"role": "user", "content": "hi"}],
    })
}

// ------------------------------------------------------------ tests

#[tokio::test]
async fn stream_is_relayed_usage_recorded_and_a_429_rotates_accounts() {
    let (upstream_url, mock) = spawn_mock(MockMode::Rotate429ThenStream).await;
    let fake = fake_backend(&upstream_url);
    let addr = spawn_router(build_router_with(fake.clone())).await;

    let response = reqwest::Client::new()
        .post(format!("{addr}/v1/chat/completions"))
        .json(&chat_body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "the retry must be transparent");
    let text = response.text().await.unwrap();
    assert!(text.contains("Hello"), "the relayed body was: {text}");
    assert!(text.contains("[DONE]"), "the relayed body was: {text}");

    wait_for_successes(&fake, 1).await;
    let successes = fake.successes.lock().unwrap();
    assert_eq!(successes.len(), 1);
    let success = &successes[0];
    assert_eq!(
        success.plan.account_id, "a2",
        "the second account must serve after the first is parked"
    );
    assert_eq!(success.usage.prompt_tokens, 12);
    assert_eq!(success.usage.completion_tokens, 3);
    assert!(
        success.usage.measured,
        "usage must come from the usage frame"
    );
    assert!(success.cost > Decimal::ZERO);
    assert!(success.attempt.billed);
    assert!(
        success.attempt.ttft_ms.is_some(),
        "a delivered stream must record TTFT"
    );
    assert_eq!(success.record.status, RequestStatus::Ok);
    assert_eq!(success.record.retries, 1);
    assert_eq!(success.record.coverage, GateClass::Covered);
    drop(successes);

    let failures = fake.failures.lock().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].account_id, "a1");
    assert_eq!(failures[0].class, UpstreamErrorClass::QuotaExhausted);
    assert!(
        failures[0].attempt.ttft_ms.is_none(),
        "an attempt that failed before content has no TTFT"
    );
    assert!(!failures[0].attempt.billed);
    drop(failures);

    assert_eq!(
        *mock.bearers.lock().unwrap(),
        vec!["Bearer sk-a1".to_string(), "Bearer sk-a2".to_string()],
        "each attempt must carry its own account key"
    );
    let bodies = mock.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2, "both attempts reached the upstream");
    for body in bodies.iter() {
        assert_eq!(
            body["stream_options"]["include_usage"],
            json!(true),
            "include_usage must be injected or the ledger records zero forever"
        );
    }
}

#[tokio::test]
async fn models_passthrough_uses_an_eligible_account() {
    let (upstream_url, mock) = spawn_mock(MockMode::JsonOk).await;
    let fake = fake_backend(&upstream_url);
    let addr = spawn_router(build_router_with(fake.clone())).await;

    let response = reqwest::Client::new()
        .get(format!("{addr}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("mock/model"), "the model list was: {text}");
    assert_eq!(
        *mock.bearers.lock().unwrap(),
        vec!["Bearer sk-a1".to_string()],
        "the passthrough must authenticate as an eligible account"
    );
    // Picking an account commits a reservation; a model list spends no
    // tokens, so it must give it straight back. While it did not, every call
    // burned one concurrency slot against the account's own
    // concurrency_limit -- and, with that limit set, the account became
    // permanently unselectable after N listings.
    assert_eq!(
        fake.pool.inflight_count("mock/a1"),
        0,
        "a passthrough must not hold a reservation"
    );
}

/// The list is the configured routes, and the fields are the provider's own.
#[tokio::test]
async fn the_model_list_is_the_routes_described_by_the_provider() {
    let catalog = r#"{"object":"list","data":[
        {"id":"mock/not-routed","object":"model","display_name":"Not Routed"},
        {"id":"mock/model","object":"model","owned_by":"z-ai","display_name":"Mock Model",
         "context_length":200000,
         "pricing":{"currency":"USD","unit":"per_1m_tokens","input":0.44,"output":1.86}}
    ]}"#;
    let (upstream_url, mock) = spawn_mock_with_catalog(MockMode::JsonOk, catalog).await;
    let addr = spawn_router(build_router_with(fake_backend(&upstream_url))).await;

    let response = reqwest::Client::new()
        .get(format!("{addr}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();

    assert_eq!(body["object"], json!("list"));
    let data = body["data"].as_array().unwrap();
    assert_eq!(
        data.len(),
        1,
        "the provider's catalog is not advertised: {body}"
    );
    assert_eq!(data[0]["id"], json!("mock/model"));
    assert_eq!(data[0]["object"], json!("model"));
    // Whatever the provider published is passed through, field for field.
    assert_eq!(data[0]["display_name"], json!("Mock Model"));
    assert_eq!(data[0]["context_length"], json!(200000));
    assert_eq!(data[0]["pricing"]["unit"], json!("per_1m_tokens"));
    assert_eq!(data[0]["pricing"]["input"], json!(0.44));
    // And its own answer is not overwritten by ours.
    assert_eq!(data[0]["owned_by"], json!("z-ai"));
    assert_eq!(
        *mock.bearers.lock().unwrap(),
        vec!["Bearer sk-a1".to_string()]
    );
}

/// A provider that does not list a configured route does not remove it.
///
/// The route is what this gateway will actually serve, so hiding it because
/// the provider's catalog is stale -- or spells the name differently -- would
/// make the list lie about what works. Fields we cannot source stay absent.
#[tokio::test]
async fn a_route_the_provider_does_not_list_is_still_listed() {
    let (upstream_url, _mock) = spawn_mock_with_catalog(
        MockMode::JsonOk,
        r#"{"object":"list","data":[{"id":"mock/other","object":"model"}]}"#,
    )
    .await;
    let addr = spawn_router(build_router_with(fake_backend(&upstream_url))).await;

    let body: Value = reqwest::Client::new()
        .get(format!("{addr}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1, "{body}");
    assert_eq!(data[0]["id"], json!("mock/model"));
    assert_eq!(data[0]["object"], json!("model"));
    assert_eq!(data[0]["owned_by"], json!("mock"), "{body}");
    assert!(
        data[0].get("display_name").is_none(),
        "no field is invented for the provider: {body}"
    );
}

/// An unreachable provider leaves the route table intact.
///
/// A model list is how a client decides what to send; failing it because one
/// provider's status endpoint is down would take the whole gateway out of the
/// picker even though every other provider is fine.
#[tokio::test]
async fn an_unreadable_catalog_still_lists_the_routes() {
    // Port 1 is not listening, so the upstream call fails.
    let addr = spawn_router(build_router_with(fake_backend("http://127.0.0.1:1"))).await;

    let response = reqwest::Client::new()
        .get(format!("{addr}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "a broken provider is not a broken model list"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["data"][0]["id"], json!("mock/model"));
    assert_eq!(body["data"][0]["owned_by"], json!("mock"));
}

/// Every configured route, from every provider, one upstream call each.
#[tokio::test]
async fn the_model_list_fans_out_over_providers_once_each() {
    let (a_url, a) = spawn_mock_with_catalog(
        MockMode::JsonOk,
        r#"{"object":"list","data":[{"id":"mock/model","object":"model","display_name":"A"}]}"#,
    )
    .await;
    let (b_url, b) = spawn_mock_with_catalog(
        MockMode::JsonOk,
        r#"{"object":"list","data":[{"id":"other/model","object":"model","display_name":"B"}]}"#,
    )
    .await;
    let addr = spawn_router(build_router_with(fake_backend_two(&a_url, &b_url))).await;

    let body: Value = reqwest::Client::new()
        .get(format!("{addr}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2, "{body}");
    assert_eq!(data[0]["id"], json!("mock/model"));
    assert_eq!(data[1]["id"], json!("other/model"));
    assert_eq!(data[0]["display_name"], json!("A"));
    assert_eq!(data[1]["display_name"], json!("B"));
    assert_eq!(
        *a.bearers.lock().unwrap(),
        vec!["Bearer sk-a1".to_string()],
        "each provider is asked once, with its own key"
    );
    assert_eq!(*b.bearers.lock().unwrap(), vec!["Bearer sk-b1".to_string()]);

    // ?provider= narrows the answer to that provider's routes, without asking
    // the other one again.
    let filtered: Value = reqwest::Client::new()
        .get(format!("{addr}/v1/models?provider=other"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let data = filtered["data"].as_array().unwrap();
    assert_eq!(data.len(), 1, "{filtered}");
    assert_eq!(data[0]["id"], json!("other/model"));
    assert_eq!(
        a.bearers.lock().unwrap().len(),
        1,
        "the filtered list does not re-ask the other provider"
    );

    let unknown = reqwest::Client::new()
        .get(format!("{addr}/v1/models?provider=nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
}

/// The dashboard endpoint is a projection of what the poller already saw.
/// Reaching a provider from here would bill an operator for reading a widget.
#[tokio::test]
async fn the_usage_report_is_offline_and_never_looks_like_an_error() {
    let (upstream_url, mock) = spawn_mock(MockMode::JsonOk).await;
    let addr = spawn_router(build_router_with(fake_backend(&upstream_url))).await;
    let client = reqwest::Client::new();

    let response = client.get(format!("{addr}/v1/usage")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();

    assert_eq!(body["object"], json!("usage"));
    // The pool is probed at 1/100 and 0/100: half a percent of the
    // inventory, not "the account we would pick next is empty".
    assert_eq!(body["usage"]["rolling"]["percent"], json!(0.5));
    assert_eq!(body["accounts"].as_array().unwrap().len(), 2);
    assert!(
        body.get("error").is_none(),
        "a 200 body carrying an error object breaks a path client: {body}"
    );
    assert!(!body.to_string().contains("sk-a1"), "{body}");
    assert!(
        mock.bearers.lock().unwrap().is_empty(),
        "the report must not reach the provider"
    );

    assert_eq!(
        client
            .get(format!("{addr}/v1/usage?provider=nope"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        client
            .get(format!("{addr}/v1/usage?account=a1"))
            .send()
            .await
            .unwrap()
            .status(),
        400,
        "a bare account id names a different account under each provider"
    );

    let one: Value = client
        .get(format!("{addr}/v1/usage?provider=mock&account=mock/a1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(one["accounts"][0]["id"], json!("mock/a1"));
}

#[tokio::test]
async fn the_usage_report_is_behind_the_local_key() {
    let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
    let mut config = test_config(&upstream_url);
    config.server.local_key = Some(SecretRef::Literal("sk-local".into()));
    let accounts = config
        .build_accounts(&crate::secrets::CredentialStore::memory())
        .unwrap();
    let pool = Pool::new(accounts, &config.policy);
    activate(&pool, "mock/a1", 1);
    activate(&pool, "mock/a2", 0);
    let fake = Arc::new(FakeBackend::new(config, pool));
    let addr = spawn_router(build_router_with(fake)).await;
    let url = format!("{addr}/v1/usage");
    let client = reqwest::Client::new();

    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("sk-nope")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("sk-local")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

/// A pool nobody has probed says so, instead of answering with zeros that
/// read exactly like an untouched allowance.
#[tokio::test]
async fn an_unprobed_pool_reports_that_it_has_no_reading() {
    let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
    let config = test_config(&upstream_url);
    let accounts = config
        .build_accounts(&crate::secrets::CredentialStore::memory())
        .unwrap();
    let fake = Arc::new(FakeBackend::new(
        config.clone(),
        Pool::new(accounts, &config.policy),
    ));
    let addr = spawn_router(build_router_with(fake)).await;

    let response = reqwest::Client::new()
        .get(format!("{addr}/v1/usage"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("probed"),
        "{body}"
    );
}

#[tokio::test]
async fn non_streaming_response_usage_is_recorded() {
    let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
    let fake = fake_backend(&upstream_url);
    let addr = spawn_router(build_router_with(fake.clone())).await;

    let response = reqwest::Client::new()
        .post(format!("{addr}/v1/chat/completions"))
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("Hello"));

    wait_for_successes(&fake, 1).await;
    let successes = fake.successes.lock().unwrap();
    assert_eq!(successes[0].usage.prompt_tokens, 12);
    assert_eq!(successes[0].usage.completion_tokens, 3);
    assert!(successes[0].usage.measured);
    assert_eq!(successes[0].plan.account_id, "a1");
}

#[tokio::test]
async fn no_eligible_account_is_429_with_retry_after_and_no_upstream_call() {
    let fake = fake_backend("http://127.0.0.1:1");
    let longest = now() + time::Duration::seconds(300);
    let earliest = now() + time::Duration::seconds(120);
    fake.pool.note_error(
        "mock/a1",
        UpstreamErrorClass::QuotaExhausted,
        None,
        Some(earliest),
    );
    fake.pool.note_error(
        "mock/a2",
        UpstreamErrorClass::QuotaExhausted,
        None,
        Some(longest),
    );
    let addr = spawn_router(build_router_with(fake.clone())).await;

    let response = reqwest::Client::new()
        .post(format!("{addr}/v1/chat/completions"))
        .json(&chat_body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    let raw = response
        .headers()
        .get(header::RETRY_AFTER)
        .expect("a 429 must carry Retry-After")
        .to_str()
        .unwrap()
        .to_string();
    let secs: u64 = raw.parse().expect("Retry-After must be delta-seconds");
    assert!(
        (1..=120).contains(&secs),
        "the earliest reset wins, got {secs}"
    );

    // The body has to explain the refusal, not just name it. An operator
    // reading "no eligible account" cannot tell a spent allowance from a
    // request priced above it, and the two need different responses.
    let body = response.text().await.unwrap();
    assert!(
        body.contains("parked after a quota error"),
        "the refusal must name the reason: {body}"
    );
    assert!(
        body.contains("mock/a"),
        "the refusal must name the account that came closest: {body}"
    );

    assert!(fake.successes.lock().unwrap().is_empty());
    assert!(fake.failures.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_model_is_rejected_before_any_upstream_call() {
    let (upstream_url, mock) = spawn_mock(MockMode::JsonOk).await;
    let fake = fake_backend(&upstream_url);
    let addr = spawn_router(build_router_with(fake.clone())).await;

    let response = reqwest::Client::new()
        .post(format!("{addr}/v1/chat/completions"))
        .json(&json!({"model": "nope/model", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        0,
        "an unroutable model must never reach a provider"
    );
}

#[tokio::test]
async fn local_bearer_auth_is_enforced_when_configured() {
    let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
    let mut config = test_config(&upstream_url);
    config.server.local_key = Some(SecretRef::Literal("sk-local".into()));
    let accounts = config
        .build_accounts(&crate::secrets::CredentialStore::memory())
        .unwrap();
    let pool = Pool::new(accounts, &config.policy);
    activate(&pool, "mock/a1", 1);
    activate(&pool, "mock/a2", 0);
    let fake = Arc::new(FakeBackend::new(config, pool));
    let addr = spawn_router(build_router_with(fake)).await;
    let client = reqwest::Client::new();
    let url = format!("{addr}/v1/chat/completions");

    let missing = client
        .post(&url)
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 401);

    let wrong = client
        .post(&url)
        .bearer_auth("sk-nope")
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let accepted = client
        .post(&url)
        .bearer_auth("sk-local")
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), 200);
}

#[tokio::test]
async fn request_body_over_the_configured_limit_is_rejected() {
    let (upstream_url, _mock) = spawn_mock(MockMode::JsonOk).await;
    let mut config = test_config(&upstream_url);
    config.policy.max_body_bytes = 64;
    let accounts = config
        .build_accounts(&crate::secrets::CredentialStore::memory())
        .unwrap();
    let pool = Pool::new(accounts, &config.policy);
    activate(&pool, "a1", 1);
    let fake = Arc::new(FakeBackend::new(config, pool));
    let addr = spawn_router(build_router_with(fake)).await;

    let big = json!({
        "model": "mock/model",
        "messages": [{"role": "user", "content": "x".repeat(4096)}],
    });
    let response = reqwest::Client::new()
        .post(format!("{addr}/v1/chat/completions"))
        .json(&big)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
}

#[tokio::test]
async fn dropping_the_relay_records_a_partial_aborted_request() {
    let fake = fake_backend("http://127.0.0.1:1");
    let reservation = fake
        .pool
        .select("mock", "mock/model", None, Decimal::ZERO, "req-1", 1, &[])
        .unwrap();
    let payload: &'static [u8] =
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";
    let finalizer = Finalizer {
        backend: fake.clone(),
        runtime: fake.runtime(),
        reservation,
        request_id: "req-1".to_string(),
        client_model: "mock/model".to_string(),
        provider_request_id: None,
        currency: "USD".to_string(),
        credit_multiplier: Decimal::ONE,
        coverage: GateClass::Covered,
        attempt_no: 1,
        started_at: now(),
        estimated_prompt_tokens: 10,
        status_code: 200,
    };
    let chunks: Vec<reqwest::Result<Bytes>> = vec![Ok(Bytes::from_static(payload))];
    let mut relay = RelayStream {
        inner: Box::pin(futures_util::stream::iter(chunks)),
        meter: StreamMeter::sse(),
        started: Instant::now(),
        first_content_at: None,
        bytes_sent: 0,
        finalizer: Some(finalizer),
        is_sse: true,
        idle_timeout: Duration::from_secs(120),
        idle: Box::pin(tokio::time::sleep(Duration::from_secs(120))),
    };

    use futures_util::StreamExt;
    assert!(relay.next().await.is_some());
    // The client disconnects here: dropping the stream aborts the upstream
    // response and records the partial spend.
    drop(relay);

    let successes = fake.successes.lock().unwrap();
    assert_eq!(successes.len(), 1);
    assert_eq!(successes[0].record.status, RequestStatus::Aborted);
    assert_eq!(successes[0].attempt.bytes_sent, payload.len() as u64);
    assert!(successes[0].attempt.billed);
    assert!(successes[0].attempt.ttft_ms.is_some());
}

/// A snapshot published while a request is in flight: that request settles on
/// the snapshot that admitted it, and a request started afterwards uses the new
/// one. This is the seam the apply engine will publish through.
#[tokio::test]
async fn a_published_snapshot_does_not_reprice_an_in_flight_request() {
    let (upstream_url, _mock, started, release) = spawn_held_mock().await;
    let config = priced_config(&upstream_url, 1.0, 2.0);
    let fake = fake_backend_with_config(config.clone());
    let addr = spawn_router(build_router_with(fake.clone())).await;

    // The first request is admitted, reaches the upstream and holds before its
    // first chunk: it is genuinely in flight.
    let client = reqwest::Client::new();
    let url = format!("{addr}/v1/chat/completions");
    let first = {
        let client = client.clone();
        let url = url.clone();
        tokio::spawn(async move {
            client
                .post(url)
                .json(&chat_body(true))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        })
    };
    started.await.unwrap();
    assert!(
        fake.successes.lock().unwrap().is_empty(),
        "the request must still be in flight when the snapshot is published"
    );

    // Publish a snapshot whose configured price is a hundred times higher.
    let second_config = priced_config(&upstream_url, 100.0, 200.0);
    fake.install_runtime(runtime_for(&second_config));
    release.send(()).unwrap();

    let relayed = first.await.unwrap();
    assert!(
        relayed.contains("[DONE]"),
        "the held stream must complete: {relayed}"
    );
    wait_for_successes(&fake, 1).await;

    let old_prices = table_for(&config);
    let new_prices = table_for(&second_config);
    let usage = fake.successes.lock().unwrap()[0].usage.clone();
    let at_admission = old_prices
        .estimate(
            "mock",
            "mock/model",
            &TierKey::Default,
            &usage,
            Decimal::ONE,
        )
        .expect("the first snapshot prices this model");
    let after_publish = new_prices
        .estimate(
            "mock",
            "mock/model",
            &TierKey::Default,
            &usage,
            Decimal::ONE,
        )
        .expect("the second snapshot prices this model");
    assert_ne!(
        at_admission, after_publish,
        "the published config must actually change the price"
    );

    {
        let successes = fake.successes.lock().unwrap();
        assert_eq!(
            successes[0].cost, at_admission,
            "the in-flight request must settle at the price that admitted it"
        );
        assert_eq!(
            successes[0].record.cost, at_admission,
            "the ledger row must carry the admission-time cost"
        );
    }

    // A request admitted after the publish uses the new price.
    let response = client
        .post(format!("{addr}/v1/chat/completions"))
        .json(&chat_body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.text().await.unwrap();
    wait_for_successes(&fake, 2).await;

    let successes = fake.successes.lock().unwrap();
    let later = &successes[1];
    let later_expected = new_prices
        .estimate(
            "mock",
            "mock/model",
            &TierKey::Default,
            &later.usage,
            Decimal::ONE,
        )
        .expect("the second snapshot prices this model");
    assert_eq!(
        later.cost, later_expected,
        "a request admitted after the publish uses the new price"
    );
}

// ------------------------------------------------------------- console

/// A console-enabled router on an ephemeral port. The listener binds first so
/// the console's Host allowlist pins the port the kernel actually gave us,
/// and the service carries peer addresses the guard can read.
async fn spawn_console_router<B>(backend: Arc<B>) -> String
where
    B: super::RequestBackend + super::OperatorBackend + 'static,
{
    spawn_console_router_with(backend, "/tmp/lgw-test-config.toml").await
}

/// The same router, with the configuration file path the config page describes.
async fn spawn_console_router_with<B>(backend: Arc<B>, config_path: &str) -> String
where
    B: super::RequestBackend + super::OperatorBackend + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let console = super::console::ConsoleContext::new(&addr.to_string(), addr.port(), config_path);
    let app = build_router_with_console(backend, console);
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn the_console_is_served_to_the_loopback_operator() {
    let addr = spawn_console_router(fake_backend("http://127.0.0.1:1")).await;
    let client = reqwest::Client::new();

    let page = client.get(format!("{addr}/")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(page.headers().get("x-frame-options").unwrap(), "DENY");
    assert_eq!(page.headers().get("cache-control").unwrap(), "no-store");
    assert!(
        page.headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("default-src 'self'")
    );
    let html = page.text().await.unwrap();
    assert!(html.contains("data-theme-toggle"));
    assert!(html.contains("data-poll"));
    assert!(html.contains("console/partials/board"));
    assert!(html.contains("accounts"), "the board table renders");
}

#[tokio::test]
async fn a_foreign_host_does_not_see_the_console() {
    let addr = spawn_console_router(fake_backend("http://127.0.0.1:1")).await;
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{addr}/"))
        .header(reqwest::header::HOST, "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
}

#[tokio::test]
async fn the_console_assets_are_embedded_and_typed() {
    let addr = spawn_console_router(fake_backend("http://127.0.0.1:1")).await;
    let client = reqwest::Client::new();

    let css = client
        .get(format!("{addr}/console/assets/console.css"))
        .send()
        .await
        .unwrap();
    assert_eq!(css.status(), 200);
    assert!(
        css.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
    assert!(css.text().await.unwrap().contains("--bg"));

    let missing = client
        .get(format!("{addr}/console/assets/not-a-file.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn the_console_speaks_both_languages_and_polls_one_renderer() {
    let addr = spawn_console_router(fake_backend("http://127.0.0.1:1")).await;
    let client = reqwest::Client::new();

    let chinese = client
        .get(format!("{addr}/?lang=zh"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(chinese.contains(r#"lang="zh""#));
    assert!(chinese.contains("账号与额度"));

    let partial = client
        .get(format!("{addr}/console/partials/board"))
        .send()
        .await
        .unwrap();
    assert_eq!(partial.status(), 200);
    let body = partial.text().await.unwrap();
    assert!(!body.contains("<html"), "the partial is not a whole page");
    assert!(body.contains("accounts"));
}

#[tokio::test]
async fn console_writes_need_json_and_the_console_header() {
    let addr = spawn_console_router(fake_backend("http://127.0.0.1:1")).await;
    let client = reqwest::Client::new();

    let no_json = client.post(format!("{addr}/")).send().await.unwrap();
    assert_eq!(no_json.status(), 415);

    let no_header = client
        .post(format!("{addr}/"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(no_header.status(), 403);

    let foreign_origin = client
        .post(format!("{addr}/"))
        .header("content-type", "application/json")
        .header("x-lgw-console", "1")
        .header("origin", "http://evil.example")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(foreign_origin.status(), 403);

    let method = client
        .post(format!("{addr}/"))
        .header("content-type", "application/json")
        .header("x-lgw-console", "1")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(method.status(), 405);
}

#[tokio::test]
async fn the_console_is_not_behind_the_local_key() {
    let mut config = test_config("http://127.0.0.1:1");
    config.server.local_key = Some(SecretRef::Literal("sk-local".into()));
    let backend = fake_backend_with_config(config);
    let addr = spawn_console_router(backend).await;
    let client = reqwest::Client::new();

    assert_eq!(
        client
            .get(format!("{addr}/"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(format!("{addr}/v1/usage"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{addr}/v1/usage"))
            .bearer_auth("sk-local")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn the_configuration_page_shows_the_file_with_literals_redacted() {
    let toml = r#"
[server]
listen = "127.0.0.1:8787"

[policy]
soft_threshold = 0.75

[providers.mock]
adapter = "none"
base_url = "http://127.0.0.1:1/v1"

[providers.mock.accounts.a1]
key = "sk-file-SECRET"
label = "file account"

[routes]
"mock/model" = "mock"

[prices.mock."mock/model"]
input = 0.15
output = 0.60
"#;
    let config = Config::from_toml_str(toml).expect("config");
    let accounts = config
        .build_accounts(&crate::secrets::CredentialStore::memory())
        .unwrap();
    let pool = Pool::new(accounts, &config.policy);
    activate(&pool, "mock/a1", 0);
    let fake = Arc::new(FakeBackend::new(config, pool));

    let dir = std::env::temp_dir().join(format!("lgw-config-page-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, toml).unwrap();

    let addr = spawn_console_router_with(fake, path.to_str().unwrap()).await;
    let client = reqwest::Client::new();
    let page = client.get(format!("{addr}/config")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("Configuration"));
    assert!(html.contains("server.listen"));
    assert!(html.contains("policy.soft_threshold"));
    assert!(html.contains("restart required"));
    assert!(html.contains("applies live"));
    assert!(html.contains("inline-literal"), "the key source is named");
    assert!(
        !html.contains("sk-file-SECRET"),
        "the literal value is never printed"
    );
    assert!(
        html.contains("&lt;redacted&gt;"),
        "the raw view carries a redacted value"
    );
    assert!(html.contains(&path.display().to_string()));

    let chinese = client
        .get(format!("{addr}/config?lang=zh"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(chinese.contains("配置"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unreadable_configuration_file_is_a_banner_not_a_crash() {
    let addr = spawn_console_router_with(
        fake_backend("http://127.0.0.1:1"),
        "/tmp/lgw-no-such-config-file.toml",
    )
    .await;
    let client = reqwest::Client::new();
    let page = client.get(format!("{addr}/config")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("could not be read"));
    assert!(
        !html.contains("raw TOML"),
        "no raw view without a readable file"
    );
}

const VALIDATE_TOML: &str = r#"[server]
listen = "127.0.0.1:8787"

[policy]
soft_threshold = 0.8

[providers.mock]
adapter = "none"
base_url = "http://127.0.0.1:1/v1"

[providers.mock.accounts.a1]
key = "sk-validate-SECRET"
label = "validated account"

[routes]
"mock/model" = "mock"
"#;

/// A console router whose configuration page describes a real temporary file,
/// with a backend built from the same configuration.
async fn validate_router() -> (String, std::path::PathBuf, std::path::PathBuf) {
    let config = Config::from_toml_str(VALIDATE_TOML).expect("valid config");
    let accounts = config
        .build_accounts(&crate::secrets::CredentialStore::memory())
        .unwrap();
    let pool = Pool::new(accounts, &config.policy);
    activate(&pool, "mock/a1", 0);
    let fake = Arc::new(FakeBackend::new(config, pool));

    // The timestamp alone collides when parallel tests call this in the same
    // clock tick, and then one test's cleanup deletes another's file.
    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = format!(
        "lgw-validate-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, VALIDATE_TOML).unwrap();

    let addr = spawn_console_router_with(fake, path.to_str().unwrap()).await;
    (addr, path, dir)
}

fn draft_body(value: &str) -> serde_json::Value {
    serde_json::json!({
        "fingerprint": crate::config_doc::fingerprint(VALIDATE_TOML),
        "draft": [{"path": ["policy", "soft_threshold"], "value": value}]
    })
}

#[tokio::test]
async fn a_valid_draft_shows_the_diff_and_writes_nothing() {
    let (addr, path, dir) = validate_router().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{addr}/console/api/config/validate?lang=en"))
        .header("x-lgw-console", "1")
        .json(&draft_body("0.75"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let html = response.text().await.unwrap();
    assert!(html.contains("-soft_threshold = 0.8"));
    assert!(html.contains("+soft_threshold = 0.75"));
    assert!(html.contains("Pending change"));

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(
        on_disk.contains("soft_threshold = 0.8"),
        "validate must not write"
    );
    assert!(!on_disk.contains("soft_threshold = 0.75"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_invalid_draft_is_a_field_error_in_both_languages() {
    let (addr, _path, dir) = validate_router().await;
    let client = reqwest::Client::new();
    let english = client
        .post(format!("{addr}/console/api/config/validate?lang=en"))
        .header("x-lgw-console", "1")
        .json(&draft_body("1.5"))
        .send()
        .await
        .unwrap();
    assert_eq!(english.status(), 422);
    let html = english.text().await.unwrap();
    assert!(html.contains("policy.soft_threshold"));
    assert!(html.contains("must be within 0.0..=1.0"));

    let chinese = client
        .post(format!("{addr}/console/api/config/validate?lang=zh"))
        .header("x-lgw-console", "1")
        .json(&draft_body("1.5"))
        .send()
        .await
        .unwrap();
    assert_eq!(chinese.status(), 422);
    let html = chinese.text().await.unwrap();
    assert!(html.contains("0.0 到 1.0"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_draft_from_an_older_file_asks_for_a_reload() {
    let (addr, _path, dir) = validate_router().await;
    let client = reqwest::Client::new();
    let stale = client
        .post(format!("{addr}/console/api/config/validate?lang=en"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({
            "fingerprint": "0000000000000000",
            "draft": [{"path": ["policy", "soft_threshold"], "value": "0.75"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    assert!(
        stale
            .text()
            .await
            .unwrap()
            .contains("changed after this page loaded")
    );

    let unknown = client
        .post(format!("{addr}/console/api/config/validate?lang=en"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({
            "fingerprint": crate::config_doc::fingerprint(VALIDATE_TOML),
            "draft": [{"path": ["policy", "made_up"], "value": "1"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 422);
    assert!(unknown.text().await.unwrap().contains("policy.made_up"));
    let _ = std::fs::remove_dir_all(&dir);
}

const APPLY_TOML: &str = r#"[server]
listen = "127.0.0.1:8787"

[policy]
soft_threshold = 0.8
max_retries = 2

[providers.mock]
adapter = "none"
base_url = "http://127.0.0.1:1/v1"

[providers.mock.accounts.a1]
key = "sk-apply-a1"

[providers.mock.accounts.a2]
key = "sk-apply-a2"

[routes]
"mock/model" = "mock"

[prices.mock."mock/model"]
input = 0.15
output = 0.60
"#;

/// A console router over a real configuration file, with a backend built from
/// the same file. Returns the base URL, the backend (so the test can see what
/// was published) and the file paths.
async fn apply_router() -> (
    String,
    Arc<FakeBackend>,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let config = Config::from_toml_str(APPLY_TOML).expect("valid config");
    let fake = fake_backend_with_config(config);
    let unique = format!(
        "lgw-apply-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, APPLY_TOML).unwrap();
    let addr = spawn_console_router_with(fake.clone(), path.to_str().unwrap()).await;
    (addr, fake, path, dir)
}

fn apply_body(path: &[&str], value: &str) -> serde_json::Value {
    serde_json::json!({
        "fingerprint": crate::config_doc::fingerprint(APPLY_TOML),
        "draft": [{"path": path, "value": value}]
    })
}

async fn post_apply(addr: &str, body: &serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{addr}/console/api/config/apply?lang=en"))
        .header("x-lgw-console", "1")
        .json(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn an_apply_writes_the_file_atomically_and_publishes_the_new_policy() {
    let (addr, fake, path, dir) = apply_router().await;
    let response = post_apply(&addr, &apply_body(&["policy", "soft_threshold"], "0.75")).await;
    assert_eq!(response.status(), 200);
    let html = response.text().await.unwrap();
    assert!(html.contains("Applied."));
    assert!(html.contains("+soft_threshold = 0.75"));
    assert!(html.contains("data-applied"));

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("soft_threshold = 0.75"));
    assert!(!on_disk.contains("soft_threshold = 0.8"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an applied configuration is private");
    }
    let published = fake
        .applied
        .lock()
        .unwrap()
        .clone()
        .expect("a snapshot was published");
    assert_eq!(published.policy.soft_threshold, 0.75);
    assert_eq!(fake.runtime().config().policy.soft_threshold, 0.75);

    let page = reqwest::get(format!("{addr}/config?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("value=\"0.75\""),
        "the page describes the applied file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_restart_only_apply_is_refused_by_name_and_writes_nothing() {
    let (addr, _fake, path, dir) = apply_router().await;
    let before = std::fs::read(&path).unwrap();
    let response = post_apply(&addr, &apply_body(&["policy", "max_body_bytes"], "1048576")).await;
    assert_eq!(response.status(), 422);
    let html = response.text().await.unwrap();
    assert!(html.contains("policy.max_body_bytes"));
    assert!(html.contains("need a restart"));
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_apply_of_a_section_without_a_live_path_is_refused_and_writes_nothing() {
    let (addr, _fake, path, dir) = apply_router().await;
    let before = std::fs::read(&path).unwrap();
    // A section the console does not manage has no live apply path; the
    // request is refused by name rather than written and ignored.
    let response = post_apply(&addr, &apply_body(&["no_such_section", "field"], "value")).await;
    assert_eq!(response.status(), 422);
    let html = response.text().await.unwrap();
    assert!(html.contains("no_such_section.field"));
    assert!(html.contains("cannot be applied"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_draft_cannot_write_a_secret_into_the_configuration() {
    let (addr, fake, path, dir) = apply_router().await;
    let before = std::fs::read(&path).unwrap();
    let response = post_apply(
        &addr,
        &apply_body(
            &["providers", "mock", "accounts", "a1", "key"],
            "sk-written-through-a-draft",
        ),
    )
    .await;
    assert_eq!(response.status(), 422);
    let html = response.text().await.unwrap();
    assert!(html.contains("secret"), "got {html}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
    assert_eq!(
        fake.applied_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "nothing was published"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_stale_apply_leaves_the_newer_file_untouched() {
    let (addr, _fake, path, dir) = apply_router().await;
    let newer = format!("{APPLY_TOML}\n# written by another editor\n");
    std::fs::write(&path, &newer).unwrap();
    let response = post_apply(&addr, &apply_body(&["policy", "soft_threshold"], "0.75")).await;
    assert_eq!(response.status(), 409);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("changed after this page loaded")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_applies_at_once_end_in_one_consistent_configuration() {
    let (addr, fake, path, dir) = apply_router().await;
    let left_body = apply_body(&["policy", "soft_threshold"], "0.7");
    let right_body = apply_body(&["policy", "soft_threshold"], "0.6");
    let (left, right) = tokio::join!(
        post_apply(&addr, &left_body),
        post_apply(&addr, &right_body),
    );
    let mut statuses = [left.status().as_u16(), right.status().as_u16()];
    statuses.sort();
    assert_eq!(
        statuses,
        [200, 409],
        "one apply wins and one is refused as stale"
    );

    let on_disk = std::fs::read_to_string(&path).unwrap();
    let published = fake.applied.lock().unwrap().clone().expect("one snapshot");
    assert_eq!(
        published.policy.soft_threshold,
        if on_disk.contains("soft_threshold = 0.7") {
            0.7
        } else {
            0.6
        },
        "the file and the running snapshot agree"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_apply_during_a_stream_keeps_the_in_flight_request_on_its_snapshot() {
    let (upstream_url, _mock, started, release) = spawn_held_mock().await;
    let toml = priced_toml(&upstream_url, 1.0, 2.0);
    let config = Config::from_toml_str(&toml).unwrap();
    let fake = fake_backend_with_config(config.clone());

    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "lgw-apply-stream-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, &toml).unwrap();
    let addr = spawn_console_router_with(fake.clone(), path.to_str().unwrap()).await;

    let client = reqwest::Client::new();
    let first = {
        let client = client.clone();
        let url = format!("{addr}/v1/chat/completions");
        tokio::spawn(async move {
            client
                .post(url)
                .json(&chat_body(true))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        })
    };
    started.await.unwrap();
    assert!(fake.successes.lock().unwrap().is_empty(), "still in flight");

    let response = reqwest::Client::new()
        .post(format!("{addr}/console/api/config/apply?lang=en"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({
            "fingerprint": crate::config_doc::fingerprint(&toml),
            "draft": [{"path": ["policy", "max_retries"], "value": "5"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        fake.runtime().config().policy.max_retries,
        5,
        "the new snapshot is live while the stream is still open"
    );

    release.send(()).unwrap();
    let relayed = first.await.unwrap();
    assert!(
        relayed.contains("[DONE]"),
        "the held stream completes: {relayed}"
    );
    wait_for_successes(&fake, 1).await;
    let old_prices = table_for(&config);
    let usage = fake.successes.lock().unwrap()[0].usage.clone();
    let at_admission = old_prices
        .estimate(
            "mock",
            "mock/model",
            &TierKey::Default,
            &usage,
            Decimal::ONE,
        )
        .expect("the admitting snapshot prices this model");
    assert_eq!(
        fake.successes.lock().unwrap()[0].cost,
        at_admission,
        "the in-flight request settles on the snapshot that admitted it"
    );

    let later = client
        .post(format!("{addr}/v1/chat/completions"))
        .json(&chat_body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(later.status(), 200);
    let _ = later.text().await.unwrap();
    wait_for_successes(&fake, 2).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_invalid_apply_changes_neither_the_file_nor_the_snapshot() {
    let (addr, fake, path, dir) = apply_router().await;
    let before = std::fs::read(&path).unwrap();
    let response = post_apply(&addr, &apply_body(&["policy", "soft_threshold"], "1.5")).await;
    assert_eq!(response.status(), 422);
    let html = response.text().await.unwrap();
    assert!(html.contains("policy.soft_threshold"));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the file is untouched"
    );
    assert!(
        fake.applied.lock().unwrap().is_none(),
        "no snapshot was published"
    );
    assert_eq!(fake.runtime().config().policy.soft_threshold, 0.8);
    let _ = std::fs::remove_dir_all(&dir);
}

fn compiled_body(changes: &[(&[&str], &str)]) -> serde_json::Value {
    let draft: Vec<serde_json::Value> = changes
        .iter()
        .map(|(path, value)| {
            serde_json::json!({
                "path": path,
                "value": value,
            })
        })
        .collect();
    serde_json::json!({
        "fingerprint": crate::config_doc::fingerprint(APPLY_TOML),
        "draft": draft,
    })
}

#[tokio::test]
async fn applying_an_account_edit_keeps_its_readings_and_sticky_state() {
    let (addr, fake, path, dir) = apply_router().await;
    let before = fake
        .pool
        .snapshot()
        .into_iter()
        .find(|a| a.id == "a1")
        .expect("a1 is in the pool");
    assert_eq!(before.gates[0].used, Decimal::from(1));

    let response = client_apply(
        &addr,
        &compiled_body(&[
            (&["providers", "mock", "accounts", "a1", "label"], "renamed"),
            (&["providers", "mock", "accounts", "a1", "rpm_limit"], "5"),
        ]),
    )
    .await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("label = \"renamed\""));
    assert!(on_disk.contains("rpm_limit = 5"));

    let after = fake
        .pool
        .snapshot()
        .into_iter()
        .find(|a| a.id == "a1")
        .expect("a1 stays in the pool");
    assert_eq!(after.label, "renamed");
    assert_eq!(after.rpm_limit, Some(5));
    assert_eq!(after.state, crate::domain::AccountState::Active);
    assert_eq!(after.gates[0].used, Decimal::from(1), "readings survive");
    let published = fake.applied.lock().unwrap().clone().expect("published");
    let account = &published.providers["mock"].accounts["a1"];
    assert_eq!(account.label.as_deref(), Some("renamed"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn changing_a_provider_endpoint_resets_its_accounts_to_unknown() {
    let (addr, fake, path, dir) = apply_router().await;
    let response = client_apply(
        &addr,
        &compiled_body(&[(&["providers", "mock", "base_url"], "http://127.0.0.1:2/v1")]),
    )
    .await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("base_url = \"http://127.0.0.1:2/v1\"")
    );
    for account in fake.pool.snapshot() {
        assert_eq!(
            account.state,
            crate::domain::AccountState::Unknown,
            "a reading from the old endpoint must not serve the new one"
        );
        assert!(account.gates.is_empty());
    }
    assert_eq!(
        fake.runtime().config().providers["mock"].base_url,
        "http://127.0.0.1:2/v1"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_invalid_account_edit_leaves_the_pool_and_file_unchanged() {
    let (addr, fake, path, dir) = apply_router().await;
    let before_file = std::fs::read(&path).unwrap();
    let before_pool = fake.pool.snapshot();
    let response = client_apply(
        &addr,
        &compiled_body(&[(
            &["providers", "mock", "accounts", "a1", "rpm_limit"],
            "many",
        )]),
    )
    .await;
    assert_eq!(response.status(), 422);
    let html = response.text().await.unwrap();
    assert!(html.contains("providers.mock.accounts.a1.rpm_limit"));
    assert!(html.contains("not an integer"));
    assert_eq!(std::fs::read(&path).unwrap(), before_file);
    assert!(fake.applied.lock().unwrap().is_none());
    let after_pool = fake.pool.snapshot();
    assert_eq!(after_pool.len(), before_pool.len());
    for (before, after) in before_pool.iter().zip(after_pool.iter()) {
        assert_eq!(before.gates.len(), after.gates.len());
        for (left, right) in before.gates.iter().zip(after.gates.iter()) {
            assert_eq!(left.kind, right.kind);
            assert_eq!(left.used, right.used);
            assert_eq!(left.cap, right.cap);
        }
        assert_eq!(before.state, after.state);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

async fn client_apply(addr: &str, body: &serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{addr}/console/api/config/apply?lang=en"))
        .header("x-lgw-console", "1")
        .json(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn an_account_can_be_added_through_apply_and_is_unknown_until_probed() {
    let (addr, fake, path, dir) = apply_router().await;
    let key_file = dir.join("new-account.key");
    std::fs::write(&key_file, "sk-added-account\n").unwrap();

    let body = serde_json::json!({
        "fingerprint": crate::config_doc::fingerprint(APPLY_TOML),
        "draft": [{
            "action": "add_account",
            "provider": "mock",
            "id": "a3",
            "label": "third",
            "key_source": "file",
            "key_value": key_file.display().to_string()
        }]
    });
    let response = client_apply(&addr, &body).await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("a3"));
    assert!(on_disk.contains("key_file = "));
    assert!(on_disk.contains("label = \"third\""));

    let added = fake
        .pool
        .snapshot()
        .into_iter()
        .find(|a| a.id == "a3")
        .expect("the account is in the pool");
    assert_eq!(added.state, crate::domain::AccountState::Unknown);
    assert!(added.gates.is_empty());
    assert_eq!(added.label, "third");
    let published = fake.applied.lock().unwrap().clone().expect("published");
    assert!(published.providers["mock"].accounts.contains_key("a3"));

    let chosen = fake
        .pool
        .select("mock", "", None, Decimal::ONE, "r", 0, &[])
        .expect("existing accounts still serve");
    assert_ne!(
        chosen.plan().account_id,
        "a3",
        "an unknown account is not selected"
    );
    chosen.settle(Decimal::ZERO, false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_account_can_be_removed_through_apply() {
    let (addr, fake, path, dir) = apply_router().await;
    assert!(
        fake.pool.snapshot().iter().any(|a| a.id == "a2"),
        "a2 starts in the pool"
    );
    let body = serde_json::json!({
        "fingerprint": crate::config_doc::fingerprint(APPLY_TOML),
        "draft": [{
            "action": "remove",
            "path": ["providers", "mock", "accounts", "a2"]
        }]
    });
    let response = client_apply(&addr, &body).await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(!on_disk.contains("sk-apply-a2"));
    assert!(!on_disk.contains("a2"));
    assert!(
        fake.pool.snapshot().iter().all(|a| a.id != "a2"),
        "a settled account is out of the running pool"
    );
    let published = fake.applied.lock().unwrap().clone().expect("published");
    assert!(!published.providers["mock"].accounts.contains_key("a2"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_configuration_page_offers_account_add_and_remove() {
    let (addr, _fake, _path, dir) = apply_router().await;
    let html = reqwest::get(format!("{addr}/config?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("data-add-account"));
    assert!(html.contains("data-account-remove"));
    assert!(html.contains("Add an account"));
    assert!(html.contains("Remove account"));
    let _ = std::fs::remove_dir_all(&dir);
}

const ROUTES_TOML: &str = r#"[server]
listen = "127.0.0.1:8787"

[policy]
max_retries = 2

[providers.mock]
adapter = "none"
base_url = "http://127.0.0.1:1/v1"

[providers.mock.accounts.a1]
key = "sk-mock-a1"

[providers.alt]
adapter = "none"
base_url = "http://127.0.0.1:2/v1"

[providers.alt.accounts.a1]
key = "sk-alt-a1"

[routes]
"mock/model" = "mock"

[provider_prefix_aliases]
"alias" = "mock"

[prices.mock."mock/model"]
input = 0.15
output = 0.60

[[prices.mock."mock/model".schedules]]
name = "peak"
days = "*"
hours = "00:00-23:59"
tz = "UTC"
input = 0.30
output = 1.20
"#;

async fn routes_router() -> (
    String,
    Arc<FakeBackend>,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let config = Config::from_toml_str(ROUTES_TOML).expect("valid config");
    let fake = fake_backend_with_config(config);
    let dir = std::env::temp_dir().join(format!(
        "lgw-routes-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, ROUTES_TOML).unwrap();
    let addr = spawn_console_router_with(fake.clone(), path.to_str().unwrap()).await;
    (addr, fake, path, dir)
}

fn structural_body(draft: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "fingerprint": crate::config_doc::fingerprint(ROUTES_TOML),
        "draft": draft,
    })
}

#[tokio::test]
async fn a_route_is_remapped_for_new_requests_only() {
    let (addr, fake, _path, dir) = routes_router().await;
    let admitted_before = fake.runtime();
    assert_eq!(
        admitted_before
            .resolve("mock/model")
            .expect("route")
            .provider,
        "mock"
    );

    let body = structural_body(vec![serde_json::json!({
        "action": "set",
        "path": ["routes", "mock/model"],
        "value": "alt"
    })]);
    assert_eq!(client_apply(&addr, &body).await.status(), 200);

    assert_eq!(
        fake.runtime()
            .resolve("mock/model")
            .expect("route")
            .provider,
        "alt",
        "a new request resolves the new mapping"
    );
    assert_eq!(
        admitted_before.resolve("mock/model").unwrap().provider,
        "mock",
        "the snapshot that admitted an in-flight request keeps its mapping"
    );

    // Add and then remove a route; resolution follows the file exactly.
    let current = std::fs::read_to_string(addr_path(&dir)).unwrap();
    let added = serde_json::json!({
        "fingerprint": crate::config_doc::fingerprint(&current),
        "draft": [{
            "action": "add_route",
            "model": "mock/extra",
            "provider": "mock"
        }]
    });
    assert_eq!(client_apply(&addr, &added).await.status(), 200);
    assert_eq!(
        fake.runtime().resolve("mock/extra").unwrap().provider,
        "mock"
    );
    let fingerprint =
        crate::config_doc::fingerprint(&std::fs::read_to_string(addr_path(&dir)).unwrap());
    let removed = serde_json::json!({
        "fingerprint": fingerprint,
        "draft": [{
            "action": "remove_route",
            "model": "mock/extra"
        }]
    });
    assert_eq!(client_apply(&addr, &removed).await.status(), 200);
    assert!(fake.runtime().resolve("mock/extra").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

fn addr_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("config.toml")
}

#[tokio::test]
async fn an_alias_and_a_price_edit_apply_to_the_next_request() {
    let (addr, fake, _path, dir) = routes_router().await;
    let before_price = fake
        .runtime()
        .prices()
        .estimate(
            "mock",
            "mock/model",
            &TierKey::Default,
            &TokenUsage {
                prompt_tokens: 1_000,
                completion_tokens: 0,
                cached_tokens: 0,
                reasoning_tokens: 0,
                measured: true,
            },
            Decimal::ONE,
        )
        .expect("configured price");

    let body = structural_body(vec![
        serde_json::json!({
            "action": "set",
            "path": ["prices", "mock", "mock/model", "input"],
            "value": "0.9"
        }),
        serde_json::json!({
            "action": "add_alias",
            "alias": "shorty",
            "provider": "alt"
        }),
    ]);
    assert_eq!(client_apply(&addr, &body).await.status(), 200);

    let resolved = fake.runtime().resolve("shorty/any-model").expect("alias");
    assert_eq!(resolved.provider, "alt");
    assert_eq!(resolved.model, "any-model");

    let after_price = fake
        .runtime()
        .prices()
        .estimate(
            "mock",
            "mock/model",
            &TierKey::Default,
            &TokenUsage {
                prompt_tokens: 1_000,
                completion_tokens: 0,
                cached_tokens: 0,
                reasoning_tokens: 0,
                measured: true,
            },
            Decimal::ONE,
        )
        .expect("re-seeded price");
    assert!(after_price > before_price, "the configured price moved");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_schedule_can_be_removed_and_added_for_the_next_request() {
    let (addr, fake, _dir_path, dir) = routes_router().await;
    let at = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    assert_eq!(
        fake.runtime().tier_at("mock", "mock/model", at),
        TierKey::Named("peak".to_string())
    );

    let removed = structural_body(vec![serde_json::json!({
        "action": "remove_schedule",
        "provider": "mock",
        "model": "mock/model",
        "index": 0
    })]);
    assert_eq!(client_apply(&addr, &removed).await.status(), 200);
    assert_eq!(
        fake.runtime().tier_at("mock", "mock/model", at),
        TierKey::Default,
        "a removed schedule stops applying"
    );

    let path_now = std::fs::read_to_string(addr_path(&dir)).unwrap();
    let added = serde_json::json!({
        "fingerprint": crate::config_doc::fingerprint(&path_now),
        "draft": [{
            "action": "add_schedule",
            "provider": "mock",
            "model": "mock/model",
            "name": "all-day",
            "days": "*",
            "hours": "00:00-23:59",
            "tz": "UTC",
            "input": 0.2,
            "output": 0.8
        }]
    });
    assert_eq!(client_apply(&addr, &added).await.status(), 200);
    assert_eq!(
        fake.runtime().tier_at("mock", "mock/model", at),
        TierKey::Named("all-day".to_string())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_failed_route_change_leaves_routes_and_prices_alone() {
    let (addr, fake, path, dir) = routes_router().await;
    let before = std::fs::read(&path).unwrap();
    let body = structural_body(vec![serde_json::json!({
        "action": "set",
        "path": ["routes", "mock/model"],
        "value": "no-such-provider"
    })]);
    let response = client_apply(&addr, &body).await;
    assert_eq!(response.status(), 422);
    assert!(response.text().await.unwrap().contains("routes.mock/model"));
    assert_eq!(
        fake.runtime().resolve("mock/model").unwrap().provider,
        "mock"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_configuration_page_exposes_routes_aliases_and_prices() {
    let (addr, _fake, _path, dir) = routes_router().await;
    let html = reqwest::get(format!("{addr}/config?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("data-remove-action"));
    assert!(html.contains("Add a route"));
    assert!(html.contains("Add an alias"));
    assert!(html.contains("Add a price"));
    assert!(html.contains("Add a schedule"));
    assert!(html.contains("Provider aliases"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A console router plus the shared watcher state, over a real file.
async fn watcher_setup(
    toml: &str,
) -> (
    String,
    super::console::ConsoleContext,
    Arc<FakeBackend>,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let config = Config::from_toml_str(toml).expect("valid config");
    let fake = fake_backend_with_config(config);
    let dir = std::env::temp_dir().join(format!(
        "lgw-watch-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, toml).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let console =
        super::console::ConsoleContext::new(&addr.to_string(), addr.port(), path.to_str().unwrap());
    crate::reload::mark_loaded(
        &console.reload,
        &crate::config_doc::fingerprint(toml),
        crate::domain::now(),
    );
    let app = super::build_router_with_console(fake.clone(), console.clone());
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let base = format!("http://{addr}");
    (base, console, fake, path, dir)
}

fn replace_config_file(path: &std::path::Path, contents: &str) {
    let temporary = path.with_extension("editor-save");
    std::fs::write(&temporary, contents).unwrap();
    std::fs::rename(&temporary, path).unwrap();
}

#[tokio::test]
async fn an_external_edit_is_applied_once_through_the_watcher() {
    let (base, console, fake, path, dir) = watcher_setup(APPLY_TOML).await;
    assert_eq!(
        fake.applied_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );

    let edited = APPLY_TOML.replace("soft_threshold = 0.8", "soft_threshold = 0.75");
    replace_config_file(&path, &edited);
    let outcome = crate::reload::reload_once(fake.as_ref(), &path, &console).await;
    assert!(
        matches!(outcome, crate::reload::Outcome::Applied { .. }),
        "got {outcome:?}"
    );
    assert_eq!(fake.runtime().config().policy.soft_threshold, 0.75);
    assert_eq!(
        fake.applied_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    let again = crate::reload::reload_once(fake.as_ref(), &path, &console).await;
    assert_eq!(again, crate::reload::Outcome::Unchanged);
    assert_eq!(
        fake.applied_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "unchanged bytes are never applied twice"
    );

    let page = reqwest::get(format!("{base}/config?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("value=\"0.75\""));
    assert!(!page.contains("An external edit could not be applied"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_invalid_external_edit_keeps_serving_and_surfaces_the_failure() {
    let (base, console, fake, path, dir) = watcher_setup(APPLY_TOML).await;
    let broken = APPLY_TOML.replace("soft_threshold = 0.8", "soft_threshold = 1.5");
    replace_config_file(&path, &broken);
    let outcome = crate::reload::reload_once(fake.as_ref(), &path, &console).await;
    assert!(matches!(outcome, crate::reload::Outcome::Rejected { .. }));
    assert_eq!(
        fake.runtime().config().policy.soft_threshold,
        0.8,
        "the previous snapshot keeps serving"
    );
    assert_eq!(
        fake.applied_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );

    let page = reqwest::get(format!("{base}/config?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("An external edit could not be applied"));
    assert!(page.contains("policy.soft_threshold"));

    // Fixing the file is enough: no restart, no console action.
    let fixed = APPLY_TOML.replace("soft_threshold = 0.8", "soft_threshold = 0.7");
    replace_config_file(&path, &fixed);
    let outcome = crate::reload::reload_once(fake.as_ref(), &path, &console).await;
    assert!(matches!(outcome, crate::reload::Outcome::Applied { .. }));
    assert_eq!(fake.runtime().config().policy.soft_threshold, 0.7);
    let page = reqwest::get(format!("{base}/config?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!page.contains("An external edit could not be applied"));
    assert!(page.contains("value=\"0.7\""));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_restart_only_external_edit_is_refused_by_name() {
    let (_base, console, fake, path, dir) = watcher_setup(APPLY_TOML).await;
    let changed = APPLY_TOML.replace("listen = \"127.0.0.1:8787\"", "listen = \"127.0.0.1:9999\"");
    replace_config_file(&path, &changed);
    let outcome = crate::reload::reload_once(fake.as_ref(), &path, &console).await;
    let crate::reload::Outcome::Rejected { message, .. } = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert!(message.contains("server.listen"), "got {message}");
    assert_eq!(
        fake.runtime().config().server.listen,
        "127.0.0.1:8787",
        "the listener cannot change without a restart"
    );
    assert_eq!(
        fake.applied_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_console_save_is_not_applied_again_by_the_watcher() {
    let (base, console, fake, path, dir) = watcher_setup(APPLY_TOML).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/console/api/config/apply?lang=en"))
        .header("x-lgw-console", "1")
        .json(&apply_body(&["policy", "soft_threshold"], "0.75"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert_eq!(
        fake.applied_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    let outcome = crate::reload::reload_once(fake.as_ref(), &path, &console).await;
    assert_eq!(outcome, crate::reload::Outcome::Unchanged);
    assert_eq!(
        fake.applied_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the console's own write is not a second apply"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn usage_fixture() -> crate::store::UsageReport {
    use crate::store::{LatencySummary, UsageFullRow, UsageReport};
    let money = |text: &str| text.parse::<Decimal>().expect("decimal");
    let mut usd = UsageFullRow {
        key: "xkiro".to_string(),
        currency: "USD".to_string(),
        requests: 3,
        failed: 1,
        covered_requests: 2,
        metered_requests: 1,
        input_tokens: 1_200,
        output_tokens: 300,
        cached_tokens: 600,
        covered_cost: money("0.75"),
        metered_cost: money("0.5"),
        other_cost: Decimal::ZERO,
    };
    let mut cny = UsageFullRow {
        key: "deepseek".to_string(),
        currency: "CNY".to_string(),
        requests: 2,
        failed: 0,
        covered_requests: 0,
        metered_requests: 2,
        input_tokens: 400,
        output_tokens: 100,
        cached_tokens: 0,
        covered_cost: Decimal::ZERO,
        metered_cost: money("0.25"),
        other_cost: Decimal::ZERO,
    };
    usd.requests = 3;
    cny.requests = 2;
    UsageReport {
        rows: vec![usd, cny],
        daily: vec![UsageFullRow {
            key: "2026-10-18".to_string(),
            currency: "USD".to_string(),
            requests: 5,
            ..UsageFullRow::default()
        }],
        latency: LatencySummary {
            p50_ttft_ms: Some(120),
            p95_ttft_ms: Some(1_400),
            samples: 42,
        },
        latency_since: None,
        latency_truncated: true,
    }
}

#[tokio::test]
async fn the_usage_page_shows_the_same_numbers_in_both_languages() {
    let fake = fake_backend("http://127.0.0.1:1");
    *fake.usage.lock().unwrap() = Some(usage_fixture());
    let addr = spawn_console_router(fake.clone()).await;

    let english = reqwest::get(format!("{addr}/usage?range=7d&group=provider&lang=en"))
        .await
        .unwrap();
    assert_eq!(english.status(), 200);
    let english = english.text().await.unwrap();
    assert!(english.contains("Covered spend"));
    assert!(english.contains("Metered spend"));
    assert!(english.contains("0.75"));
    assert!(english.contains("0.5"));
    assert!(english.contains("0.25"));
    assert!(
        !english.contains("1.25"),
        "currencies and coverage never mix"
    );
    assert!(english.contains("p95 TTFT"));
    assert!(english.contains("1.4s"));
    assert!(english.contains("last 30 days"));
    assert!(english.contains("range=30d"), "range controls are links");

    let chinese = reqwest::get(format!("{addr}/usage?range=30d&group=model&lang=zh"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(chinese.contains("用量"));
    for number in ["0.75", "0.5", "0.25", "1.4s", "42"] {
        assert!(chinese.contains(number), "ZH missing {number}");
    }
    assert!(chinese.contains("30 天"));
}

#[tokio::test]
async fn the_usage_page_says_when_there_is_no_ledger() {
    let addr = spawn_console_router(fake_backend("http://127.0.0.1:1")).await;
    let html = reqwest::get(format!("{addr}/usage?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("No ledger yet"));
    assert!(!html.contains("Covered spend"));
}

fn requests_fixture() -> Vec<crate::store::RequestHistory> {
    use crate::domain::{GateClass, RequestStatus, RequestSummary};
    use crate::store::{AttemptDetail, RequestHistory};
    let at = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let money = |text: &str| text.parse::<Decimal>().expect("decimal");
    let request =
        |id: &str, coverage: GateClass, status: RequestStatus, retries: u32, cost: &str| {
            RequestSummary {
                ts: at,
                request_id: id.to_string(),
                provider: "xkiro".to_string(),
                account_id: "acc-1".to_string(),
                model: "z-ai/glm-5.3-flash".to_string(),
                input_tokens: 1_200,
                output_tokens: 300,
                cached_tokens: 1_024,
                ttft_ms: Some(240),
                tps: Some(45.2),
                coverage,
                status,
                cost: money(cost),
                currency: "USD".to_string(),
                retries,
            }
        };
    let attempt = |no: u32, code: Option<u16>, class: Option<&str>| AttemptDetail {
        attempt_no: no,
        account_id: "acc-1".to_string(),
        provider: "xkiro".to_string(),
        started_at: at + time::Duration::seconds(i64::from(no)),
        ttft_ms: if class.is_some() { None } else { Some(240) },
        status_code: code,
        error_class: class.map(str::to_string),
        retry_after_ms: None,
        billed: class.is_none(),
    };
    vec![
        RequestHistory {
            request: request("r1", GateClass::Covered, RequestStatus::Ok, 1, "0.25"),
            attempts: vec![
                attempt(1, Some(429), Some("quota_exhausted")),
                attempt(2, Some(200), None),
            ],
        },
        RequestHistory {
            request: request("r2", GateClass::Metered, RequestStatus::Error, 0, "0.5"),
            attempts: vec![attempt(1, Some(402), Some("quota_exhausted"))],
        },
    ]
}

#[tokio::test]
async fn the_requests_page_shows_attempts_and_filters() {
    let fake = fake_backend("http://127.0.0.1:1");
    *fake.history.lock().unwrap() = Some(requests_fixture());
    let addr = spawn_console_router(fake.clone()).await;

    let english = reqwest::get(format!("{addr}/requests?lang=en&limit=50"))
        .await
        .unwrap();
    assert_eq!(english.status(), 200);
    let english = english.text().await.unwrap();
    assert!(english.contains("quota_exhausted"));
    assert!(english.contains("2 / 2"));
    assert!(english.contains("retried"));
    assert!(english.contains("0.25 USD"));
    assert!(english.contains("0.5 USD"));
    assert!(english.contains("metered"));
    assert!(english.contains("errors only"));
    assert!(english.contains("coverage=metered"), "filters are links");

    let chinese = reqwest::get(format!(
        "{addr}/requests?errors=1&coverage=metered&limit=25&lang=zh"
    ))
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
    for number in ["0.25 USD", "0.5 USD", "1.2K / 300"] {
        assert!(chinese.contains(number), "ZH missing {number}");
    }
    assert!(chinese.contains("请求"));
}

#[tokio::test]
async fn the_requests_page_explains_an_empty_ledger() {
    let addr = spawn_console_router(fake_backend("http://127.0.0.1:1")).await;
    let html = reqwest::get(format!("{addr}/requests?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("No ledger yet"));
    assert!(!html.contains("<table"));
}

fn fake_with_learned_price() -> Arc<FakeBackend> {
    let config = Config::from_toml_str(APPLY_TOML).expect("config");
    let fake = fake_backend_with_config(config.clone());
    let mut table = (*fake.runtime().prices()).clone();
    table.set(
        "mock",
        "mock/model",
        &TierKey::Default,
        crate::meter::ModelPrice {
            input_per_token: "0.0000009".parse().unwrap(),
            output_per_token: "0.0000009".parse().unwrap(),
            cache_read_per_token: "0.00000009".parse().unwrap(),
            samples: 4,
        },
    );
    fake.install_runtime(Arc::new(
        Runtime::new(Arc::new(config), Arc::new(table)).unwrap(),
    ));
    fake
}

#[tokio::test]
async fn the_prices_page_shows_both_rates_and_forgets_them() {
    let fake = fake_with_learned_price();
    let addr = spawn_console_router(fake.clone()).await;

    let page = reqwest::get(format!("{addr}/prices?lang=en"))
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let page = page.text().await.unwrap();
    assert!(page.contains("learned"));
    assert!(page.contains("configured"));
    assert!(page.contains("Forget model"));
    assert!(page.contains("data-forget"));
    assert!(page.contains("mock/model"));

    let chinese = reqwest::get(format!("{addr}/prices?lang=zh"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(chinese.contains("价格"));
    assert!(chinese.contains("mock/model"));

    let response = reqwest::Client::new()
        .post(format!("{addr}/console/api/prices/forget"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({ "provider": "mock", "model": "mock/model" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["forgotten"], 1);

    let runtime = fake.runtime();
    let after = runtime
        .prices()
        .entry("mock", "mock/model", &TierKey::Default)
        .expect("entry");
    assert!(after.learned.is_none(), "the learned rate is forgotten");
    assert!(after.configured.is_some(), "the configured card is back");

    let page = reqwest::get(format!("{addr}/prices?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("configured"));
    assert!(!page.contains("Forget model"), "nothing is learned now");
}

#[tokio::test]
async fn forgetting_prices_needs_the_console_guards() {
    let fake = fake_with_learned_price();
    let addr = spawn_console_router(fake.clone()).await;
    // No custom header: the mutation guard refuses it before it reaches the
    // backend, so the learned rate is untouched.
    let response = reqwest::Client::new()
        .post(format!("{addr}/console/api/prices/forget"))
        .json(&serde_json::json!({ "provider": "mock" }))
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), 200);
    let runtime = fake.runtime();
    let entry = runtime
        .prices()
        .entry("mock", "mock/model", &TierKey::Default)
        .expect("entry");
    assert!(entry.learned.is_some(), "nothing was forgotten");
}

#[tokio::test]
async fn a_model_scope_without_a_provider_is_refused() {
    let addr = spawn_console_router(fake_with_learned_price()).await;
    let response = reqwest::Client::new()
        .post(format!("{addr}/console/api/prices/forget"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({ "model": "mock/model" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
}

#[tokio::test]
async fn a_page_with_no_prices_says_so() {
    let addr = spawn_console_router(fake_backend("http://127.0.0.1:1")).await;
    let html = reqwest::get(format!("{addr}/prices?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("No configured or learned prices yet") || html.contains("No ledger yet"));
}

#[tokio::test]
async fn the_board_offers_probe_and_disable_actions() {
    let addr = spawn_console_router(fake_backend("http://127.0.0.1:1")).await;
    let html = reqwest::get(format!("{addr}/?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("data-account-probe"));
    assert!(html.contains("data-account-disable"));
    assert!(html.contains("data-board-result"));
    assert!(html.contains("Probe"));
    assert!(html.contains("Disable"));
}

#[tokio::test]
async fn probing_and_operator_state_work_through_the_console() {
    let fake = fake_backend("http://127.0.0.1:1");
    let addr = spawn_console_router(fake.clone()).await;
    let client = reqwest::Client::new();

    let probed = client
        .post(format!("{addr}/console/api/accounts/probe"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(probed.status(), 200);
    for account in fake.pool.snapshot() {
        assert_eq!(
            account.gates.first().map(|gate| gate.used),
            Some(Decimal::from(5)),
            "probe all refreshed every account"
        );
    }

    let disabled = client
        .post(format!("{addr}/console/api/accounts/state"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({
            "provider": "mock", "account": "a1", "enabled": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(disabled.status(), 200);
    let state = fake.pool.account("mock/a1").expect("account").state;
    assert_eq!(state, crate::domain::AccountState::Disabled);

    // A successful probe must not lift the operator's decision.
    let probe_one = client
        .post(format!("{addr}/console/api/accounts/probe"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({ "provider": "mock", "account": "a1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(probe_one.status(), 200);
    let account = fake.pool.account("mock/a1").expect("account");
    assert_eq!(account.state, crate::domain::AccountState::Disabled);
    assert_eq!(
        account.last_error.as_deref(),
        Some(crate::accounts::OPERATOR_DISABLED)
    );

    let enabled = client
        .post(format!("{addr}/console/api/accounts/state"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({
            "provider": "mock", "account": "a1", "enabled": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(enabled.status(), 200);
    assert_eq!(
        fake.pool.account("mock/a1").unwrap().state,
        crate::domain::AccountState::Unknown,
        "enable does not trust the account yet"
    );

    let probe_again = client
        .post(format!("{addr}/console/api/accounts/probe"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({ "provider": "mock", "account": "a1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(probe_again.status(), 200);
    assert_eq!(
        fake.pool.account("mock/a1").unwrap().state,
        crate::domain::AccountState::Active,
        "only a successful probe makes it serve again"
    );
}

#[tokio::test]
async fn a_failing_or_incomplete_probe_is_reported_without_a_hang() {
    let addr = spawn_console_router(fake_backend("http://127.0.0.1:1")).await;
    let client = reqwest::Client::new();

    let failed = client
        .post(format!("{addr}/console/api/accounts/probe"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({ "provider": "mock", "account": "broken" }))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 502);
    let body: serde_json::Value = failed.json().await.unwrap();
    let error = body["error"].as_str().expect("error text");
    assert!(error.contains("connection refused"));
    assert!(!error.contains('\n'));

    let incomplete = client
        .post(format!("{addr}/console/api/accounts/probe"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({ "provider": "mock" }))
        .send()
        .await
        .unwrap();
    assert_eq!(incomplete.status(), 422);

    // The mutation guard refuses a request that does not carry the console
    // header, before any provider is contacted.
    let unguarded = client
        .post(format!("{addr}/console/api/accounts/state"))
        .json(&serde_json::json!({
            "provider": "mock", "account": "a1", "enabled": false
        }))
        .send()
        .await
        .unwrap();
    assert_ne!(unguarded.status(), 200);
}

#[test]
fn sanitised_provider_errors_are_one_short_line() {
    let messy = "line one\n\tline two   with spaces";
    let clean = super::console::sanitised(messy);
    assert!(!clean.contains('\n'));
    assert!(!clean.contains('\t'));
    assert!(clean.contains("line one line two"));
    let long = "x".repeat(600);
    assert_eq!(super::console::sanitised(&long).chars().count(), 300);
}

#[tokio::test]
async fn the_doctor_page_reports_every_check_in_both_languages() {
    let (addr, _fake, _path, dir) = routes_router().await;
    let english = reqwest::get(format!("{addr}/doctor?lang=en"))
        .await
        .unwrap();
    assert_eq!(english.status(), 200);
    let english = english.text().await.unwrap();
    assert!(english.contains("Doctor"));
    assert!(english.contains("providers"));
    assert!(english.contains("adapter = none"));
    assert!(english.contains("FAIL"));
    assert!(english.contains("data-probe-all"));
    assert!(
        !english.contains("sk-mock-a1") && !english.contains("sk-alt-a1"),
        "a doctor page must never carry a credential value"
    );

    let chinese = reqwest::get(format!("{addr}/doctor?lang=zh"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(chinese.contains("诊断"));
    assert!(chinese.contains("失败"));
    assert!(chinese.contains("adapter = none"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn probing_reaches_the_provider_through_a_mock_upstream() {
    // A provider that answers the xKiro usage endpoint.
    let app = axum::Router::new().route(
        "/v1/usage",
        axum::routing::get(|| async {
            axum::Json(serde_json::json!({
                "plan": "pro",
                "windows": [{
                    "kind": "short",
                    "window_sec": 18_000,
                    "spent_usd": "0.5",
                    "cap_usd": "2.0",
                    "remaining_usd": "1.5",
                    "resets_in_sec": 3_600
                }],
                "free_tokens": { "used_today": 10, "limit_per_day": 100, "remaining": 90 },
                "wallet": { "balance_usd": "5.0", "held_usd": "0.0" }
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "lgw-probe-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    let db = dir.join("gateway.db");
    let toml = format!(
        r#"
[server]
listen = "127.0.0.1:0"
db_path = "{}"

[providers.xkiro]
adapter = "xkiro"
base_url = "http://{upstream}"

[providers.xkiro.accounts.a1]
key = "sk-probe-test"

[routes]
"z-ai/glm-5.3-flash" = "xkiro"
"#,
        db.display()
    );
    std::fs::write(&path, &toml).unwrap();
    let config = Config::from_toml_str(&toml).expect("config");
    let gateway = crate::router::Gateway::new(config).await.expect("gateway");
    let backend: Arc<super::GatewayBackend> = Arc::new(super::GatewayBackend::new(gateway.clone()));
    let addr = spawn_console_router_with(backend, path.to_str().unwrap()).await;

    let response = reqwest::Client::new()
        .post(format!("{addr}/console/api/accounts/probe"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let accounts = gateway.snapshot();
    assert_eq!(accounts.len(), 1);
    let account = &accounts[0];
    assert_eq!(account.state, crate::domain::AccountState::Active);
    assert!(
        account
            .gates
            .iter()
            .any(|gate| gate.kind == crate::domain::GateKind::FiveHour),
        "the mock upstream's window reached the board: {:?}",
        account.gates
    );
    let _ = gateway.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_keys_page_lists_sources_and_manages_the_store() {
    let config = Config::from_toml_str(APPLY_TOML).expect("config");
    let fake = fake_backend_with_config(config);
    let addr = spawn_console_router(fake.clone()).await;

    let page = reqwest::get(format!("{addr}/keys?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("inline-literal"));
    assert!(page.contains("data-key-add"));
    assert!(
        !page.contains("sk-apply-a1"),
        "a value never reaches the page"
    );

    let secret = "sk-console-very-secret-123";
    let added = reqwest::Client::new()
        .post(format!("{addr}/console/api/keys/add"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({
            "provider": "mock", "account": "a1", "key": secret
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(added.status(), 200);
    assert!(!added.text().await.unwrap().contains(secret));
    let stored = fake.credentials.get("mock", "a1").unwrap().expect("stored");
    assert_eq!(stored.expose(), secret);

    let page = reqwest::get(format!("{addr}/keys?lang=en"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!page.contains(secret), "the value is write-only");
    assert!(page.contains("mock"));

    let tested = reqwest::Client::new()
        .post(format!("{addr}/console/api/keys/test?lang=en"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({ "provider": "mock", "account": "a1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(tested.status(), 200);
    let body: serde_json::Value = tested.json().await.unwrap();
    let detail = body["detail"].as_str().expect("detail");
    assert!(detail.contains("ok"), "got {detail}");
    assert!(!detail.contains(secret));

    let removed = reqwest::Client::new()
        .post(format!("{addr}/console/api/keys/remove"))
        .header("x-lgw-console", "1")
        .json(&serde_json::json!({ "provider": "mock", "account": "a1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), 200);
    assert!(fake.credentials.get("mock", "a1").unwrap().is_none());

    let chinese = reqwest::get(format!("{addr}/keys?lang=zh"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(chinese.contains("密钥"));
    assert!(chinese.contains("来源"));
}

#[tokio::test]
async fn key_mutations_require_the_console_guards() {
    let config = Config::from_toml_str(APPLY_TOML).expect("config");
    let fake = fake_backend_with_config(config);
    let addr = spawn_console_router(fake.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("{addr}/console/api/keys/add"))
        .json(&serde_json::json!({
            "provider": "mock", "account": "a1", "key": "sk-unguarded"
        }))
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), 200);
    assert!(fake.credentials.get("mock", "a1").unwrap().is_none());
}
