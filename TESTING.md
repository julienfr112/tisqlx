# Testing tisqlx

## Tiers

| tier | needs | what it covers | run with |
|---|---|---|---|
| **Unit** | nothing | Pure logic, each module's `#[cfg(test)]`: the wire codec, URL options, auth scrambles, savepoint SQL, the migration planner (`validate`/`pending`/`to_revert`), the macro type mapping checked against the runtime `Type::compatible`, and CLI version naming. | `make test-unit` |
| **Mock protocol** | nothing | `tisqlx-core/tests/mock/`: a scripted MySQL-protocol server sends exact packet sequences. It tests what a real server can't reproduce on demand: futures dropped mid-response, multiple result sets, malformed packets, the 8113 re-prepare, deferred `STMT_CLOSE`, stale prepared metadata, hostile handshakes (clear-text auth, unknown plugins, non-TiDB servers), and pool cleanup after a dropped mid-result stream. | `make test-unit` |
| **Derive compile tests** | nothing | `tisqlx/tests/derives.rs` checks that the derives generate impls for generic, borrowed and nested types; `tests/ui/*.rs` checks the derive compile errors (trybuild). | `make test-unit` |
| **Live** | dedicated TiDB | `tisqlx-core/tests/live/`: every feature against a real server, one throwaway database per test. Covers auth over TLS, the Unix socket and plain TCP, all types in both protocols, transactions and abandoned savepoints, pool accounting and lock release, write-conflict retry, stale reads, migrations, and 17 MB packets. | `make test-live` |
| **Macros (live)** | dedicated TiDB | `tisqlx/tests/macros.rs` compiles the `query!` family online and runs the queries. `tests/ui-query/*.rs` checks the query macro compile errors (bind count, bad column names, bad SQL, type mismatches). | `make test-live` |
| **`#[tisqlx::test]`** | dedicated TiDB | `tisqlx/tests/test_attr.rs` uses the attribute itself: a database per test, migrations, `Result` returns. Without a database these show as *ignored*; `make test-live` runs them with `--include-ignored`. | `make test-live` |
| **Cluster** | tiup playground | `tisqlx-core/tests/live/cluster.rs` needs a real cluster: TiFlash reads, DDL on one TiDB node while the other holds a cached statement, stale reads returning old data, and TiKV lock-wait timeouts. | `make db-cluster test-cluster` |
| **CLI** | both tiers | `tisqlx-cli/tests/cli.rs` runs the real binary: `migrate add`/`build-script` without a DB, and a full lifecycle against TiDB (setup, dry runs, revert, dirty migration, repair). | `make test-unit` / `make test-live` |
| **Offline data** | dedicated TiDB | `tisqlx prepare --check`: the committed `.tisqlx/` still matches the schema. | `make prepare-check` |

Live tests **skip** unless `TISQLX_TEST_URL` is set, so `cargo test` works anywhere.
`make test-live` and CI set `TISQLX_TEST_REQUIRE=1`, which turns a missing database into a failure.

## The test database

Tests never touch a shared server. `make db-up` starts a dedicated TiDB v8.5.8 with `scripts/tidb.sh`:

- It runs standalone (unistore, no PD/TiKV) on port **4100**.
- It has TLS (`auto-tls`) and a Unix socket at `target/tidb/tidb.sock`.
- It uses the tiup `tidb-server` binary, or docker with `TIDB_RUNNER=docker`; CI uses docker.

```sh
make db-up        # once
make test         # unit + live
make db-down
```

The **cluster tier** runs a real cluster with `tiup playground` v8.5.8: 2 TiDB nodes, PD,
TiKV and TiFlash.
- Every port is shifted by 10000 (TiDB on 14000/14001), so it can run beside a default
  playground. `scripts/tidb.sh` refuses to start if those ports are taken.
- It starts in seconds once binaries are cached, and uses a few GB of RAM.
- `make db-cluster-down` stops it and removes its data. CI runs this tier nightly.

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
| `make lint` | `rustfmt --check` and clippy with `-D warnings` (workspace lints in `Cargo.toml`) |
| `make features` | the facade builds with every supported feature set |
| `make msrv` | builds on Rust 1.88 |
| `make audit` | RustSec advisories |
| `make coverage` | `cargo llvm-cov` HTML report over the live tier (about 81% of lines) |
| `make ci` | lint + features + test-live + prepare-check |

CI (`.github/workflows/ci.yml`) also runs the live tier against TiDB v7.5.6 (verified locally with the same docker image; version-dependent tests such as `VECTOR` skip with a reason).
