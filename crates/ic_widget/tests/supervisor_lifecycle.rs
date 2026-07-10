//! The supervisor against a real `ironclaw-reborn serve`.
//!
//! Unit tests pin the command line and the backoff arithmetic. Only this test
//! proves the environment is right, that readiness is detected, that a gateway
//! which rejects our token is not restarted forever, and that a child cannot
//! outlive the widget.
//!
//! ```bash
//! cargo build -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta
//! cargo test  -p ic_widget --features webui-v2-beta
//! ```
#![cfg(feature = "webui-v2-beta")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ic_integration_tests::{MockLlm, reborn_bin};
use ic_widget::ProcessJob;
use ic_widget::supervisor::{GatewayConfig, GatewayState, GatewaySupervisor};

/// A hermetic gateway config: isolated home, mock LLM, test-scale timings.
async fn config(home: &tempfile::TempDir) -> (GatewayConfig, MockLlm) {
    let bin = reborn_bin();
    assert!(
        bin.exists(),
        "build the binary first:\n  cargo build -p ironclaw_reborn_cli \
         --bin ironclaw-reborn --features webui-v2-beta"
    );

    let mock = MockLlm::start("supervised-ok".to_string()).await;
    let mut config = GatewayConfig::new(
        bin,
        home.path().join("reborn-home"),
        format!("ic-widget-test-{}", uuid_like()),
    )
    .expect("a free port");

    config.llm_env = vec![
        ("LLM_BACKEND".into(), "openai_compatible".into()),
        ("LLM_BASE_URL".into(), mock.base_url()),
        ("LLM_API_KEY".into(), "test-key".into()),
        ("LLM_MODEL".into(), "mock-model".into()),
        ("LLM_MAX_RETRIES".into(), "0".into()),
        // Keep all on-disk state inside the tempdir.
        (
            "HOME".into(),
            home.path().join("home").display().to_string(),
        ),
        (
            "USERPROFILE".into(),
            home.path().join("home").display().to_string(),
        ),
    ];
    config.startup_timeout = Duration::from_secs(90);
    config.initial_backoff = Duration::from_millis(100);
    (config, mock)
}

fn uuid_like() -> String {
    // The crate does not depend on `uuid` in dev; a nanosecond stamp is unique
    // enough to keep two concurrent tests from sharing a token.
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    )
}

/// Poll until the port can be bound again, proving the child is gone.
async fn wait_for_port_release(port: u16, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "port {port} is still held; the gateway outlived its supervisor"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gateway_starts_serves_requests_and_stops_cleanly() {
    let home = tempfile::tempdir().expect("tempdir");
    let (config, _mock) = config(&home).await;
    let port = config.port;
    let job = Arc::new(ProcessJob::new().expect("job object"));

    let mut gateway = GatewaySupervisor::start(config, job).await.expect("start");

    assert_eq!(gateway.state(), GatewayState::Ready);
    // Readiness is an authenticated read, so being ready already proves the
    // token round-tripped through the child's environment.
    gateway.recheck().await.expect("health");

    let thread_id = gateway
        .client()
        .create_thread()
        .await
        .expect("the gateway serves requests");
    assert!(!thread_id.as_str().is_empty());

    gateway.stop().await;
    assert_eq!(gateway.state(), GatewayState::Stopped);
    wait_for_port_release(port, Duration::from_secs(15)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_that_rejects_our_token_is_terminal_rather_than_restarted() {
    let home = tempfile::tempdir().expect("tempdir");
    let (mut config, _mock) = config(&home).await;

    // `env()` layers `llm_env` last, so this overrides the token the child is
    // told to accept while the supervisor's client keeps using `config.token`.
    // That is exactly the split-brain a stale credential-store entry produces.
    config.llm_env.push((
        "IRONCLAW_REBORN_WEBUI_TOKEN".into(),
        "a-different-token".into(),
    ));
    config.max_crashes = 3;
    config.initial_backoff = Duration::from_secs(5); // a restart would be obvious
    let port = config.port;
    let job = Arc::new(ProcessJob::new().expect("job object"));

    let started = Instant::now();
    let error = GatewaySupervisor::start(config, job)
        .await
        .expect_err("a gateway that rejects our token must not report ready");

    // The gateway is healthy; it simply does not believe us. Restarting it
    // cannot help, so the supervisor must stop immediately rather than burn
    // three attempts and two backoffs.
    let message = error.to_string();
    assert!(
        message.contains("rejected our bearer token"),
        "expected a token-rejection diagnosis, got: {message}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the supervisor retried a token rejection; it took {:?}",
        started.elapsed()
    );
    wait_for_port_release(port, Duration::from_secs(15)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_supervisor_kills_the_gateway() {
    let home = tempfile::tempdir().expect("tempdir");
    let (config, _mock) = config(&home).await;
    let port = config.port;
    let job = Arc::new(ProcessJob::new().expect("job object"));

    {
        let gateway = GatewaySupervisor::start(config, job).await.expect("start");
        assert!(gateway.state().is_ready());
    }

    // Nothing awaited the shutdown. An orphaned gateway would keep the libSQL
    // write lock and the port, and the next launch would fail.
    wait_for_port_release(port, Duration::from_secs(15)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_binary_fails_fast_with_a_useful_message() {
    let home = tempfile::tempdir().expect("tempdir");
    let (mut config, _mock) = config(&home).await;
    config.binary = "definitely-not-a-real-binary-4f2b".into();
    config.max_crashes = 2;
    config.initial_backoff = Duration::from_millis(50);
    let job = Arc::new(ProcessJob::new().expect("job object"));

    let started = Instant::now();
    let error = GatewaySupervisor::start(config, job)
        .await
        .expect_err("a missing binary cannot serve");
    assert!(error.to_string().contains("failed 2 times"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(20));
}
