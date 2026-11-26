# TODO

Known open items. Priority: **P1** is a real bug or a hole in a guarantee, **P2** is worth
doing soon, **P3** is nice to have. The [Done](#done) section lists what was completed and where
it is tested.

## Housekeeping

- [ ] **P2: Confirm the `audit`, `cluster` and `rawkv` CI jobs are green.** The other jobs
  are; `audit` and `cluster` failed and were changed, `rawkv` is new
  ([TESTING.md § CI](TESTING.md#ci)). Trigger the `cluster` job by hand ("Run workflow"): it
  otherwise runs only nightly.

## Correctness

- [ ] **P3: A cancelled run on a borrowed connection keeps the migration lock until reused.**
  Pooled connections release it on return. A `&mut TiConnection` releases it at the next
  migrator call on that connection, or when the connection closes.
- [ ] **P3: Session variables set by a migration stay on the connection.** After each
  migration the migrator goes back to the original database, but a `SET sql_mode = …` (or any
  other session variable) reaches the next migrations, and the caller or the pool afterwards.
- [ ] **P3: A top-level `commit()` succeeds after the server ended the transaction.** After an
  implicit commit (DDL) or a raw `ROLLBACK`, only nested levels report it. The top-level
  `COMMIT` has nothing left to commit, and returns `Ok`.
- [ ] **P3: A database change the driver can't see leaves cached statements on the old
  database.** Only a statement that starts with `USE` (after any comments) empties the statement cache.
  A `USE` run by a stored procedure or an executable comment, for example, is not seen.
  *Workaround:* `TiConnection::clear_cached_statements()`.

## Macros and offline data

- [ ] **P3: Check more bind positions.** Arguments are checked where the analyzer
  (`tisqlx-macros/src/params.rs`) finds the column next to a `?`.
  - Still unchecked: function arguments, subqueries, the select list, HAVING, UNION and CTEs,
    `INSERT` without a column list, and `INSERT … SELECT`.
  - A SQL parser (e.g. `sqlparser`) could cover more, at the cost of a large compile-time
    dependency and TiDB syntax it may not know.
- [ ] **P3: A renamed dependency breaks the generated code.** Expansions hard-code `::tisqlx::…`.
  *Fix:* support `#[tisqlx(crate = "...")]` or use `proc-macro-crate`.
- [ ] **P3: Indirect macro users go undetected by `tisqlx prepare`.** Workspace members that use
  the macros only through a crate that re-exports them are not detected or cleaned.
- [ ] **P3: The CLI and the macros look up `.env` differently.** Both read the nearest `.env`
  file, but the CLI starts from the current directory and the macros from the crate's
  manifest directory. On a malformed line the CLI warns and ignores the rest of the file; the
  macros skip the line. `#[tisqlx::test]` reads no `.env` at run time.

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
- [ ] **P2: RawKV over TLS.** `tisqlx-rawkv` refuses `https://` PD URLs, and connects to TiKV
  in plain text. It needs a rustls connector (ring, like `tisqlx-core`) with a CA and an
  optional client certificate, plus a TLS playground to test against.
- [ ] **P3: RawKV gaps.** API V2 (keyspace-prefixed keys), reverse scans (they need the region
  *before* a key from PD), `RawBatchScan`, `RawChecksum`.
  Retries after a dropped TiKV connection and after a leader transfer (`NotLeader`) are
  implemented but not tested; testing them needs a playground with 3 TiKV and a
  `pd-ctl operator add transfer-leader`.

## Ideas (TiDB-specific)

- Large-table scans in primary-key or `_tidb_rowid` ranges, and wrappers for `BATCH ON … LIMIT`.
- Query tagging (`tidb_session_alias` or comments) for TiDB Dashboard and statement summaries.
- A replica-read pool (`tidb_replica_read`) for stale and read-only traffic.
- Migration DDL progress from `ADMIN SHOW DDL JOBS`, and warnings for DDL that is slow on TiDB.

## Done

Completed work that isn't obvious from the docs, and the test that covers it. "mock" is
`tisqlx-core/tests/mock/`, "live" is `tisqlx-core/tests/live/`. Items marked **(no test)**
were fixed by inspection only.

**Correctness**
- **Migration lock released after a cancelled run.** A run can only be cancelled mid-command;
  the connection is then discarded and the server frees the lock with the session (live
  `cancelled_run_releases_the_migration_lock`). When `RELEASE_LOCK` itself fails, the
  connection is reset with `RELEASE_ALL_LOCKS()` (mock
  `migration_lock_is_released_when_release_lock_fails`).
- **Unknown column types read as strings** instead of breaking prepared queries: mock
  `unknown_column_type_is_read_as_a_string`.
- **Cached statements follow the current database.** TiDB binds a prepared statement to the
  database current when it was prepared, so a statement starting with `USE` empties the cache:
  live `cached_statement_follows_the_current_database`. An index hint (`USE INDEX`) doesn't:
  unit `use_statements_are_spotted`.
- **A database name with a backtick connects.** TiDB builds its own `USE` from the handshake
  without escaping, so the driver selects such a database itself: live
  `database_name_with_a_backtick`.
- **An implicit commit (DDL) is reported at nested levels** instead of error 1305: live
  `ddl_inside_a_nested_transaction`.
- **`rows_affected()` counts matched rows** (`CLIENT_FOUND_ROWS`): live
  `rows_affected_counts_matched_rows`.
- **`clear_cached_statements` closes the statements at once:** mock
  `clearing_the_statement_cache_closes_the_statements_at_once`.
- **A failed `ROLLBACK TO SAVEPOINT` rolls back the whole transaction,** and the outer
  `commit()` then fails: mock `failed_savepoint_rollback_aborts_the_whole_transaction`.
- **A retried transaction rolls back before the next attempt:** mock
  `retried_transaction_rolls_back_before_the_next_attempt`.
- **Pool background work is bounded** by the connect timeout (reset on drop, ping on acquire,
  close): mock `hung_reset_is_abandoned_and_frees_its_slot` covers the reset; the ping and
  the close have **(no test)**.
- **`idle_timeout` and `min_connections` are enforced,** and idle connections are reused most
  recent first: live `maintenance_reaps_idle_connections_and_keeps_the_minimum`,
  `idle_connections_are_reused_most_recent_first`. `idle_timeout` only retires connections
  above `min_connections` (it used to close and reopen them every period): live
  `idle_timeout_keeps_the_minimum_connections_open`.
- **The balancer forgets servers that left the host list** (DNS) once they have no connection:
  unit `servers_no_longer_listed_are_forgotten_once_empty`.
- **A bulk insert whose batch was cancelled mid-send fails** instead of reporting success, and
  `on_duplicate_key_update(&[])` is a configuration error: live
  `a_cancelled_batch_fails_the_writer`, `configuration_errors`.
- **Migration bookkeeping is committed when autocommit is off:** live
  `bookkeeping_is_committed_when_autocommit_is_off`.
- **A failed migration's open transaction is rolled back,** so a later `repair` can't commit
  the half-done migration: live `failed_migration_does_not_leave_its_transaction_open`.
- **`fetch_one` and `fetch_optional` read the whole response,** so the connection goes straight
  back to the pool instead of through a background reset: live
  `sequential_fetch_one_keeps_using_one_connection`.
- **A huge `connect_timeout` means no timeout** instead of a panic: mock
  `huge_connect_timeout_means_no_timeout`.
- **`min_connections` is capped at `max_connections`.** The maintenance task used to open
  connections above the maximum: live `maintenance_never_opens_more_than_max_connections`.
- **Nested transactions inside a stale read are no-ops.** TiDB accepts `SAVEPOINT` there but
  creates nothing, so the nested commit or rollback failed with 1305 and rolled the stale read
  back: live `nesting_inside_a_stale_read`.
- **Inexact decodes are errors.** A DECIMAL too wide for `Decimal` and a DOUBLE out of `f32`
  range used to round or become infinity: live
  `inexact_conversions_are_errors_in_both_protocols`. A date type no longer drops a
  DATETIME's time of day, and the zero date is an error, not `None`: live
  `dates_from_datetimes_and_zero_dates_in_both_protocols`.
- **Configuration mistakes are configuration errors:** mock
  `zero_connect_timeout_is_a_configuration_error`, `no_hosts_is_a_configuration_error`,
  `empty_session_variable_name_is_a_configuration_error`.
- **Pool pings only after `ping_after_idle`:** mock `recently_returned_connection_is_not_pinged`,
  live `recently_used_connection_killed_by_the_server_fails_once`.
- **Migration bookkeeping holds up against the migration's own SQL.** The migrations table is
  qualified with its database, the connection goes back to its original database after every
  migration (so a `USE` reaches neither the next migration nor the caller), and a migration
  that leaves a transaction open fails and stays dirty (live
  `migration_sql_cannot_break_the_bookkeeping`). One that deletes its own record
  fails with `NotApplied` (live `record_deleted_by_the_migration_fails_the_run`).
- **The migration lock timeout is rounded up to whole seconds:** unit test in `migrate.rs`.
- **TiDB older than 8.5 is refused:** mock `tidb_older_than_the_minimum_is_rejected`.
- **A NULL parameter of every Rust type works in every column kind,** in writes and in
  comparisons: live `null_of_every_type_into_every_column`.
- **Hostile input** (`tisqlx-core/src/robustness.rs`): every packet, row and value decoder gets
  random bytes and corrupted valid packets, and must return an error, never panic. It is a
  deterministic stand-in for `cargo fuzz`, which needs nightly; `TISQLX_FUZZ_ITERS` runs it
  longer. It found two overflows, in TIME hours and in binary DATETIME microseconds.
- **(no test)** A failed `BEGIN` with an isolation level marks the connection broken, so the
  one-shot level cannot leak to the next transaction.
- **(no test)** Session variables are quoted unless they are plain decimal literals
  (`inf`/`NaN` are quoted).
- **(no reliable test)** An expired connection's pool slot is no longer over-counted. The live
  pool tests check `size()` after a `max_lifetime` replacement, but the old transient
  over-count would not reliably fail them.

**Macros and offline data**
- **Bind arguments are type-checked** where the analyzer finds the column next to a `?`:
  unit `params::tests` and `bind_check::tests`, acceptance in `tisqlx/tests/macros.rs`
  (`bind_arguments_of_compatible_types`), rejection in `tests/ui-query/bind_types.rs`.
  Unsuffixed numeric literals are classified from the syntax, since their type is still being
  inferred when the check runs.
- **A `LIKE` pattern is checked only against text and binary columns,** and a statement with
  an executable comment is not analysed: unit `like_patterns_are_told_apart_from_values`,
  `executable_comments_disable_the_analysis`.
- **Unaliased expression names are not parsed as overrides** (`DATE_FORMAT(c, '%H:%i')`), and
  **a directory named `.env` no longer hangs the build:** unit `query::tests::overrides`,
  `describe::tests::a_directory_named_dotenv_is_skipped`.
- **The derives reject attributes they would ignore,** and `rename_all` keeps an acronym as
  one word: `tests/ui/from_row_rename.rs`, `tests/ui/type_derive_errors.rs`, unit
  `acronyms_are_one_word`.
- **The CLI reports a malformed `.env`,** and `migrate build-script` escapes any path:
  `tisqlx-cli/tests/cli.rs` (`dotenv_lookup`, `build_script_compiles_for_any_path`).
- **`tisqlx prepare` honours `-p`/`--package` and `--manifest-path`.** `--workspace` and
  `--all-targets` may be passed again, and with `-p`/`--exclude` the other packages' files are
  kept and not reported stale: unit tests in `prepare.rs`. A relative `--manifest-path` works
  from any directory **(no test)**.
- **(no test)** `describe` times out after 30 s; `query_file!` normalises CRLF, so its offline
  data matches on every OS; the offline lookup stops at the workspace root (the directory with
  `Cargo.lock`).

**Session time zones** (`tisqlx-core/src/tz.rs`)
- Zone data comes from jiff with the system tzdata (bundled only on Windows). Around DST
  changes, wall times map to instants exactly as TiDB maps them (Go's `time.Date`, with gap
  times clamped to the transition).
- Tests: unit `tz::tests`; live `timezone::*` (round trips in Europe/Paris, and DST parity with
  `UNIX_TIMESTAMP` in Paris and New York).

**Features** (described in [docs/](docs/README.md))
- **`BulkInsert`:** unit tests for statement building and the time-zone offsets of appended
  rows; live `bulk::*` for batching by rows and by bytes, instants and NULLs in a Paris session,
  the three conflict modes, a failed second batch, and all or nothing inside a transaction.
  `SHOW CONFIG` falls back to the 100 MB default of `txn-total-size-limit` when it isn't allowed.
- **Several TiDB servers per pool:** unit `balance::tests` and URL parsing; mock (spread over two
  servers, a dead one skipped, an unresponsive one only costs its share of the timeout, a
  connection lost during the TLS handshake and a full server (1040) fail over, a wrong
  password doesn't); live (a dead host in the list, `localhost` with and without DNS
  balancing); cluster (both TiDB nodes get connections).
- **`Acquire`:** live `acquire_accepts_pools_connections_and_transactions`.
- **`#[tisqlx::test]`, with migrations and fixtures:** `tisqlx/tests/test_attr.rs`, and
  `tests/ui/test_attr_errors.rs` for the compile errors.
- **`Pool::transaction_retry_if` and `tisqlx::error::is_retryable`:** live
  `retry_with_a_foreign_error_type`.
- **`i128`/`u128`, `&RawValue`, `Box<T>`/`Arc<T>`:** live `wide_integers_and_raw_json`.
- **`migrate revert --all`,** and the output of a failed revert: `tisqlx-cli/tests/cli.rs`.

**Test infrastructure**
- **Cluster tier** (`make db-cluster test-cluster`, see [TESTING.md](TESTING.md)): TiFlash
  reads, DDL on one node with a cached statement on the other, stale reads returning old data,
  and pessimistic lock-wait timeouts. The `resource-group` option and pool recovery after
  `KILL` work standalone, so they are in the live tier.
- **Skipped tests are visible:** every test that needs a database is `#[ignore]`d with the
  `make` target that runs it.
