//! Throughput benchmark: N connections run a workload at full speed for a fixed time.
//!
//! ```sh
//! make bench                                   # dedicated test TiDB, 10 connections, 10 s
//! cargo run --release -p tisqlx-core --example bench -- \
//!     --url tidb://root@127.0.0.1:4100 --connections 10 --seconds 10 --workload select
//! ```
//!
//! Workloads (`--workload`, default `all`):
//! - `select`: `SELECT v, n FROM bench WHERE id = ?`, a random row of 10 000;
//! - `insert`: `INSERT INTO bench_w (v, n) VALUES (?, ?)`;
//! - `txn`: `BEGIN; SELECT ... FOR UPDATE; UPDATE ...; COMMIT` on a random row (4 statements).
//!
//! It uses the database `tisqlx_bench` (recreated). Never point it at a shared server.

// A benchmark reports on stdout.
#![allow(clippy::print_stdout)]

use std::time::{Duration, Instant};

use tisqlx_core::{ConnectOptions, Pool, PoolOptions, TiConnection, query, query_as};

const ROWS: i64 = 10_000;

struct Args {
    url: String,
    connections: u32,
    seconds: u64,
    workload: String,
}

fn args() -> Args {
    let mut a = Args { url: "tidb://root@127.0.0.1:4100".into(), connections: 10, seconds: 10, workload: "all".into() };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--url" => a.url = value,
            "--connections" => a.connections = value.parse().expect("--connections"),
            "--seconds" => a.seconds = value.parse().expect("--seconds"),
            "--workload" => a.workload = value,
            _ => panic!("unknown flag {flag}; see the doc comment of examples/bench.rs"),
        }
    }
    a
}

/// xorshift64: a cheap per-worker random row id.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[derive(Clone, Copy)]
enum Workload {
    Select,
    Insert,
    Txn,
}

async fn one(pool: &Pool, w: Workload, rng: &mut u64) -> tisqlx_core::Result<()> {
    let id = (next(rng) % ROWS as u64) as i64 + 1;
    match w {
        Workload::Select => {
            let (_v, _n): (String, i64) =
                query_as("SELECT v, n FROM bench WHERE id = ?").bind(id).fetch_one(pool).await?;
        }
        Workload::Insert => {
            query("INSERT INTO bench_w (v, n) VALUES (?, ?)").bind("payload-0123456789").bind(id).execute(pool).await?;
        }
        Workload::Txn => {
            let mut tx = pool.begin().await?;
            let (n,): (i64,) =
                query_as("SELECT n FROM bench WHERE id = ? FOR UPDATE").bind(id).fetch_one(&mut *tx).await?;
            query("UPDATE bench SET n = ? WHERE id = ?").bind(n + 1).bind(id).execute(&mut *tx).await?;
            tx.commit().await?;
        }
    }
    Ok(())
}

struct Outcome {
    done: u64,
    errors: u64,
    latencies: Vec<Duration>,
}

async fn run(pool: &Pool, w: Workload, connections: u32, seconds: u64) -> (Outcome, Duration) {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(seconds);
    let workers: Vec<_> = (0..connections)
        .map(|i| {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ (u64::from(i) + 1);
                let mut out = Outcome { done: 0, errors: 0, latencies: Vec::with_capacity(1 << 16) };
                while Instant::now() < deadline {
                    let t = Instant::now();
                    match one(&pool, w, &mut rng).await {
                        Ok(()) => {
                            out.done += 1;
                            out.latencies.push(t.elapsed());
                        }
                        Err(e) => {
                            if out.errors == 0 {
                                eprintln!("worker {i}: {e}");
                            }
                            out.errors += 1;
                        }
                    }
                }
                out
            })
        })
        .collect();
    let mut total = Outcome { done: 0, errors: 0, latencies: Vec::new() };
    for w in workers {
        let o = w.await.expect("worker panicked");
        total.done += o.done;
        total.errors += o.errors;
        total.latencies.extend(o.latencies);
    }
    (total, start.elapsed())
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

async fn setup(url: &str) -> ConnectOptions {
    let mut admin = TiConnection::connect(url).await.expect("connect");
    admin.execute_raw("DROP DATABASE IF EXISTS tisqlx_bench; CREATE DATABASE tisqlx_bench").await.unwrap();
    let opts = url.parse::<ConnectOptions>().unwrap().database("tisqlx_bench");
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw(
        "CREATE TABLE bench (id BIGINT PRIMARY KEY, v VARCHAR(64) NOT NULL, n BIGINT NOT NULL);
         CREATE TABLE bench_w (id BIGINT PRIMARY KEY AUTO_INCREMENT, v VARCHAR(64) NOT NULL, n BIGINT NOT NULL)",
    )
    .await
    .unwrap();
    for chunk in (1..=ROWS).collect::<Vec<_>>().chunks(1000) {
        let values: Vec<String> = chunk.iter().map(|id| format!("({id}, 'row-{id}', 0)")).collect();
        conn.execute_raw(&format!("INSERT INTO bench VALUES {}", values.join(","))).await.unwrap();
    }
    conn.execute_raw("ANALYZE TABLE bench").await.unwrap();
    opts
}

#[tokio::main]
async fn main() {
    let a = args();
    let opts = setup(&a.url).await;
    let pool = PoolOptions::new()
        .max_connections(a.connections)
        .min_connections(a.connections)
        .connect_with(opts)
        .await
        .unwrap();
    let server = pool.acquire().await.unwrap().server_version().to_owned();
    println!("server {server}, {} connections, {} s per workload, {} rows", a.connections, a.seconds, ROWS);
    println!(
        "{:<8} {:>10} {:>10} {:>8} {:>9} {:>9} {:>9}",
        "workload", "requests", "req/s", "errors", "p50", "p99", "max"
    );
    let all = [("select", Workload::Select), ("insert", Workload::Insert), ("txn", Workload::Txn)];
    for (name, w) in all.into_iter().filter(|(n, _)| a.workload == "all" || a.workload == *n) {
        // Warm up: prepared statements cached on every connection.
        run(&pool, w, a.connections, 1).await;
        let (mut o, elapsed) = run(&pool, w, a.connections, a.seconds).await;
        o.latencies.sort_unstable();
        let rate = o.done as f64 / elapsed.as_secs_f64();
        let ms = |d: Duration| format!("{:.2}ms", d.as_secs_f64() * 1000.0);
        println!(
            "{name:<8} {:>10} {:>10.0} {:>8} {:>9} {:>9} {:>9}",
            o.done,
            rate,
            o.errors,
            ms(percentile(&o.latencies, 0.50)),
            ms(percentile(&o.latencies, 0.99)),
            ms(o.latencies.last().copied().unwrap_or_default()),
        );
    }
    pool.close().await;
}
