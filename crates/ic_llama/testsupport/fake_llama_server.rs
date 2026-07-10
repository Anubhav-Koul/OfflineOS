//! A stand-in for `llama-server`, used by `tests/sidecar_supervision.rs`.
//!
//! The supervisor's whole job is reacting to how a real child process behaves —
//! binding late, answering `503` while loading, dying mid-generation. None of
//! that can be exercised against an in-process fake, so the tests drive a real
//! subprocess that reproduces each behavior on demand.
//!
//! It accepts (and ignores) every flag `llama-server` takes except `--port`,
//! and is steered by environment variables:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `FAKE_LLAMA_MODE` | `ready` (default), `loading`, `crash`, `flaky`, `die_after_ready` |
//! | `FAKE_LLAMA_LOAD_MS` | answer `503` for this long before going healthy |
//! | `FAKE_LLAMA_ALIVE_MS` | `die_after_ready`: stay healthy this long, then exit |
//! | `FAKE_LLAMA_CRASH_TIMES` | `flaky`: crash this many times, then come up healthy |
//! | `FAKE_LLAMA_STATE_FILE` | `flaky`: where the attempt counter lives |
//!
//! This is a test fixture. It uses `unwrap` freely; a panic here is a broken
//! test, which is exactly what should happen.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = flag(&args, "--port")
        .expect("the supervisor always passes --port")
        .parse()
        .expect("--port must be a number");

    // The supervisor drains both pipes; emitting on each proves it.
    println!("fake llama-server starting on port {port}");
    eprintln!(
        "fake llama-server: loading model {}",
        flag(&args, "--model").unwrap_or_default()
    );

    let mode = env("FAKE_LLAMA_MODE").unwrap_or_else(|| "ready".to_string());
    let load_for = Duration::from_millis(env_ms("FAKE_LLAMA_LOAD_MS"));

    match mode.as_str() {
        "crash" => {
            eprintln!("fake llama-server: fatal error, exiting");
            std::process::exit(1);
        }
        "flaky" => {
            let attempt = bump_attempt_counter();
            let crash_times = env_ms("FAKE_LLAMA_CRASH_TIMES");
            if attempt <= crash_times {
                eprintln!("fake llama-server: fatal error on attempt {attempt}");
                std::process::exit(1);
            }
            serve(port, Some(Instant::now() + load_for), None);
        }
        // Bind the port but never finish "loading", the way a server wedged on a
        // model it cannot allocate behaves.
        "loading" => serve(port, None, None),
        "die_after_ready" => {
            let alive_for = Duration::from_millis(env_ms("FAKE_LLAMA_ALIVE_MS"));
            serve(
                port,
                Some(Instant::now() + load_for),
                Some(Instant::now() + load_for + alive_for),
            );
        }
        _ => serve(port, Some(Instant::now() + load_for), None),
    }
}

/// Serve `/health` until `die_at`.
///
/// `ready_at` of `None` means never healthy. Accepts one connection at a time,
/// which is all the supervisor's health poller needs.
fn serve(port: u16, ready_at: Option<Instant>, die_at: Option<Instant>) {
    let listener =
        TcpListener::bind(("127.0.0.1", port)).expect("bind the port the supervisor reserved");
    // Poll `die_at` between connections rather than blocking forever on accept.
    listener
        .set_nonblocking(true)
        .expect("non-blocking listener");

    loop {
        if let Some(die_at) = die_at
            && Instant::now() >= die_at
        {
            eprintln!("fake llama-server: dying after having been ready");
            std::process::exit(1);
        }

        match listener.accept() {
            Ok((stream, _)) => handle(stream, ready_at),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    }
}

fn handle(mut stream: std::net::TcpStream, ready_at: Option<Instant>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    // Drain headers so the client's write completes.
    for line in reader.lines() {
        match line {
            Ok(line) if line.is_empty() => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }

    let ready = ready_at.is_some_and(|at| Instant::now() >= at);
    let response = if ready {
        let body = r#"{"status":"ok"}"#;
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    } else {
        // Exactly what `llama-server` says while the weights load.
        let body = r#"{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}"#;
        format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// `--flag value` → `Some(value)`.
fn flag(args: &[String], name: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == name)?;
    args.get(index + 1).cloned()
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn env_ms(name: &str) -> u64 {
    env(name)
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

/// Count spawns across restarts so `flaky` can fail a fixed number of times.
fn bump_attempt_counter() -> u64 {
    let path = env("FAKE_LLAMA_STATE_FILE").expect("flaky mode needs FAKE_LLAMA_STATE_FILE");
    let previous: u64 = std::fs::read_to_string(&path)
        .ok()
        .and_then(|contents| contents.trim().parse().ok())
        .unwrap_or(0);
    let attempt = previous + 1;
    std::fs::write(&path, attempt.to_string()).expect("write the attempt counter");
    attempt
}
