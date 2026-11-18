# TODO

Known open items. Priority: **P1** is a real bug or a hole in a guarantee, **P2** is worth
doing soon, **P3** is nice to have. The [Done](#done) section lists what was completed and where
it is tested.

## Housekeeping

- [ ] **P2: Run CI once for real.** There is no git remote, so `.github/workflows/ci.yml` has
  never run.
  - The v7.5.6 leg was reproduced locally with the same docker image.
  - The v8.5.8 leg was only reproduced with the tiup binary, not with `pingcap/tidb:v8.5.8`.
  - The nightly cluster job installs tiup in CI; that install step is untested.

## Correctness

- [ ] **P2: `DateTime<Utc>` / `OffsetDateTime` silently assume a UTC session.** This is
  documented, but with `timezone=server` on a non-UTC server, TIMESTAMP values are read and
  written shifted, with no error.
  - *Why it's still open:* `Encode` and `Decode` get no connection context. Enforcing it means
    threading a "session is UTC" flag into `Arguments` and `ValueRef`, which is an API-level change.
  - *Option:* refuse to connect when `timezone` is unset and the server zone is not UTC, but
    only if the caller opted into chrono or time.
- [ ] **P3: `Option<T>::produces` for `None`.** It declares `T`'s type (e.g. JSON) for a NULL
  parameter. This is harmless as far as tested (TiDB checks the NULL bitmap first), but it has not
  been verified for every type.
- [ ] **P3: A cancelled run on a borrowed connection keeps the migration lock until reused.**
  Pooled connections release it on return. A `&mut TiConnection` releases it at the next
  migrator call on that connection, or when the connection closes.

## Macros and offline data

- [ ] **P2: Bind argument types are not checked against the server.** TiDB reports every
  parameter as a string, so only the bind count is checked at compile time; argument types are
  only checked by the `Encode` bound.
  *Fix (partial):* infer the expected types from the SQL context (columns compared with `?`).
- [ ] **P3: A renamed dependency breaks the generated code.** Expansions hard-code `::tisqlx::…`.
  *Fix:* support `#[tisqlx(crate = "...")]` or use `proc-macro-crate`.
- [ ] **P3: Indirect macro users go undetected by `tisqlx prepare`.** Workspace members that use
  the macros only through a crate that re-exports them are not detected or cleaned.

## Test infrastructure

- [ ] **P2: Make skipped tests visible.** Without a database, `cargo test` reports skipped live
  tests as passed. `TISQLX_TEST_REQUIRE=1` (and `TISQLX_TEST_REQUIRE_CLUSTER=1`) cover CI and
  `make`.
  *Option:* a summary line, or `#[ignore]` plus `--include-ignored` in the live targets.
- [ ] **P3: Coverage gaps.**
  - `tls.rs`: `verify-ca` and a custom `ssl-ca` are untested. Test them with the CA that
    `auto-tls` generates.
  - `prepare.rs` is only exercised as a subprocess, so coverage does not see it.
  - `executor.rs` is low.
- [ ] **P3: Fuzz the packet decoders.** `cargo fuzz` targets for `Handshake`, `OkPacket`,
  `ColumnDefinition` and the row decoders.
- [ ] **P3: No test for the race between `Pool::close` and a connection being returned.** The
  fix (checking under the idle lock) isn't covered, because the interleaving is hard to force.
- [ ] **P3: No test for a record deleted mid-run.** That `run` fails with `NotApplied` when its
  record disappears is untested.
- [ ] **P3: No cluster test for a node restart.** Pool recovery is tested with `KILL`, not with a
  real TiDB restart (`tiup playground scale-in`).

## Performance

- [ ] **P2: Ping on every pooled acquire.** `test_before_acquire` (default on) adds a round trip
  to every query run through `&Pool`.
  *Option:* only ping connections idle for longer than a threshold. This changes behaviour
  (sqlx also pings every time), so it is left as a decision.
- [ ] **P3: Allocations on every packet.** `recv_packet` allocates and zero-fills a fresh `Vec`
  per packet.
  *Fix:* reuse a buffer for control packets.
- [ ] **P3: Column definitions on cache hits.** They are fully decoded on every execute of a
  cached statement, only to be compared.
  *Fix:* compare the raw bytes.
- [ ] **P3:** each row allocates a second `Vec` of value ranges; use compact offsets.
- [ ] **P3:** `send_execute` copies the bind payload once more. This only matters for large binds.
- [ ] **P3:** statement-cache eviction is O(capacity). Use an LRU list.

## Missing features

- [ ] **P3: Remaining types.**
  - `chrono::DateTime<FixedOffset>`/`Local` (needs the session zone; see above).
  - Network address types for `INET6_ATON` values.
  - The macros can't infer `types::Vector`, because TiDB sends VECTOR columns as text; it needs
    an override.
- [ ] **P3: Fixtures for `#[tisqlx::test]`.** SQL files applied after the migrations.
- [ ] **P3: An `Acquire`-style trait.** Lets functions accept a pool, a connection or a
  transaction uniformly.
- [ ] **P3: Protocol features.** Compression, `LOAD DATA LOCAL INFILE`,
  `COM_STMT_SEND_LONG_DATA` and cursor fetch are not supported.

## Ideas (TiDB-specific)

- Multi-host pools: several TiDB addresses or one DNS name, with health checks.
- `bulk_insert`: batch rows into multi-row `INSERT`s sized to `max_allowed_packet` and TiDB's
  transaction limits.
- Large-table scans in primary-key or `_tidb_rowid` ranges, and wrappers for `BATCH ON … LIMIT`.
- Query tagging (`tidb_session_alias` or comments) for TiDB Dashboard and statement summaries.
- A replica-read pool (`tidb_replica_read`) for stale and read-only traffic.
- Migration DDL progress from `ADMIN SHOW DDL JOBS`, and warnings for DDL that is slow on TiDB.

## Done

"mock" is `tisqlx-core/tests/mock/`, "live" is `tisqlx-core/tests/live/`. Items marked
**(no test)** were fixed by inspection only; the others have a test, and the mock tests were
checked to fail when the fix is reverted.

**Correctness**
- **Migration lock released after a cancelled run.** A run can only be cancelled mid-command, and
  then the connection is discarded and the server frees the lock with the session.
  - Test: live `cancelled_run_releases_the_migration_lock` (it would pass without the new code).
  - The new code covers the remaining case: `RELEASE_LOCK` itself failing. Connections that may
    hold a lock are reset with `RELEASE_ALL_LOCKS()`.
  - Test: mock `migration_lock_is_released_when_release_lock_fails`.
- **Unknown column types read as strings** instead of breaking prepared queries.
  Test: mock `unknown_column_type_is_read_as_a_string`.
- **A failed `ROLLBACK TO SAVEPOINT` rolls back the whole transaction,** and the outer `commit()`
  then fails instead of committing nothing silently.
  Test: mock `failed_savepoint_rollback_aborts_the_whole_transaction`.
- **Pool background work is bounded** by the connect timeout: reset on drop, ping on acquire,
  close. Test: mock `hung_reset_is_abandoned_and_frees_its_slot`.
- **`idle_timeout` and `min_connections` are enforced.** Idle connections are reused most recent
  first, and a maintenance task retires expired connections and tops the pool up.
  Tests: live `maintenance_reaps_idle_connections_and_keeps_the_minimum`,
  `idle_connections_are_reused_most_recent_first`.
- **A failed `BEGIN` with an isolation level** marks the connection broken, so the one-shot level
  cannot leak to the next transaction. **(no test)**
- **Migration bookkeeping holds up against the migration's own SQL.** The migrations table is
  qualified with its database (`USE other` is harmless), and a migration that leaves a
  transaction open fails, is rolled back and stays dirty.
  Test: live `migration_sql_cannot_break_the_bookkeeping`.
- **`migrate revert --all`.** Test: `tisqlx-cli/tests/cli.rs`.
- **Session variables are quoted unless they are plain decimal literals** (`inf`/`NaN` are now
  quoted). **(no test)**

**Macros and offline data**
- **`describe` times out after 30 s.** **(no test)**
- **`query_file!` normalises CRLF**, so its offline data matches on every OS. **(no test)**
- **Offline lookup stops at the workspace root** (the directory with `Cargo.lock`). **(no test)**
- **`tisqlx prepare` honours `-p`/`--package` and `--manifest-path`.** Tests: unit, in
  `prepare.rs`.

**Untested fixes now covered**
- Rollback before a retry: mock `retried_transaction_rolls_back_before_the_next_attempt`.
- Revert failure output: `cli.rs`.
- Lock-timeout rounding: unit test.
- An expired connection's slot: only indirectly (the live pool tests check `size()` after a
  `max_lifetime` replacement; the old transient over-count would not reliably fail them).

**Test infrastructure: the cluster tier**
- **How to run:** `make db-cluster && make test-cluster`.
  - It uses tiup playground with 2 TiDB, PD, TiKV and TiFlash, ports offset by 10000.
  - The script refuses to start on busy ports.
  - CI runs it nightly.
- **Tests:** TiFlash reads, DDL on one node with a cached statement on the other, stale reads
  returning old data, bounded staleness, and pessimistic lock-wait timeouts.
- **Moved to the standard tier,** because they work standalone: the `resource-group` option, and
  pool recovery after `KILL`.

**Features**
- **`Pool::transaction_retry_if` and `tisqlx::error::is_retryable`,** so retries work with
  `anyhow` and other wrapping error types. Test: live `retry_with_a_foreign_error_type`.
- **`#[tisqlx::test]` (with `migrations = "dir"`):** a throwaway database per test.
  Tests: `tisqlx/tests/test_attr.rs`, plus `tests/ui/test_attr_errors.rs` for the compile errors.
- **Types:**
  - `i128`/`u128` for integral DECIMAL and integer columns;
  - `&serde_json::value::RawValue`;
  - `Box<T>`/`Arc<T>` for any bindable `T`.

  Test: live `wide_integers_and_raw_json`.
