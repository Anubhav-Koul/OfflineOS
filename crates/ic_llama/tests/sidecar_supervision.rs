//! Supervision behavior, driven against a real child process.
//!
//! `testsupport/fake_llama_server.rs` reproduces the behaviors that matter:
//! binding the port before the model is loaded, answering `503` while loading,
//! crashing on startup, crashing after having been healthy, and never becoming
//! healthy at all. Everything here is about what the supervisor does in
//! response.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ic_llama::wiring::{LLM_BASE_URL, LLM_MODEL, LlmEnv};
use ic_llama::{Error, ModelId, Sidecar, SidecarConfig, SidecarState, SpawnHook};
use tokio::sync::watch;

/// The fake server, built by cargo alongside this test.
fn fake_server() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ic_llama_fake_server"))
}

/// A config with test-scale timings: the defaults are tuned for a real model
/// load, which is minutes.
fn config(mode: &str) -> SidecarConfig {
    let mut config = SidecarConfig::new(
        fake_server(),
        PathBuf::from("test-model.gguf"),
        ModelId::new("test-model").expect("valid id"),
    )
    .expect("a free port");
    config.startup_timeout = Duration::from_secs(10);
    config.initial_backoff = Duration::from_millis(50);
    config.max_backoff = Duration::from_millis(200);
    config.max_crashes = 2;
    config.env = vec![("FAKE_LLAMA_MODE".into(), mode.into())];
    config
}

fn set_env(config: &mut SidecarConfig, name: &str, value: impl ToString) {
    config.env.push((name.into(), value.to_string()));
}

/// Block until `predicate` holds, or fail the test.
async fn wait_for(
    states: &mut watch::Receiver<SidecarState>,
    within: Duration,
    predicate: impl Fn(&SidecarState) -> bool,
) -> SidecarState {
    let deadline = Instant::now() + within;
    loop {
        let current = states.borrow_and_update().clone();
        if predicate(&current) {
            return current;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "timed out waiting for a state change; stuck at {current:?}"
        );
        if tokio::time::timeout(remaining, states.changed())
            .await
            .is_err()
        {
            panic!("timed out waiting for a state change; stuck at {current:?}");
        }
    }
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
            "port {port} is still held; the child outlived its supervisor"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_healthy_server_becomes_ready_and_answers_on_its_base_url() {
    let config = config("ready");
    let port = config.port;

    let sidecar = Sidecar::start(config).await.expect("server should start");

    assert_eq!(sidecar.state(), SidecarState::Ready);
    assert_eq!(sidecar.base_url(), format!("http://127.0.0.1:{port}/v1"));

    let response = reqwest::get(sidecar.health_url())
        .await
        .expect("health request");
    assert!(response.status().is_success());
}

#[tokio::test]
async fn a_slow_load_is_waited_out_rather_than_treated_as_a_failure() {
    let mut config = config("ready");
    // The server binds immediately and answers 503 for 700ms. A supervisor that
    // read 503 as a failure would restart it into a loop it never escapes.
    set_env(&mut config, "FAKE_LLAMA_LOAD_MS", 700);

    let started = Instant::now();
    let sidecar = Sidecar::start(config).await.expect("server should start");

    assert_eq!(sidecar.state(), SidecarState::Ready);
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "returned ready before the model finished loading"
    );
}

#[tokio::test]
async fn a_server_that_crashes_once_is_restarted_and_comes_up() {
    let temp = tempfile::tempdir().expect("tempdir");
    let counter = temp.path().join("attempts");

    let mut config = config("flaky");
    set_env(&mut config, "FAKE_LLAMA_CRASH_TIMES", 1);
    set_env(&mut config, "FAKE_LLAMA_STATE_FILE", counter.display());

    let sidecar = Sidecar::start(config)
        .await
        .expect("should survive one crash");

    assert_eq!(sidecar.state(), SidecarState::Ready);
    // Crashed once, restarted, came up on the second spawn.
    assert_eq!(
        std::fs::read_to_string(&counter).expect("counter"),
        "2",
        "expected exactly one restart"
    );
    assert!(
        sidecar.output_tail().contains("fatal error on attempt 1"),
        "the failed attempt's output should be kept for diagnostics:\n{}",
        sidecar.output_tail()
    );
}

