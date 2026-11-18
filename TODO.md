# TODO

Known open items. Priority: **P1** is a real bug or a hole in a guarantee, **P2** is worth
doing soon, **P3** is nice to have. The [Done](#done) section lists what was completed and where
it is tested.

## Housekeeping

- [ ] **Deferred: run CI once for real**, when the repository has a remote. Until then
  `.github/workflows/ci.yml` has never run. [TESTING.md § CI](TESTING.md#ci) lists what was
  checked locally, what wasn't, and the steps for the first run.

## Correctness

- [ ] **P3: A cancelled run on a borrowed connection keeps the migration lock until reused.**
  Pooled connections release it on return. A `&mut TiConnection` releases it at the next
  migrator call on that connection, or when the connection closes.

## Macros and offline data

- [ ] **P3: Check more bind positions.** Arguments are checked where the analyzer
  (`tisqlx-macros-core/src/params.rs`) finds the column next to a `?`.
  - Still unchecked: function arguments, subqueries, the select list, HAVING, UNION and CTEs,
    `INSERT` without a column list, and `INSERT … SELECT`.
  - A SQL parser (e.g. `sqlparser`) could cover more, at the cost of a large compile-time
    dependency and TiDB syntax it may not know.
- [ ] **P3: A renamed dependency breaks the generated code.** Expansions hard-code `::tisqlx::…`.
  *Fix:* support `#[tisqlx(crate = "...")]` or use `proc-macro-crate`.
- [ ] **P3: Indirect macro users go undetected by `tisqlx prepare`.** Workspace members that use
  the macros only through a crate that re-exports them are not detected or cleaned.

## Test infrastructure

- [ ] **P3: Coverage gaps.**
  - `tls.rs`: `verify-ca` and a custom `ssl-ca` are untested. Test them with the CA that
    `auto-tls` generates.
  - `prepare.rs` is only exercised as a subprocess, so coverage does not see it.
  - `executor.rs` is low.
- [ ] **P3: No test for the race between `Pool::close` and a connection being returned.** The
  fix (checking under the idle lock) isn't covered, because the interleaving is hard to force.
- [ ] **P3: No cluster test for a node restart.** Pool recovery is tested with `KILL`, not with a
  real TiDB restart (`tiup playground scale-in`).

## Performance

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
  - Network address types for `INET6_ATON` values.
  - The macros can't infer `types::Vector`, because TiDB sends VECTOR columns as text; it needs
    an override.
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

**Session time zones** (`tisqlx-core/src/tz.rs`)
- **Instant types follow the session `time_zone`.** `DateTime<Utc>`, the new
  `DateTime<FixedOffset>` and `DateTime<Local>`, and `OffsetDateTime` are converted on the
  client: bind parameters at execute time, values at decode.
  - Zone data comes from jiff with the system tzdata (bundled only on Windows).
  - Around DST changes, wall times map to instants exactly as TiDB maps them (Go's
    `time.Date`, with gap times clamped to the transition).
  - `TiConnection::reload_time_zone()` handles a raw `SET time_zone`, since TiDB has no
    session tracking.
- Tests:
  - unit `tz::tests`;
  - live `timezone::*`: round trips through DATETIME and TIMESTAMP in Europe/Paris, and
    DST parity with `UNIX_TIMESTAMP` in Paris and New York, on 8.5.8 and 7.5.6. Both were
    checked to fail when the conversion is removed.

**Pool pings only after `ping_after_idle`** (default 30 s; `Duration::ZERO` pings every time).
Tests: mock `recently_returned_connection_is_not_pinged`, live
`recently_used_connection_killed_by_the_server_fails_once`.

**Bind arguments are type-checked at compile time** where the column next to a `?` can be
found.
- **How:**
  - a lexical analyzer finds the column;
  - a probe `SELECT <columns> FROM <the query's FROM>` gets its type from the server;
  - autoref dispatch checks the argument's kind (`tisqlx_core::bind_check`).
- **Never rejected:** types tisqlx cannot classify, including generic arguments and derived
  newtypes over such types. `value as _` opts one argument out.
- **Unsuffixed numeric literals** are classified from the syntax, since their type is still
  being inferred when the check runs.
- **Offline data** stores the inferred types: 31 of the 37 parameters in the repo's 18 queries
  are checked.
- **Tests:**
  - unit `params::tests` (including placeholders inside strings and comments) and
    `bind_check::tests`;
  - acceptance in `tisqlx/tests/macros.rs` (`bind_arguments_of_compatible_types`);
  - rejection in `tests/ui-query/bind_types.rs`.

**Skipped tests are visible.** Every test that needs a database is `#[ignore]`d with the
`make` target that runs it, so a plain `cargo test` reports them as ignored, not passed.
`make test-live`/`test-cluster` pass `--include-ignored`.

**More coverage**
- A NULL parameter of every Rust type is ignored in every column kind, in writes and in
  comparisons (35 types × 14 column kinds). Test: live `null_of_every_type_into_every_column`.
- A migration that deletes its own record fails with `NotApplied`. Test: live
  `record_deleted_by_the_migration_fails_the_run`.

**Hostile input** (`tisqlx-core/src/robustness.rs`): every packet, row and value decoder gets
random bytes and corrupted valid packets, and must return an error, never panic. It is a
deterministic stand-in for `cargo fuzz`, which needs nightly. `TISQLX_FUZZ_ITERS` runs it
longer; 1M iterations are clean. It found and fixed:
- a TIME with a huge hour count overflowed: a panic in debug builds, a wrong value in release;
- a binary DATETIME with microseconds ≥ 10⁶ overflowed the time-zone conversion.

**Features**
- **`Acquire`:** one function for a pool, a connection, a pooled connection or a transaction;
  `begin()` nests as a savepoint. Test: live `acquire_accepts_pools_connections_and_transactions`.
- **Fixtures for `#[tisqlx::test]`:** `fixtures("a.sql", ...)` runs SQL files after the
  migrations, in order. Tests: `tisqlx/tests/test_attr.rs` (`applies_fixtures_in_order`) and the
  compile errors in `tests/ui/test_attr_errors.rs`.
- **`Pool::transaction_retry_if` and `tisqlx::error::is_retryable`,** so retries work with
  `anyhow` and other wrapping error types. Test: live `retry_with_a_foreign_error_type`.
- **`#[tisqlx::test]` (with `migrations = "dir"`):** a throwaway database per test.
  Tests: `tisqlx/tests/test_attr.rs`, plus `tests/ui/test_attr_errors.rs` for the compile errors.
- **Types:**
  - `i128`/`u128` for integral DECIMAL and integer columns;
  - `&serde_json::value::RawValue`;
  - `Box<T>`/`Arc<T>` for any bindable `T`.

  Test: live `wide_integers_and_raw_json`.
