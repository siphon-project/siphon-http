//! HTTP load harness for a siphon-http server.
//!
//! Two modes:
//!
//!   * `drive`  — flood HTTP requests at a target and report throughput +
//!                request→response latency percentiles. Point it at a real
//!                `siphon --features http` running `harness/bench_echo.py`, or
//!                at this harness's own `serve` mock.
//!   * `serve`  — a minimal mock HTTP server (200 OK for any request). Lets you
//!                smoke-test the driver — and CI — without standing up siphon.
//!
//! Self-test (no siphon):
//!   http-load serve --port 8080 &
//!   http-load drive --port 8080 --count 50000 --concurrency 64

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::any;
use axum::Router;
use clap::{Parser, Subcommand};
use tokio::sync::{Mutex, Semaphore};

#[derive(Parser)]
#[command(name = "http-load", about = "HTTP load driver + mock server for siphon-http")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Flood HTTP requests at a target and report throughput + latency.
    Drive(DriveArgs),
    /// Run a mock HTTP server that 200s every request (for self-test / CI).
    Serve(ServeArgs),
}

#[derive(Parser)]
struct DriveArgs {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8080)]
    port: u16,
    #[arg(long, default_value = "/")]
    path: String,
    /// Total requests to send.
    #[arg(long, default_value_t = 50_000)]
    count: usize,
    /// Max in-flight requests.
    #[arg(long, default_value_t = 64)]
    concurrency: usize,
}

#[derive(Parser)]
struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8080)]
    port: u16,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    match Cli::parse().cmd {
        Cmd::Serve(a) => serve(a).await,
        Cmd::Drive(a) => drive(a).await,
    }
}

// ── Mock server ───────────────────────────────────────────────────────────

async fn serve(a: ServeArgs) {
    let app = Router::new().fallback(any(|| async { "ok" }));
    let addr = format!("{}:{}", a.host, a.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("bind mock server");
    println!("mock HTTP server listening on {addr} (Ctrl-C to stop)");
    axum::serve(listener, app).await.unwrap();
}

// ── Load driver ─────────────────────────────────────────────────────────────

async fn drive(a: DriveArgs) {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(a.concurrency)
        .build()
        .expect("build client");
    let url = format!("http://{}:{}{}", a.host, a.port, a.path);

    // Warm up (excluded from the measurement window).
    for _ in 0..a.concurrency.min(a.count) {
        let _ = client.get(&url).send().await;
    }

    println!(
        "driving {} at concurrency {} ({} requests)",
        url, a.concurrency, a.count
    );

    let sem = Arc::new(Semaphore::new(a.concurrency));
    let latencies = Arc::new(Mutex::new(Vec::<u64>::with_capacity(a.count)));
    let errors = Arc::new(AtomicU64::new(0));

    let started = Instant::now();
    let mut handles = Vec::with_capacity(a.count);
    for _ in 0..a.count {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let url = url.clone();
        let lat = latencies.clone();
        let errs = errors.clone();
        handles.push(tokio::spawn(async move {
            let t0 = Instant::now();
            match client.get(&url).send().await {
                Ok(r) if r.status().is_success() => {
                    let _ = r.bytes().await;
                    lat.lock().await.push(t0.elapsed().as_micros() as u64);
                }
                _ => {
                    errs.fetch_add(1, Ordering::Relaxed);
                }
            }
            drop(permit);
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    let elapsed = started.elapsed();

    report(&latencies.lock().await, errors.load(Ordering::Relaxed), elapsed);
}

fn report(latencies: &[u64], errors: u64, elapsed: Duration) {
    let ok = latencies.len() as u64;
    let total = ok + errors;
    let secs = elapsed.as_secs_f64().max(1e-9);
    let rps = ok as f64 / secs;

    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();
    let pct = |p: f64| -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    };

    println!("\n── results ──────────────────────────────");
    println!("  requests  : {total}  ok {ok}  errors {errors}");
    println!("  elapsed   : {secs:.3}s");
    println!("  throughput: {rps:.0} req/s");
    if !sorted.is_empty() {
        println!(
            "  latency   : p50 {:.2}ms  p90 {:.2}ms  p99 {:.2}ms  p999 {:.2}ms  max {:.2}ms",
            pct(50.0) as f64 / 1000.0,
            pct(90.0) as f64 / 1000.0,
            pct(99.0) as f64 / 1000.0,
            pct(99.9) as f64 / 1000.0,
            (*sorted.last().unwrap()) as f64 / 1000.0,
        );
    }
    if errors > 0 {
        std::process::exit(1);
    }
}
