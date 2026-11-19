# Reference

What tisqlx accepts and what it does, by area. For how to use it, see the
[how-to guides](how-to.md); for the reasons, the [explanation](explanation.md).

[Queries](#queries) · [Connection URL](#connection-url) · [Authentication and TLS](#authentication-and-tls) ·
[Pool options](#pool-options) · [Statements](#statements) · [Query macros](#query-macros) ·
[Derives](#derives) · [Type mapping](#type-mapping) · [Time zones](#time-zones) ·
[TiDB primitives](#tidb-primitives) · [Errors](#errors) · [CLI](#cli) · [Crates](#crates)

## Queries

| to get | call |
|---|---|
| the first row (an error if there is none) | `fetch_one` |
| the first row, if there is one | `fetch_optional` |
| all rows in a `Vec` | `fetch_all` |
| rows one by one, as a stream | `fetch` |
| no rows, only `rows_affected()` and `last_insert_id()` | `execute` |

| function | each row becomes |
|---|---|
| `query` | a `Row`: `let name: String = row.get("name");` |
| `query_as` | a tuple, or a struct that implements `FromRow` |
| `query_scalar` | its first column |

| other constructor | |
|---|---|
| `raw_sql(sql)` | the text protocol: no bind parameters, but several `;`-separated statements, and statements that can't be prepared |
| `query_with(sql, arguments)` | `query` with arguments built beforehand (`Arguments::add`) |

- **`.bind(value)`** binds the next `?`. A value that fails to encode is reported when the
  query runs.
- **`.persistent(false)`** prepares the statement for this run only, instead of caching it on
  the connection.
- **`.try_map(|row| …)`** on a `query` converts each row with a closure.
- **`fetch_many`** on a `query` streams every result set of a multi-statement `raw_sql`, as
  `Step::Row` and `Step::Done`.
- **Executors:** every `fetch*` and `execute` takes `&pool`, `&mut conn` (a `TiConnection` or a
  `PoolConnection`) or `&mut *tx`.
- **`Row`:** `get(index)` panics on a missing column or a type mismatch, `try_get(index)`
  returns the error. The index is a position or a column name.

## Connection URL

`tidb://user:pass@host:4000/db?ssl-mode=verify-identity&txn-mode=optimistic&resource-group=rg1`
(`mysql://` is accepted too; the user defaults to `root`, the host to `localhost`, the port to 4000).

| query key | default | meaning |
|---|---|---|
| `ssl-mode` | `preferred` | `disabled`, `preferred`, `required`, `verify-ca` or `verify-identity` |
| `ssl-ca` | webpki roots | PEM file of CA certificates for the two `verify-*` modes |
| `socket` | | path of a Unix socket to use instead of TCP |
| `connect-timeout` | `30` | seconds, at least 1 |
| `timezone` | `+00:00` | session `time_zone`; `server` keeps the server's (see [Time zones](#time-zones)) |
| `txn-mode` | server's | `pessimistic` or `optimistic` |
| `resource-group` | | TiDB resource group of the session |
| `statement-cache-capacity` | `100` | prepared statements cached per connection; `0` disables the cache |
| `require-tidb` | `true` | `false` also accepts TiDB older than 8.5 and servers that aren't TiDB |
| `load-balance-dns` | `false` | balance over the addresses of one DNS name (see [Connect to several TiDB servers](how-to.md#connect-to-several-tidb-servers)) |

Any other key is set as a session variable, e.g. `?tidb_isolation_read_engines=tikv,tiflash`.
Its name may only contain letters, digits, `_` and `.`.

The user, password, database, keys and values are percent-decoded, and must then be valid
UTF-8. A `+` is a literal plus (`timezone=+08:00`), not a space.

## Authentication and TLS

`mysql_native_password` works on every transport. TiDB has no RSA key exchange, so a
`caching_sha2_password` account needs TLS or a Unix socket; tisqlx reports this clearly instead
of failing with "access denied". The default `ssl-mode=preferred` uses TLS whenever the server
offers it. `preferred` and `required` encrypt without verifying the certificate, `verify-ca`
verifies its chain, and `verify-identity` also its host name. Use `verify-identity` for TiDB Cloud.

## Pool options

`PoolOptions` and their defaults:

| option | default | meaning |
|---|---|---|
| `max_connections` | 10 | at least 1 |
| `min_connections` | 0 | connections kept open; capped at `max_connections` |
| `acquire_timeout` | 30 s | how long `acquire()` waits for a free connection |
| `idle_timeout` | 10 min | closes connections idle for this long, down to `min_connections` |
| `max_lifetime` | 30 min | closes a connection this long after it was opened, once it is no longer in use |
| `test_before_acquire` | `true` | ping an idle connection before handing it out |
| `ping_after_idle` | 30 s | only ping a connection that sat idle at least this long |
| `after_connect` | | callback run on every new connection |

**Checkouts** ping a connection only if it sat idle for at least `ping_after_idle`. A
connection used moments ago is handed out as is. If the server dropped it in the meantime (a
restart or a `KILL`), that one query fails and the pool discards the connection.
`ping_after_idle(Duration::ZERO)` pings on every checkout, and `test_before_acquire(false)`
never pings.

Reading the pool: `size()` (open connections), `num_idle()`, `host_connections()`,
`is_closed()`. `close().await` closes the idle connections and refuses new checkouts;
connections in use are closed when returned. A `PoolConnection` goes back to the pool when
dropped; `detach()` keeps it out for good.

## Statements

**`rows_affected()`** counts the rows a statement matched, not the rows it changed (tisqlx sets
`CLIENT_FOUND_ROWS`), so an `UPDATE` that leaves a row as it was still counts it.

**The statement cache** keeps up to `statement-cache-capacity` prepared statements per
connection. It survives DDL: TiDB reports the schema change (error 8113), and the statement
is prepared again, once. TiDB binds a prepared statement to the database that was current
when it was prepared, so a statement that starts with `USE` empties the cache.

## Query macros

Set `DATABASE_URL` in the environment or in a `.env` file. The macros read the nearest
`.env`, starting from the crate's directory and going up; the environment wins over it.
Without `DATABASE_URL`, or with `TISQLX_OFFLINE=true`, they use the offline data instead (see
[Build without a database](how-to.md#build-without-a-database)).

Nullability comes from TiDB's column metadata: TiDB keeps `NOT NULL` accurately,
and drops it for the nullable side of outer joins and for `MAX`/`MIN`/`SUM`/`AVG`. Override it
in the column alias:

| alias | meaning |
|---|---|
| `` AS `col!` `` | force non-null |
| `` AS `col?` `` | force `Option` |
| `` AS `col: MyType` `` | decode as `MyType` (any `Decode` type, e.g. a `#[derive(Type)]` enum); `` `col!: MyType` `` and `` `col?: MyType` `` combine both |
| `` AS `col: _` `` | infer from the struct field (`query_as!` only) |

**Bind arguments** are checked by count, and by type where tisqlx can tell what a `?` stands for.
TiDB reports every parameter as a string, so the macros read it from the SQL instead: the column
a `?` is compared with or assigned to. They then ask the server for that column's type, by
preparing (never running) `SELECT <those columns> FROM <the query's FROM>`.

- **Recognised:** `col = ?` and the other comparisons, `[NOT] LIKE`, `IN (?, …)`,
  `BETWEEN ? AND ?`, `SET col = ?`, `INSERT … (cols) VALUES (?, …)` and `LIMIT`/`OFFSET ?`.
  A `LIKE` pattern is text whatever the column, so it is checked only against text and binary
  columns.
- **Not recognised:** anything else, such as a function argument, a subquery or the select
  list, keeps only the `Encode` bound. A statement with an executable comment (`/*! … */`)
  is not analysed at all.
- **Skipping the check:** `value as _` skips it for one argument; the other ways are in the how-to linked below.
- **The check is by kind, and strict on purpose.** TiDB accepts almost any bound type and
  converts it at runtime. Measured on 8.5.8:
  - in a comparison, a mismatched value never errors: `WHERE id = 'abc'` or
    `WHERE created_at > 5` silently matches nothing, with only a warning;
  - in a write, the outcome depends on the value: `'5'` into a BIGINT works, `'abc'` fails.

  So the check rejects any argument whose kind differs from the column's, before the value
  can do either.
- **Types tisqlx can't classify** are never rejected. That covers hand-written `Encode` impls,
  generic arguments, and derived newtypes around such types.
- **Offline data:** the inferred types are stored in `.tisqlx/`, so offline builds check the same
  thing.

Arguments accepted for each column type. `&T`, `Option<T>`, `Box<T>`, `Arc<T>` and
`#[derive(Type)]` newtypes count as their inner `T`; a derived enum counts as text.

| Column next to the `?` | Accepted arguments |
|---|---|
| `TINYINT`…`BIGINT` (incl. `UNSIGNED`), `BOOLEAN`, `DECIMAL`, `FLOAT`, `DOUBLE`, `BIT`, `YEAR`, and `LIMIT ?`/`OFFSET ?` | `i8`…`i128`, `u8`…`u128`, `f32`, `f64`, `bool`, `Decimal`, `BigDecimal` |
| `CHAR`, `VARCHAR`, `TEXT`, `ENUM`, `SET`, `VECTOR` | `&str`, `String`, `Cow<str>`, `types::Set`, `types::Vector`, `uuid::fmt::Hyphenated`, derived enums, and JSON values (`serde_json::Value`, `Json<T>`, `&RawValue`) |
| `BINARY`, `VARBINARY`, `BLOB` | `&[u8]`, `Vec<u8>`, `[u8; N]`, `Cow<[u8]>`, `uuid::Uuid`, and everything accepted for text |
| `JSON` | `serde_json::Value`, `Json<T>`, `&RawValue`, and text (it must be valid JSON) |
| `DATE` | `chrono::NaiveDate`, `time::Date`, and the datetime types |
| `DATETIME`, `TIMESTAMP` | `NaiveDateTime`, `DateTime<Utc \| FixedOffset \| Local>`, `time::PrimitiveDateTime`, `time::OffsetDateTime`, and the date types |
| `TIME` | `chrono::TimeDelta`, `NaiveTime`, `time::Time`, `time::Duration`, `std::time::Duration` |
| `GEOMETRY`, or a position the macros don't analyse | anything that implements `Encode` |

When an argument is rejected, see
[Pass a value whose type differs from the column](how-to.md#pass-a-value-whose-type-differs-from-the-column).

## Derives

```rust
#[derive(tisqlx::FromRow)]
#[tisqlx(rename_all = "camelCase")]
struct User {
    id: i64,                              // column `id`
    created_at: NaiveDateTime,            // column `createdAt`
    #[tisqlx(rename = "mail")]
    email: Option<String>,
    #[tisqlx(json)]
    settings: Settings,                   // a JSON column, through serde
}

#[derive(tisqlx::Type)]
#[tisqlx(rename_all = "snake_case")]
enum Status { Active, InProgress }        // 'active', 'in_progress'

#[derive(tisqlx::Type)]
struct UserId(i64);                       // encodes and decodes as its field
```

- **`FromRow`** is for structs. Named fields are read by column name, tuple fields by
  position. Field attributes: `rename = "col"`, `default` (the type's default when the column
  is absent), `json`, `try_from = "Type"` (decode as `Type`, then `TryFrom`), `flatten` (a
  nested `FromRow`) and `skip` (always the default).
- **`Type`** is for a struct with one unnamed field, or an enum without fields, which is
  stored as text (`VARCHAR` or `ENUM`). Variants take `rename = "value"`.
- **`rename_all`** takes `lowercase`, `UPPERCASE`, `snake_case`, `SCREAMING_SNAKE_CASE`,
  `kebab-case`, `camelCase` or `PascalCase`. An acronym is one word: `HTTPServer` becomes
  `http_server`. It applies to `FromRow` structs with named fields and to `Type` enums.
- **Compile errors:** an unknown attribute; `rename_all` anywhere else; `rename` on a tuple field or next to `skip` or
  `flatten`; more than one of `skip`, `flatten`, `try_from` and `json`; `default` next to
  `skip`, `flatten` or `try_from`; `Type` on a generic enum.

## Type mapping

| TiDB | Rust | feature |
|---|---|---|
| `BOOLEAN`/`TINYINT(1)`, `BIT(1)` | `bool` | |
| `TINYINT`..`BIGINT` (`UNSIGNED`) | `i8`..`i64` (`u8`..`u64`), `i128`/`u128`; `Tso` for a `BIGINT UNSIGNED` | |
| `BIT(n)`, `YEAR` | `u64` (or `Vec<u8>`), `u16` | |
| `FLOAT`, `DOUBLE` | `f32`, `f64` | |
| `DECIMAL` | `Decimal` (up to ~28 significant digits; a wider value is an error, not rounded), `BigDecimal` (any precision, up to TiDB's 65), `i128`/`u128` (integral values), `f32`/`f64` (rounded) | `decimal`, `bigdecimal` |
| `CHAR`, `VARCHAR`, `TEXT` | `String`, `&str`, `Cow<str>`, `Box<str>`, `Arc<str>` | |
| `BINARY`, `VARBINARY`, `BLOB` | `Vec<u8>`, `&[u8]`, `[u8; N]`, `Box<[u8]>`, `Arc<[u8]>` | |
| `ENUM` | `String`, or a `#[derive(Type)]` enum | |
| `SET` | `types::Set` (list of members), `String` | |
| `JSON` | `serde_json::Value`, `Json<T>`, `&RawValue` | |
| `VECTOR` | `types::Vector` (`Vec<f32>`) | |
| `DATE` | `chrono::NaiveDate`, `time::Date` | `chrono`, `time` |
| `DATETIME` | `chrono::NaiveDateTime`, `time::PrimitiveDateTime` | `chrono`, `time` |
| `TIMESTAMP` (or `DATETIME`) as an instant | `chrono::DateTime<Utc>`, `DateTime<FixedOffset>`, `DateTime<Local>`, `time::OffsetDateTime` | `chrono`, `time` |
| `TIME` | `chrono::TimeDelta`, `time::Duration`, `std::time::Duration` (≥ 0); `NaiveTime`/`time::Time` within a day | `chrono`, `time` |
| `BINARY(16)`, `CHAR(36)` | `uuid::Uuid` (bytes or text), `uuid::fmt::Hyphenated` (text) | `uuid` |

The query macros infer chrono types when `chrono` is on, else `time`. They infer `Decimal`, or
`BigDecimal` when `bigdecimal` is on and the column is wider than 28 digits (or `decimal` is
off). `DECIMAL(28, s)` with a scale counts as wider: TiDB reports the same width for it as for
29 digits. `SET` columns become `types::Set`. UUID and VECTOR columns look like binary and text
columns to the macros, so they need an override such as `` AS `id: Uuid` ``.

**Decoding is exact.** A value the Rust type can't hold is an error, never rounded or cut: a
`DECIMAL` too wide for `Decimal`, a fractional `DECIMAL` read as an integer, a `DOUBLE` out
of `f32` range, or a `DATETIME` with a time of day read as a date type (use a datetime type
or `DATE(col)`). Two exceptions: asking for a float (`f32`/`f64` round a `DECIMAL`, and
`f32` rounds a `DOUBLE` that is within its range), and `bool`, which is `true` for any
non-zero integer. The zero date `0000-00-00` (non-strict
`sql_mode`) is an error for every date and datetime type, `Option<_>` included.

## Time zones

TiDB sends and receives DATETIME and TIMESTAMP values as wall times in the
session `time_zone`. Naive types (`NaiveDateTime`, `PrimitiveDateTime`) are those wall times
as they are. Instant types (`DateTime<…>`, `OffsetDateTime`) are converted from and to the
session zone on the client, using the system time zone database:
- **Default (`timezone=+00:00`):** a UTC session, so no conversion and no loss.
- **`timezone=Europe/Paris`, `timezone=server`, …:** values are converted, and decoded values
  carry the session offset (`FixedOffset`, `OffsetDateTime`). Around daylight-saving changes
  a wall time is ambiguous or doesn't exist. TiDB itself then stores a TIMESTAMP one hour off,
  and tisqlx decodes exactly as TiDB interprets it. Keep the UTC default to store instants
  losslessly.
- **The zone is read when the connection opens,** and again after a pool's `after_connect` hook.
  TiDB doesn't report later changes, so after a raw `SET time_zone` call
  `TiConnection::reload_time_zone()`.
- **A zone missing from the client's database** (e.g. a container without tzdata) makes
  instant types fail with an error. Naive types keep working.

## CLI

```sh
cargo install --path tisqlx-cli
tisqlx database setup                 # create the database and run the migrations
tisqlx migrate add -r create_users    # a reversible migration (.up.sql and .down.sql)
tisqlx migrate run --dry-run          # list what would be applied
tisqlx migrate repair 20260101000000 --applied   # after fixing a dirty migration by hand
tisqlx prepare --check                # fail if the offline query data is stale
```

| command | does | flags |
|---|---|---|
| `database create` | `CREATE DATABASE IF NOT EXISTS` | |
| `database drop` | `DROP DATABASE IF EXISTS`, after asking | `-y` |
| `database setup` | creates the database and runs the migrations | |
| `database reset` | drops, creates and migrates, after asking. It reads the migrations before it drops anything. | `-y` |
| `migrate add <description>` | creates a migration file. The version style and the reversibility follow the existing files. | `-r`/`--reversible`, `-s`/`--simple`, `--sequential`, `-t`/`--timestamp` |
| `migrate run` | applies the pending migrations | `--dry-run`, `--target-version <v>`, `--ignore-missing` |
| `migrate revert` | reverts the latest migration | `--dry-run`, `--target-version <v>` (everything newer), `--all`, `--ignore-missing` |
| `migrate info` | lists every migration and its status | |
| `migrate repair <version>` | clears a dirty migration after a fix by hand | `--applied` (mark it applied) or `--remove` (run it again next time) |
| `migrate build-script` | writes a `build.rs`, so `migrate!()` picks up new files | `--force` |
| `prepare` | writes the offline data of the query macros to `.tisqlx/` | `--check`, `-- <cargo check args>` |

- **The database URL:** every command that connects takes `-D <url>` or reads
  `DATABASE_URL`, from the environment or from the nearest `.env` file. The CLI looks for
  `.env` from the current directory upwards; the macros look from the crate's directory.
- **Migrations** are read from `./migrations` unless `--source <dir>` says otherwise.
- **`prepare`** runs `cargo check --workspace --all-targets`. Arguments after `--` go to
  `cargo check`: with `-p`/`--package` or `--exclude` only those packages are checked, and
  the other packages' files in `.tisqlx/` are kept. `--manifest-path` selects another
  workspace.

## TiDB primitives

| call | gives or does |
|---|---|
| `conn.tidb_version()` | the server's `TiDbVersion` (`None` if it isn't TiDB), comparable: `v >= TiDbVersion::new(8, 5, 0)` |
| `conn.current_tso()` | the start `Tso` of the current transaction, usable later in `StaleRead::Tso` |
| `conn.set_read_staleness(Some(duration))` | every read of the session becomes a stale read (`tidb_read_staleness`, whole seconds) |
| `conn.set_isolation_read_engines(&[Engine::TiFlash])` | the engines the optimizer may read from |
| `done.last_insert_id()` | the id generated by `AUTO_INCREMENT` or `AUTO_RANDOM` |
| `?resource-group=rg1` in the URL | the resource group of every session |
| `err.is_retryable()`, `DatabaseError::is_write_conflict()`, `is_unique_violation()`, … | what a server error means |

`RetryPolicy` has three public fields: `max_attempts` (5), `initial_backoff` (10 ms) and
`max_backoff` (1 s). `TxnOptions` is `ReadWrite { mode, isolation }` (the default, with both `None`: the session's),
`StaleRead(…)` or `CausalConsistencyOnly`; `pessimistic()`, `optimistic()` and `stale_read(…)`
build the common ones.

## Errors

`tisqlx::Error` is non-exhaustive. The variants an application usually tells apart:

| variant | when |
|---|---|
| `Database(DatabaseError)` | the server returned an error: `code`, `sql_state`, `message` |
| `RowNotFound` | `fetch_one` found no row |
| `PoolTimedOut` | no connection became free within `acquire_timeout` |
| `PoolClosed` | the pool was closed |
| `Configuration` | a bad URL or option |
| `Io`, `Tls`, `Protocol` | the connection failed or was misused; a failed one is not reused |
| `ColumnDecode`, `ColumnNotFound`, `Encode` | a value and its Rust type don't fit |
| `NotTiDb`, `UnsupportedTiDb` | the server is not TiDB 8.5 or newer |
| `Migrate(MigrateError)` | a migration failed: `Dirty`, `VersionMismatch`, `LockTimeout`, … |

| test | true for |
|---|---|
| `Error::is_retryable()` | write conflicts, deadlocks, lock wait timeouts, a schema change during the transaction: run the whole transaction again |
| `error::is_retryable(&dyn Error)` | the same, anywhere in an error's `source()` chain (for `anyhow::Error`) |
| `DatabaseError::is_unique_violation()` | error 1062 |
| `DatabaseError::is_foreign_key_violation()` | errors 1451 and 1452 |
| `DatabaseError::is_check_violation()` | error 3819 |
| `DatabaseError::is_write_conflict()` | errors 9007, 8002 and 8005 |

`Error::as_database_error()` gives the `DatabaseError`; `error::codes` names the codes.

## Crates

| crate | role |
|---|---|
| `tisqlx` | facade: re-exports and macros. Features: `macros`, `chrono`, `decimal` (the defaults), `time`, `bigdecimal`, `uuid` |
| `tisqlx-core` | driver: wire protocol, auth, TLS, types, pool, transactions, TiDB features, migrations |
| `tisqlx-macros` | `query!` family, `migrate!`, `#[derive(FromRow, Type)]`, `#[tisqlx::test]` (proc-macro crate, used through `tisqlx`) |
| `tisqlx-cli` | `tisqlx` binary: `database`, `migrate`, `prepare` |