#[tokio::test]
async fn the_spawn_hook_runs_on_every_spawn_including_restarts() {
    // The desktop widget uses this hook to enlist each child in its process
    // job. A restart produces a *new* child that must be enlisted too, so the
    // hook has to fire on the restart, not just the first spawn.
    let temp = tempfile::tempdir().expect("tempdir");
    let counter = temp.path().join("attempts");

    let mut config = config("flaky");
    set_env(&mut config, "FAKE_LLAMA_CRASH_TIMES", 1);
    set_env(&mut config, "FAKE_LLAMA_STATE_FILE", counter.display());

    let spawns = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&spawns);
    config.on_spawn = Some(SpawnHook::new(move |_child| {
        seen.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));

    let sidecar = Sidecar::start(config).await.expect("survives one crash");
    assert_eq!(sidecar.state(), SidecarState::Ready);

    // Crashed once then came up: two spawns, so two hook calls.
    assert_eq!(spawns.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_spawn_hook_error_fails_the_attempt_and_kills_the_child() {
    // A child that cannot be contained (job assignment failed) must not be left
    // running. A hook that always errors turns every spawn into a failed
    // attempt, so the supervisor exhausts its retries and declares the model
    // suspect — the same terminal path as a child that will not start.
    let mut config = config("ready");
    config.max_crashes = 2;
    let port = config.port;
    config.on_spawn = Some(SpawnHook::new(|_child| {
        Err(std::io::Error::other("job assignment refused"))
    }));

    let error = Sidecar::start(config)
        .await
        .expect_err("an uncontainable child must not report ready");
    assert!(
        matches!(error, Error::ModelSuspect { .. }),
        "expected the retries to be exhausted, got {error:?}"
    );

    // The child the hook rejected was killed, not leaked: its port is free.
    wait_for_port_release(port, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn a_server_that_always_crashes_marks_the_model_suspect() {
    let config = config("crash");
    let model = config.model_id.clone();

    let error = Sidecar::start(config)
        .await
        .expect_err("a model that always crashes must not report ready");

    let Error::ModelSuspect {
        model: reported,
        crashes,
        last_output,
    } = error
    else {
        panic!("expected the model to be declared suspect");
    };
    assert_eq!(reported, model.to_string());
    assert_eq!(crashes, 2);
    // The user needs the server's own words, not just "it failed".
    let output = last_output.expect("the server's output");
    assert!(output.contains("fatal error"), "{output}");
    assert!(output.contains("test-model.gguf"), "{output}");
}

#[tokio::test]
async fn a_server_that_never_finishes_loading_is_killed_and_declared_suspect() {
    let mut config = config("loading");
    // Bound but wedged: it holds the port and answers 503 forever.
    config.startup_timeout = Duration::from_millis(600);
    let port = config.port;

    let started = Instant::now();
    let error = Sidecar::start(config)
        .await
        .expect_err("a wedged server must not hang the launch");

    let Error::ModelSuspect { last_output, .. } = error else {
        panic!("expected the model to be declared suspect");
    };
    let output = last_output.expect("a reason");
    assert!(output.contains("did not become healthy"), "{output}");

    // Two 600ms attempts plus a 50ms backoff; anything near the caller's full
    // budget would mean the per-attempt kill never fired.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    wait_for_port_release(port, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn a_server_that_dies_after_being_ready_is_restarted_then_given_up_on() {
    let mut config = config("die_after_ready");
    // Comfortably longer than the supervisor's health-poll interval, so a poll
    // is guaranteed to observe the server healthy before it dies. A shorter life
    // would test something else entirely: a server that never gets seen ready.
    set_env(&mut config, "FAKE_LLAMA_ALIVE_MS", 1200);
    let port = config.port;

    let sidecar = Sidecar::start(config).await.expect("it comes up first");
    assert_eq!(sidecar.state(), SidecarState::Ready);
    let mut states = sidecar.subscribe();

    // It dies, and the supervisor brings it back rather than giving up on the
    // first failure.
    let restarting = wait_for(&mut states, Duration::from_secs(10), |state| {
        matches!(state, SidecarState::Restarting { .. })
    })
    .await;
    let SidecarState::Restarting { attempt, .. } = restarting else {
        unreachable!("matched above");
    };
    assert_eq!(attempt, 1);

    // It dies a second time inside the stability window, so the model is now
    // suspect and the loop stops.
    let suspect = wait_for(&mut states, Duration::from_secs(10), |state| {
        matches!(state, SidecarState::Suspect { .. })
    })
    .await;
    let SidecarState::Suspect { reason } = suspect else {
        unreachable!("matched above");
    };
    assert!(reason.contains("2 times in a row"), "{reason}");

    wait_for_port_release(port, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn stopping_the_sidecar_kills_the_child() {
    let config = config("ready");
    let port = config.port;

    let mut sidecar = Sidecar::start(config).await.expect("server should start");
    sidecar.stop().await;

    assert_eq!(sidecar.state(), SidecarState::Stopped);
    wait_for_port_release(port, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn dropping_the_sidecar_kills_the_child() {
    let config = config("ready");
    let port = config.port;

    {
        let sidecar = Sidecar::start(config).await.expect("server should start");
        assert!(sidecar.state().is_ready());
    }

    // Nothing awaited the shutdown, so this relies on `kill_on_drop` reaching the
    // child when the aborted supervisor's stack unwinds. An orphaned
    // `llama-server` would hold a GPU allocation for the rest of the session.
    wait_for_port_release(port, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn the_environment_points_ironclaw_at_this_sidecar() {
    let config = config("ready");
    let port = config.port;
    let api_key = config.api_key.clone();

    let sidecar = Sidecar::start(config).await.expect("server should start");
    let env = LlmEnv::for_sidecar(&sidecar);

    assert_eq!(
        env.get(LLM_BASE_URL),
        Some(format!("http://127.0.0.1:{port}/v1").as_str())
    );
    assert_eq!(env.get(LLM_MODEL), Some("test-model"));
    assert_eq!(env.get("LLM_API_KEY"), Some(api_key.as_str()));
}

#[tokio::test]
async fn a_missing_server_binary_is_reported_rather_than_retried_forever() {
    let mut config = config("ready");
    config.server_bin = PathBuf::from("definitely-not-a-real-binary-9f3c");

    let error = Sidecar::start(config)
        .await
        .expect_err("a missing binary cannot start");
    assert!(matches!(error, Error::ModelSuspect { .. }), "{error:?}");
}
