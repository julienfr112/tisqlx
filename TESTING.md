# Testing tisqlx

## Tiers

| tier | needs | what it covers | run with |
|---|---|---|---|
| **Unit** | nothing | Pure logic, each module's `#[cfg(test)]`: the wire codec, URL options, auth scrambles, savepoint SQL, the migration planner (`validate`/`pending`/`to_revert`), the macro type mapping checked against the runtime `Type::compatible` (with chrono/`Decimal`, and again with only `time`/`BigDecimal`), the bind-site analyzer, hostile input for every decoder, and CLI version naming. | `make test-unit` |
| **Mock protocol** | nothing | `tisqlx-core/tests/mock/`: a scripted MySQL-protocol server sends exact packet sequences. It tests what a real server can't reproduce on demand: futures dropped mid-response, multiple result sets, malformed packets, the 8113 re-prepare, deferred `STMT_CLOSE`, stale prepared metadata, hostile handshakes (clear-text auth, unknown plugins, non-TiDB servers), failover between servers, and pool cleanup after a dropped mid-result stream. | `make test-unit` |
| **Derive compile tests** | nothing | `tisqlx/tests/derives.rs` checks that the derives generate impls for generic, borrowed and nested types; `tests/ui/*.rs` checks the compile errors of the derives and of `#[tisqlx::test]` (trybuild). | `make test-unit` |
| **Live** | dedicated TiDB | `tisqlx-core/tests/live/`: every feature against a real server, one throwaway database per test. Covers auth over TLS, the Unix socket and plain TCP, all types in both protocols, transactions and abandoned savepoints, pool accounting and lock release, write-conflict retry, stale reads, migrations, 17 MB packets and an exact 16 MB frame. | `make test-live` |
| **Macros (live)** | dedicated TiDB | `tisqlx/tests/macros.rs` compiles the `query!` family online and runs the queries. `tests/ui-query/*.rs` (run by `derives.rs`) checks the query macro compile errors: bind count, bind types, bad column names, bad SQL. | `make test-live` |
| **`#[tisqlx::test]`** | dedicated TiDB | `tisqlx/tests/test_attr.rs` uses the attribute itself: a database per test, migrations, fixtures, `Result` returns. | `make test-live` |
| **Cluster** | tiup playground | `tisqlx-core/tests/live/cluster.rs` needs a real cluster: TiFlash reads, DDL on one TiDB node while the other holds a cached statement, stale reads returning old data, TiKV lock-wait timeouts, and a pool spread over both nodes. | `make db-cluster test-cluster` |
| **CLI** | both tiers | `tisqlx-cli/tests/cli.rs` runs the real binary: `migrate add`, `build-script` and the `.env` lookup without a DB, and a full lifecycle against TiDB (setup, dry runs, revert, dirty migration, repair, a `reset` that can't read its migrations). | `make test-unit` / `make test-live` |
| **Offline data** | dedicated TiDB | `tisqlx prepare --check`: the committed `.tisqlx/` still matches the schema. | `make prepare-check` |

Every test that needs a database is `#[ignore]`d with the target that runs it, so a plain
`cargo test` works anywhere and reports them as *ignored*, never as passed.
- `make test-live` runs them with `--include-ignored` (skipping `cluster::`).
- `make test-cluster` runs the cluster tests. They read `TISQLX_TEST_CLUSTER` (the URLs of the
  two TiDB nodes), not `TISQLX_TEST_URL`.
- Those targets (and so CI) also set `TISQLX_TEST_REQUIRE=1` or `TISQLX_TEST_REQUIRE_CLUSTER=1`,
  so an unreachable database fails the run instead of skipping inside the tests.

## The test database

Tests never touch a shared server. `make db-up` starts a dedicated TiDB v8.5.8 with `scripts/tidb.sh`:

- It runs standalone (unistore, no PD/TiKV) on port **4100**, with its state in `target/tidb`.
  `TIDB_PORT=4200` on the `make` command line moves it (pass it to the test targets too).
- It has TLS (`auto-tls`) and a Unix socket at `target/tidb/tidb.sock`.
- It uses the tiup `tidb-server` binary (`tiup install tidb:v8.5.8`), or docker with
  `TIDB_RUNNER=docker`. The docker server has no Unix socket, so socket auth is not tested
  there; CI uses docker.

```sh
make db-up        # once
make test         # unit + live
make db-down
```

The **cluster tier** runs a real cluster with `tiup playground` v8.5.8: 2 TiDB nodes, PD,
TiKV and TiFlash.
- Every port is shifted by 10000 (TiDB on 14000/14001), so it can run beside a default
  playground. `scripts/tidb.sh` refuses to start if those ports are taken.
- It takes about 30 s to start once the binaries are cached, and uses a few GB of RAM.
- `make db-cluster-down` stops it and removes its data. CI runs this tier nightly.
- Its tests create the databases `tisqlx_c_*`.

The macro tests compile from the committed offline data (`tisqlx/.env` sets `TISQLX_OFFLINE=true`).
The live tier compiles them online instead (`DATABASE_URL` plus `TISQLX_OFFLINE=false`).
After changing a `query!` call or the test schema (`tisqlx/tests/migrations`), run `make prepare` and commit `.tisqlx/`.

## Where a new test goes

- **Logic that needs no I/O:** a unit test next to the code.
- **Connection behaviour that depends on packet timing or malformed input:** a mock test. Script the exact packets. The test must fail without the fix; check that by reverting the fix once.
- **Behaviour that depends on what TiDB actually does** (SQL semantics, error codes, metadata flags): a live test. Don't assume MySQL behaviour; tisqlx exists because TiDB differs.
- **Behaviour that needs PD, TiKV, TiFlash or a second TiDB node:** a cluster test (`require_cluster!()`). Check first whether the standalone server is enough: it often is.
- **A new macro diagnostic:** a `tests/ui*/` case; regenerate the snapshot with `TRYBUILD=overwrite`.

## Other checks

| | |
|---|---|
| `make lint` | `rustfmt --check`, and clippy on all targets and features with `-D warnings` (workspace lints in `Cargo.toml`) |
| `make features` | the facade builds with every supported feature set |
| `make msrv` | builds on Rust 1.88 |
| `make audit` | RustSec advisories (`cargo-audit`) |
| `make coverage` | `cargo llvm-cov` HTML report over the live tier |
| `make ci` | lint + features + test-live + prepare-check: the CI jobs `lint` and `live` |
| `make bench` | throughput against the test DB: point selects, inserts, transactions and bulk inserts, 10 connections at full speed for 10 s each (`tisqlx-core/examples/bench.rs`). `BENCH_ARGS="--connections 50 --workload select"` changes it. It measures the driver against the standalone `unistore` server, not a real TiKV cluster. |
| `make bench-cluster` | the same benchmark on a production-shaped tiup playground: 1 TiDB, 3 TiKV (every write replicated 3 times), PD, no TiFlash, on port offset 20000. It starts the cluster, runs the benchmark, and stops it. It takes about a minute and a few GB of RAM. |

## CI

`.github/workflows/ci.yml` runs on pushes to `main`, on pull requests, nightly, and on demand.

| Job | When | What |
|---|---|---|
| `lint` | always | `make lint features` |
| `msrv` | always | `cargo check --workspace --all-targets --all-features` on Rust 1.88, as `make msrv` does |
| `unit` | always | `make test-unit` (no database) |
| `live` | always | TiDB v8.5.8 (the minimum supported version) in docker, then `make test-live TEST_SOCKET=` (no socket, so socket auth reports itself untested) and `make prepare-check` |
| `cluster` | nightly and on demand | installs tiup, then `make db-cluster test-cluster` |
| `audit` | always | `rustsec/audit-check` |

**Status.** Whether the workflow has run green on GitHub has not been checked from this
checkout ([TODO.md](TODO.md) tracks it). Locally, `make lint features`, `make test-unit`, the
MSRV build, the live tier (with the tiup binary) and the cluster tier pass. What only a real
run can show:

- the `live` job against the `pingcap/tidb:v8.5.8` image rather than the tiup binary;
- the tiup install step of the `cluster` job (trigger it once with "Run workflow");
- the `audit` action, `Swatinem/rust-cache` and the runner environment.

**Known gap.** The docker TiDB runs with a UTC system zone. In CI the `timezone=server` path
therefore only covers the trivial UTC case. The explicit `Europe/Paris` and `America/New_York`
time-zone tests do run in CI. Locally, `make db-up` inherits the machine's zone.
