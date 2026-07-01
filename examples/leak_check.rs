//! Counting-allocator leak check for siphon-http's per-request Rust hot paths.
//!
//! A global allocator tallies bytes allocated vs freed. We warm up, snapshot
//! live bytes, hammer the parse + config paths for many cycles, and assert the
//! live-byte delta is flat (everything each cycle allocates is dropped). Prints
//! `PASS` / `FAIL` and exits non-zero on FAIL, so CI can gate on it.
//!
//! Run: `cargo run --release --example leak_check`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use siphon_http::parse::{extract_path_params, parse_query, urldecode};
use siphon_http::HttpConfig;

struct Counting;
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        FREED.fetch_add(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const CONFIG: &str = r#"
servers:
  - listen: "0.0.0.0:8443"
    tls:
      cert_path: "/tls/server.crt"
      key_path: "/tls/server.key"
    max_body_bytes: 65536
clients:
  api:
    base_url: "https://api.example.com"
    timeout_ms: 5000
    pool_size: 16
"#;

fn exercise() {
    let params = extract_path_params("/users/{id}/orders/{order}", "/users/42/orders/9001");
    let query = parse_query("page=2&limit=50&q=hello%20world&sort=-created");
    let decoded = urldecode("hello%20world%21+%28ok%29");
    let cfg = HttpConfig::from_yaml(CONFIG).unwrap();
    std::hint::black_box((params, query, decoded, cfg));
}

fn live_bytes() -> i64 {
    ALLOCATED.load(Ordering::Relaxed) as i64 - FREED.load(Ordering::Relaxed) as i64
}

fn main() {
    const CYCLES: usize = 200_000;
    // Slack for lazily-initialised statics / allocator bookkeeping. The steady
    // state should be exactly flat; a few KB of tolerance keeps CI non-flaky.
    const TOLERANCE: i64 = 8 * 1024;

    for _ in 0..1_000 {
        exercise();
    }

    let before = live_bytes();
    for _ in 0..CYCLES {
        exercise();
    }
    let after = live_bytes();
    let delta = after - before;

    println!("cycles      : {CYCLES}");
    println!("live before : {before} bytes");
    println!("live after  : {after} bytes");
    println!("delta       : {delta} bytes (tolerance ±{TOLERANCE})");

    if delta.abs() <= TOLERANCE {
        println!("PASS");
    } else {
        println!("FAIL — live bytes grew across cycles");
        std::process::exit(1);
    }
}
