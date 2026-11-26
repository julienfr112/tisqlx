# How-to guides

Recipes for one task each. They assume you have done the [tutorial](tutorial.md). The options
and defaults are in the [reference](reference.md); the reasons are in the
[explanation](explanation.md).

- [Use transactions](#use-transactions): [retry on conflicts](#retry-on-conflicts),
  [mode and isolation](#mode-and-isolation), [stale reads](#stale-reads),
  [nested transactions](#nested-transactions)
- [Write a function that takes a pool or a transaction](#write-a-function-that-takes-a-pool-or-a-transaction)
- [Load many rows](#load-many-rows)
- [Pass a value whose type differs from the column](#pass-a-value-whose-type-differs-from-the-column)
- [Apply migrations when the application starts](#apply-migrations-when-the-application-starts)
- [Repair a failed migration](#repair-a-failed-migration)
- [Test against a database](#test-against-a-database)
- [Build without a database](#build-without-a-database)
- [Connect to several TiDB servers](#connect-to-several-tidb-servers)
- [Connect to TiDB Cloud](#connect-to-tidb-cloud)
- [Serve an HTTP API with axum](#serve-an-http-api-with-axum)
- [Use TiKV directly (RawKV)](#use-tikv-directly-rawkv)

## Use transactions

### Begin, commit, roll back

```rust
let mut tx = pool.begin().await?;
tisqlx::query!("UPDATE accounts SET balance = balance - ? WHERE id = ?", 10, from).execute(&mut *tx).await?;
tisqlx::query!("UPDATE accounts SET balance = balance + ? WHERE id = ?", 10, to).execute(&mut *tx).await?;
tx.commit().await?;
```

A transaction dropped without `commit()` is rolled back, so an early return with `?` is safe.
`tx.rollback().await?` does it explicitly.

### Retry on conflicts

TiDB can refuse a transaction that did nothing wrong: a write conflict at commit in optimistic
mode, a deadlock or a lock wait timeout in pessimistic mode. The fix is to run the whole
transaction again. `Pool::transaction` does that: it begins, runs the closure, commits, and on
a retryable error starts over.

```rust
pool.transaction(|tx| Box::pin(async move {
    tisqlx::query!("UPDATE accounts SET balance = balance - ? WHERE id = ?", 10, from).execute(&mut **tx).await?;
    tisqlx::query!("UPDATE accounts SET balance = balance + ? WHERE id = ?", 10, to).execute(&mut **tx).await?;
    Ok::<_, tisqlx::Error>(())
})).await?;
```

- **The closure may run several times**, so keep side effects other than SQL out of it (sending
  an email, incrementing a counter).
- **`RetryPolicy`**: 5 attempts by default. The wait before a retry is random, up to a bound
  that doubles from 10 ms to 1 s. Change it with `transaction_with`:
  `RetryPolicy::default().max_attempts(10)`; the bounds are the public fields
  `initial_backoff` and `max_backoff`.
- **Your own error type** works when it implements `From<tisqlx::Error>` and `Retryable`. For a
  type you can't implement `Retryable` for, such as `anyhow::Error`, pass the test yourself:

  ```rust
  pool.transaction_retry_if(&TxnOptions::default(), RetryPolicy::default(),
      |e: &anyhow::Error| tisqlx::error::is_retryable(e.as_ref()),
      |tx| Box::pin(async move { /* ... */ Ok::<_, anyhow::Error>(()) })).await?;
  ```

### Mode and isolation

`begin_with` and `transaction_with` take a `TxnOptions`. Without one, a transaction uses the
session's mode and isolation (the `txn-mode` of the [connection URL](reference.md#connection-url), else the
server's).

```rust
use tisqlx::{Isolation, RetryPolicy, TxnMode, TxnOptions};

// Optimistic: no locks until commit, so conflicts surface there. Pair it with a retry.
pool.transaction_with(&TxnOptions::optimistic(), RetryPolicy::default(), |tx| Box::pin(async move {
    /* ... */
    Ok::<_, tisqlx::Error>(())
})).await?;

// Pessimistic: rows are locked as they are written.
let tx = pool.begin_with(&TxnOptions::pessimistic()).await?;

// Mode and isolation together.
let tx = pool.begin_with(&TxnOptions::ReadWrite {
    mode: Some(TxnMode::Pessimistic),
    isolation: Some(Isolation::ReadCommitted),
}).await?;

// Async commit and one-phase commit without the linearizability guarantee.
let tx = pool.begin_with(&TxnOptions::CausalConsistencyOnly).await?;
```

### Stale reads

A read-only transaction at a past timestamp. It takes no locks and any replica can serve it,
which keeps read traffic off the leaders.

```rust
use std::time::Duration;
use tisqlx::{StaleRead, TxnOptions};

// The freshest data that is at most 5 s old, from the nearest replica.
let mut tx = pool.begin_with(&TxnOptions::stale_read(StaleRead::MaxStaleness(Duration::from_secs(5)))).await?;
```

| `StaleRead::` | reads |
|---|---|
| `ExactStaleness(duration)` | as of exactly that long ago |
| `MaxStaleness(duration)` | the freshest data no older than that (`TIDB_BOUNDED_STALENESS`) |
| `At(NaiveDateTime)` | at that instant, in the session time zone |
| `Tso(tso)` | at that TiDB timestamp |

### Nested transactions

`tx.begin()` inside a transaction is a savepoint: rolling it back undoes only what ran since.

```rust
let mut tx = pool.begin().await?;
let mut inner = tx.begin().await?;      // SAVEPOINT
/* ... */
inner.rollback().await?;                // ROLLBACK TO SAVEPOINT; `tx` is still open
tx.commit().await?;
```

- **Options** belong to the outer transaction: a nested level takes none.
- **Inside a stale read** nesting sends nothing: a stale read is read-only, and TiDB has no
  savepoints in it.

### DDL inside a transaction

DDL commits the transaction, as always in TiDB. A nested `commit()` or
`rollback()` after it fails with an error that says so, and the outer `commit()` fails too.
Without a nested level nothing is reported: the top-level `commit()` succeeds, with nothing
left to commit.

## Write a function that takes a pool or a transaction

Write a function once with `Acquire`, and call it with `&pool`, `&mut conn` or `&mut tx`. Its
`begin()` is a transaction on a pool or a connection, and a savepoint inside a transaction.

```rust
use tisqlx::Acquire;

async fn rename<'c>(db: impl Acquire<'c>, id: i64, name: &str) -> tisqlx::Result<()> {
    let mut tx = db.begin().await?;
    tisqlx::query!("UPDATE users SET name = ? WHERE id = ?", name, id).execute(&mut *tx).await?;
    tx.commit().await
}

rename(&pool, 1, "alice").await?;       // its own transaction
rename(&mut tx, 1, "alice").await?;     // part of `tx`
```

`db.acquire()` gives a connection without beginning anything. In a task spawned with
`tokio::spawn`, such an `async fn` may fail to compile with "implementation of `Acquire` is not
general enough". Return a boxed future instead: `-> BoxFuture<'c, Result<T>>` (from the
`futures` crate) with `A: Acquire<'c> + 'c`.

## Load many rows

One row per `INSERT` is the slowest way to load TiDB. `BulkInsert` batches rows into
multi-row prepared statements. Measured with 10 connections on the local playground cluster
(`make bench-cluster`): about 20× more rows per second than row by row (209 000 vs 10 200
rows/s).

```rust
use tisqlx::BulkInsert;

// All at once, or streamed with `start()` / `push()` / `finish()`:
let rows = users.iter().map(|u| (&u.name, &u.email));
BulkInsert::new("users", &["name", "email"]).execute(&pool, rows).await?;
```

- **Batch size:** up to 1000 rows, and within the byte budget read from the server
  (`max_allowed_packet`, and a quarter of `txn-total-size-limit`). Full batches reuse one
  prepared statement.
- **On a duplicate key:** an error by default; `.ignore()` skips the row, `.replace()`
  replaces it, and `.on_duplicate_key_update(&["email"])` updates those columns.
- **Failures outside a transaction:** each batch commits on its own. When one fails, the
  writer stops, and its `rows_written()` says how many rows the earlier batches committed
  (`execute` returns only the error).
  A `push` cancelled while its batch is being sent stops it too: `finish()` then returns an
  error.
- **Inside a transaction** (`execute(&mut tx, …)`): the load is all or nothing, but it must then
  fit TiDB's transaction size limit.
- **`finish()` sends the last batch.** Dropping a writer that still holds rows sends nothing
  and logs a warning.
- **Configuration errors:** no column, a column listed twice, or
  `on_duplicate_key_update(&[])`.

## Pass a value whose type differs from the column

The query macros reject an argument whose kind differs from the column's
([what is checked](reference.md#query-macros), [why](explanation.md#why-the-bind-check-is-strict)).
For example, an id that arrives as a `String`:

1. **Convert in Rust (preferred).** The error then surfaces where the input is parsed:
   ```rust
   let id: i64 = id.parse()?;
   query!("SELECT name FROM users WHERE id = ?", id)
   ```
   Likewise `NaiveDate::parse_from_str(s, "%Y-%m-%d")?`, `serde_json::from_str(s)?`, and
   `Uuid::parse_str(s)?`.
2. **Convert in SQL.** A `?` inside a function call is not checked, and TiDB converts explicitly:
   ```rust
   query!("SELECT name FROM users WHERE id = CAST(? AS SIGNED)", id_text)
   ```
   Other conversions: `STR_TO_DATE(?, '%Y-%m-%d')`, `CAST(? AS JSON)`, `UNHEX(?)`,
   `FROM_UNIXTIME(?)`. Unlike an implicit conversion, an explicit one fails or warns predictably.
3. **Skip the check for one argument:** `query!("… WHERE id = ?", id_text as _)`. TiDB then
   converts implicitly at runtime, with the
   [hazards of implicit conversion](explanation.md#why-the-bind-check-is-strict).
4. **A domain type:** implement `Type` and `Encode` by hand. A type tisqlx can't classify is not
   checked; use `#[derive(Type)]` instead to be checked like the inner type.
5. **No compile-time check at all:** the runtime API, `tisqlx::query("…").bind(v)`.

## Apply migrations when the application starts

`migrate!` embeds the `migrations` directory in the binary, and `run` applies what is pending:

```rust
static MIGRATOR: tisqlx::migrate::Migrator = tisqlx::migrate!();   // or migrate!("db/migrations")

MIGRATOR.run(&pool).await?;
```

- **Several instances may start at once.** A run holds a lock on the server (`GET_LOCK`), so
  the others wait for it, up to `lock_timeout` (10 minutes), and then find nothing pending.
- **New files are not noticed by cargo on their own.** Run `tisqlx migrate build-script` once:
  it writes a `build.rs` that rebuilds the crate when the directory changes.
- **A migration file changed after it was applied** fails the run (its checksum is recorded).

## Repair a failed migration

A migration that fails stays recorded as *dirty*, and every later run stops with
"migration … is partially applied (dirty)" ([why](explanation.md#migrations)).

1. Look at what it did: `tisqlx migrate info` shows the dirty version. TiDB committed every
   DDL statement that ran before the failure.
2. Fix the schema by hand, one way or the other:
   - finish the migration's remaining statements yourself, then record it as applied:
     `tisqlx migrate repair <version> --applied`;
   - or undo what it did, fix the file, and let the next run apply it again:
     `tisqlx migrate repair <version> --remove`.
3. `tisqlx migrate run`.

## Test against a database

```rust
#[tisqlx::test(migrations = "migrations")]
async fn creates_a_user(pool: tisqlx::Pool) {
    tisqlx::query!("INSERT INTO users (name) VALUES (?)", "a").execute(&pool).await.unwrap();
}

#[tisqlx::test(migrations = "migrations", fixtures("tests/fixtures/users.sql", "tests/fixtures/posts.sql"))]
async fn lists_posts(pool: tisqlx::Pool) {
    // users.sql and posts.sql have run, in that order
}
```

- **Arguments**, both optional: `migrations = "dir"` (one directory) and
  `fixtures("a.sql", …)`. Anything else is a compile error, and so is a missing fixture file.
- **The function** is `async`, takes nothing or one `tisqlx::Pool`, and may return a `Result`.
- **Fixtures** are SQL files (several statements allowed) applied after the migrations, in
  the order given. Their paths are relative to the crate root, like `migrations`. They are
  embedded at compile time, so editing one rebuilds the tests that use it.
- **The database:** each test gets its own (`_tisqlx_test_<name>_<hash>`) on the server named
  by `DATABASE_URL`. It is recreated at the start of each run and dropped when the test
  returns. If the test panics, it is left in place so you can inspect it.
- **`DATABASE_URL` at run time** comes from the environment only: the test binary does not
  read `.env`, unlike the macros at compile time.

## Build without a database

Run `tisqlx prepare` with a database available and commit
`.tisqlx/`. Then build with `TISQLX_OFFLINE=true`. `tisqlx prepare --check` fails when the
data is stale. The macros look for `.tisqlx/` in the crate's directory, then upwards as far
as the workspace root (the directory with `Cargo.lock`).

In CI:

```sh
tisqlx prepare --check        # in a job that has a database: fails when .tisqlx/ is stale
TISQLX_OFFLINE=true cargo build
```

Without `DATABASE_URL` the macros use the offline data anyway; `TISQLX_OFFLINE=true` makes it
explicit, and wins over a `DATABASE_URL` that is set.

## Connect to several TiDB servers

List them in the URL (`tidb://user@tidb-0:4000,tidb-1:4000,tidb-2/db`; hosts without a port
use 4000) or with `ConnectOptions::hosts`.

- **Balancing:** a `Pool` opens each new connection on the server with the fewest of its
  connections.
- **Failover:** a server that fails to connect is tried last for a while (1 s, doubling up to
  30 s), so it is only used when every other server fails too.
- **Timeouts:** each attempt gets its share of `connect-timeout`, so an unresponsive server
  can't use it all up.
- **No failover on other errors:** only network errors and a server at its connection limit
  (error 1040) move on to the next server; a wrong password fails at once.
- **Single connections:** `TiConnection::connect` tries the servers in order.
- **Unix socket:** with `socket`, the host list is not used.
- **One DNS name with several addresses** (e.g. a Kubernetes headless service): add
  `load-balance-dns=true` to balance over its addresses.
- **Moving existing connections:** they stay where they are, so after an outage a recovered
  server fills up again as connections are replaced or added.
- **Monitoring:** `Pool::host_connections()` shows the spread, and `server_addr()` on a
  connection says where it landed.

## Connect to TiDB Cloud

Verify the server's certificate and host name; the default mode encrypts without verifying.

```rust
let pool = Pool::connect("tidb://USER:PASSWORD@HOST:4000/DB?ssl-mode=verify-identity").await?;
```

The certificate is checked against the webpki roots. For a private CA, add
`&ssl-ca=/path/to/ca.pem`. Percent-encode special characters of the user and the password.

## Serve an HTTP API with axum

[`tisqlx/examples/axum.rs`](../tisqlx/examples/axum.rs) is a complete service in one file:
the pool as axum state, migrations at startup, checked queries, a stale read, a bulk import, a
transaction retried on conflicts, and a clean shutdown.

```sh
tisqlx database create -D tidb://root@127.0.0.1:4000/blog
DATABASE_URL=tidb://root@127.0.0.1:4000/blog cargo run -p tisqlx --example axum
```

The parts worth copying:

- **State:** `Pool` is cheap to clone, so it is the state itself: `.with_state(pool)` and
  `State(pool): State<Pool>`.
- **Errors:** one application error type with `From<tisqlx::Error>`, turned into a response
  in one place:

  ```rust
  impl IntoResponse for AppError {
      fn into_response(self) -> Response {
          let status = match &self {
              Self::NotFound | Self::Db(tisqlx::Error::RowNotFound) => StatusCode::NOT_FOUND,
              Self::Db(tisqlx::Error::PoolTimedOut) => StatusCode::SERVICE_UNAVAILABLE,
              Self::Db(e) if e.as_database_error().is_some_and(|db| db.is_unique_violation()) => StatusCode::CONFLICT,
              _ => StatusCode::INTERNAL_SERVER_ERROR,
          };
          status.into_response()
      }
  }
  ```
- **Retried transactions:** implement `Retryable` for that error type, and
  `pool.transaction(...)` accepts closures that return it. The closure may run several times
  and its future can't borrow from the handler, so clone the input inside it.
- **Ids:** an `AUTO_RANDOM` id uses all 64 bits. Send it as a string in JSON, or JavaScript
  rounds it.
- **Shutdown:** after `axum::serve(...).with_graceful_shutdown(...)` returns, call
  `pool.close().await`.

## Use TiKV directly (RawKV)

`tisqlx-rawkv` talks to TiKV's raw key-value API without TiDB: plain keys and values, no
transactions. It speaks gRPC to PD and TiKV and needs no `protoc` or C library (its protobuf
messages are written by hand from kvproto's release-8.5).

```toml
[dependencies]
tisqlx-rawkv = "0.2"
```

```rust
use tisqlx_rawkv::RawClient;

let kv = RawClient::connect(["127.0.0.1:2379"]).await?;    // PD endpoints
kv.put("app/user/1", "alice").await?;
let v = kv.get("app/user/1").await?;                        // Option<Vec<u8>>
kv.batch_put([("app/a", "1"), ("app/b", "2")]).await?;
let page = kv.scan("app/", "app0", 100).await?;             // [start, end), at most 10240
kv.delete_range("app/", "app0").await?;

let atomic = kv.with_atomic_for_cas();                      // see below
atomic.compare_and_swap("app/counter", Some(b"1".to_vec()), "2").await?;
```

- **Operations:** `get`, `batch_get`, `put`, `batch_put`, `delete`, `batch_delete`,
  `delete_range`, `scan`/`scan_keys`, `compare_and_swap`, `put_with_ttl`/`batch_put_with_ttl`
  and `get_key_ttl_secs`, `split_regions`, and `with_cf` for the `lock` and `write` column
  families.
- **Routing:** each request goes to the leader of the region that holds its key. Regions and
  stores come from PD and are cached. `NotLeader`, `EpochNotMatch` (splits and merges),
  `RegionNotFound`, `ServerIsBusy` and unreachable stores refresh the cache and retry, for up
  to `Config::retry_deadline` (20 s). Batches are grouped by region and sent concurrently, at
  most 2048 keys or 2 MiB per request. Scans and range deletes walk the regions in order.
- **Not transactional:** a batch over several regions is several writes, and some can land
  while others fail.
- **CAS:** TiKV keeps a compare-and-swap atomic only against writes that are also marked "for
  CAS". Every client that writes those keys must use `with_atomic_for_cas()`, which marks all
  its puts and deletes. `delete_range` isn't available in that mode.
- **TTL** needs `storage.enable-ttl = true` (API V1TTL), which kvproto documents as RawKV only:
  not for a cluster that also runs TiDB. A plain V1 TiKV rejects TTL requests.
- **Next to TiDB** (API V1, the default): raw keys share the key space with TiDB's data, whose
  keys start with `t` and `m`. Keep raw keys under a prefix of your own, and never scan or
  delete an unbounded range there.
- **Not supported yet:** TLS to PD/TiKV, API V2 keyspaces, reverse scans (see
  [TODO.md](../TODO.md)).

A local TiKV without TiDB: `tiup playground --mode tikv-slim` (PD on `127.0.0.1:2379`).
