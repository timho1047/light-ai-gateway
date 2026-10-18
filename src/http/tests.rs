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
}

impl FakeBackend {
    fn new(config: Config, pool: Pool) -> Self {
        FakeBackend {
            runtime: RwLock::new(runtime_for(&config)),
            pool,
            successes: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
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

impl ProxyBackend for FakeBackend {
    fn runtime(&self) -> Arc<Runtime> {
        self.runtime.read().unwrap().clone()
    }

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

    fn snapshot(&self) -> Vec<Account> {
        self.pool.snapshot()
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
fn priced_config(base_url: &str, input: f64, output: f64) -> Config {
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

[prices.mock."mock/model"]
input = {input}
output = {output}

[routes]
"mock/model" = "mock"
"#
    );
    Config::from_toml_str(&toml).unwrap()
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
async fn spawn_console_router(backend: Arc<dyn ProxyBackend>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let console = super::console::ConsoleContext::new(
        &addr.to_string(),
        addr.port(),
        "/tmp/lgw-test-config.toml",
    );
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
