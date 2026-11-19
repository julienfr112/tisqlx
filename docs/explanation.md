# Explanation: why tisqlx works this way

Background for the choices in tisqlx. Nothing here is needed to follow a recipe; it is for
when you wonder why something behaves as it does, or whether tisqlx fits your case.

[Why only TiDB](#why-only-tidb) · [Migrations](#migrations) ·
[Transactions and retries](#transactions-and-retries) ·
[What the query macros can know](#what-the-query-macros-can-know) ·
[Why the bind check is strict](#why-the-bind-check-is-strict) ·
[Connections and cancellation](#connections-and-cancellation) · [The pool](#the-pool) ·
[Time](#time) · [Exact decoding](#exact-decoding)

## Why only TiDB

TiDB speaks the MySQL protocol, and a MySQL driver connects to it. But it is a distributed
database, and it differs from MySQL exactly where an application gets hurt: what a transaction
guarantees, when it fails, what DDL does, and what the server reports about a query.

A driver for several databases has to hide those differences behind the common denominator.
tisqlx takes the other side of the trade: it supports one server, on one runtime (tokio) with
one TLS stack (rustls), and spends the saved abstraction on handling TiDB as it is. Some
examples, each detailed below or in the [reference](reference.md):

- DDL is never transactional, so migrations have a *dirty* state instead of a rollback.
- A correct transaction can be refused (write conflict, deadlock), so retrying the whole
  transaction is built in.
- TiDB reports every bind parameter as a string, so the macros work out parameter types from
  the SQL.
- A prepared statement stays bound to the database that was current when it was prepared, and
  fails with error 8113 after a schema change, so the statement cache watches for both.
- TiDB has no RSA key exchange, so a `caching_sha2_password` account needs TLS or a Unix
  socket, and tisqlx says so instead of failing with "access denied".

The price is plain: tisqlx is of no use with MySQL or any other server, and by default refuses
to connect to one. TiDB 8.5 is the oldest version it is tested against.

It is written from scratch, without `unsafe`, after the model of [sqlx]: the same shape of
API (`query`, `query_as`, `query!`, `FromRow`, `Pool`, `migrate!`), so sqlx code and habits
carry over.

## Migrations

In most SQL databases a migration runs in a transaction: it applies fully or not at all. In
TiDB every DDL statement commits on its own, at once, and commits whatever transaction was
open. A migration of three `ALTER TABLE` statements that fails on the second has changed the
schema, and nothing can undo that automatically.

tisqlx doesn't pretend otherwise. Before it runs a migration, it records it in the
`_tisqlx_migrations` table as *dirty*; after the last statement succeeds, it marks it
successful. A failure leaves the record dirty, and every later run refuses to continue. Only a
person can tell what state the schema is in, so only a person clears it: finish the work and
`repair --applied`, or undo it and `repair --remove`
([how](how-to.md#repair-a-failed-migration)).

Around that, the migrator defends its bookkeeping against the migration's own SQL:

- **One runner at a time.** A run holds a named lock on the server (`GET_LOCK`), one per
  database, so several instances of an application starting together don't apply the same
  migration twice.
- **Applied files don't change.** The checksum of each file is recorded; a later run fails if
  an applied file was edited.
- **The session is put back.** After each migration the connection returns to its original
  database, so a `USE` in one file reaches neither the next file nor the bookkeeping. A
  migration that leaves a transaction open fails, and that transaction is rolled back, rather
  than letting the bookkeeping commit it.

It follows that a migration is best kept small: one DDL statement per file fails cleanly, with
nothing half-done.

## Transactions and retries

TiDB has two transaction modes, and they fail differently.

- **Optimistic:** nothing is locked while the transaction runs. At commit, TiDB checks that no
  one else wrote the same rows; if someone did, the commit fails with a write conflict.
- **Pessimistic** (the server's default): rows are locked as they are written, as in MySQL.
  Conflicts then show up as waits, and sometimes as a deadlock or a lock wait timeout.

Either way, a transaction that did nothing wrong can be refused, and the remedy is the same:
run it again from the start. Retrying only the failed statement is not enough, because the
earlier reads may no longer hold. This is why `Pool::transaction` takes a closure and not a
sequence of calls: tisqlx must be able to run the whole thing again. It rolls back, waits, and
calls the closure with a new transaction.

Two consequences for the closure. It may run several times, so anything other than SQL in it
(an email, a counter, a request to another service) happens several times. And it borrows the
transaction for the length of each attempt, which is why it returns a boxed future and owns
its inputs.

The wait between attempts is random, up to a bound that doubles each time. Transactions that
collided once would collide again if they all came back after the same delay.

**Dropped transactions.** A `Transaction` dropped without `commit()` is rolled back, so `?`
is safe. Dropping can't run SQL, though. A pooled connection is rolled back in the background
before it goes back to the pool, promptly, because an open pessimistic transaction holds its
locks. A connection you borrowed is rolled back before its next command.

**Nesting.** A transaction inside a transaction is a savepoint. Rolling it back undoes its own
work only; a nested level dropped without commit is rolled back to its savepoint the same way.

**DDL.** A DDL statement inside a transaction commits it, with everything before it. The
server tells no one. tisqlx notices at the next nested `commit()` or `rollback()`, whose
savepoint is gone, and reports it; at the top level there is nothing left to notice with
([TODO.md](../TODO.md) tracks this).

**Stale reads.** A stale read is a read-only transaction at a timestamp in the past. It takes
no locks, and since it doesn't need the newest data, any replica can serve it, not only the
leader of each region. It is the cheap way to take read traffic off the leaders when a few
seconds of delay are acceptable.

## What the query macros can know

`query!` doesn't parse SQL to check it. At compile time it connects to the database of
`DATABASE_URL` and *prepares* the statement, never running it. TiDB answers as it would at run
time: an error for a bad query, else the list of result columns with their types. The macro
turns those into a struct, so a typo is a compile error with TiDB's own message, and the check
can't disagree with the server.

**Nullability** comes from the same answer. TiDB reports `NOT NULL` accurately for table
columns, and drops it where a value can become `NULL` on the way: the outer side of a join,
or `MAX` over no rows. For an expression TiDB decides, sometimes cautiously; where it says
nullable and you know better, the `!` override is your assertion.

**Parameters** are the weak point. The protocol has a place for the type of each `?`, but
TiDB reports all of them as strings. So tisqlx reads the SQL itself, with a small analyzer that finds the
column next to a `?` in the common positions (`col = ?`, `SET col = ?`, `INSERT … VALUES`,
`IN`, `BETWEEN`, `LIKE`, `LIMIT`), and asks TiDB for that column's type. It is not a full SQL
parser: where it doesn't recognise the position, it checks nothing rather than guess.

**Offline data.** A build that needs a live database is a problem in CI. `tisqlx prepare`
saves what TiDB answered for each query in `.tisqlx/`, one JSON file named after the hash of
the SQL text. Committed with the code, they let the macros do the same checks without a
server. They can go stale when the schema changes, which `prepare --check` detects.

## Why the bind check is strict

Where the analyzer knows the column, an argument of another kind is a compile error: text for
a `BIGINT`, a number for a `DATETIME`. This is stricter than TiDB, which accepts nearly any
value for any parameter and converts it when the query runs. The trouble is what the
conversion does, measured on TiDB 8.5.8:

- In a comparison, a value that doesn't convert is not an error. `WHERE id = 'abc'` or
  `WHERE created_at > 5` matches nothing and returns an empty result, with only a warning
  that nobody reads.
- In a write, it depends on the value: `'5'` goes into a `BIGINT`, `'abc'` fails.

Both are bugs that pass every test written with well-formed values, and appear with the first
malformed one. A compile-time check that allowed them would give false confidence. So the
check is by kind and has no implicit conversions, and the conversion you do want is written
out, in Rust or in SQL, where it can fail visibly
([how](how-to.md#pass-a-value-whose-type-differs-from-the-column)).

The check is deliberately quiet where it can't be sure: a type it can't classify (your own
`Encode` implementation, a generic) is never rejected.

## Connections and cancellation

In async Rust any `await` can be the last: a timeout fires, a client disconnects, and the
future is dropped half-way. For a database connection that is dangerous, because the protocol
is a strict conversation, and a connection left in the middle of one would hand the next
query the previous query's rows.

Every connection therefore tracks where it is in the conversation:

- **Dropped while reading a result** (a stream not read to the end): the rest of the response
  is read and thrown away before the next command.
- **Dropped in the middle of a packet**, or after an I/O or protocol error: the position is
  unknown. The connection is marked broken and never used again; the pool discards it.

**The statement cache.** Queries with parameters are prepared once per connection and reused
(100 statements by default, the least recently used evicted). Two TiDB behaviours make that
less simple than it sounds. After a schema change, executing an old statement fails with
error 8113: tisqlx prepares it again and retries, once and only if no row was delivered yet.
And a statement keeps reading the database that was current when it was prepared, so a `USE`
empties the cache.

**`rows_affected()`** counts matched rows, not changed ones. MySQL's default would report 0
for an `UPDATE` that sets a row to the values it already has, which makes "was there such a
row?" unanswerable. tisqlx asks the server for matched rows (`CLIENT_FOUND_ROWS`).

## The pool

The pool does three jobs: bound the number of connections, hand out healthy ones, and spread
them over the TiDB servers.

- **Healthy connections.** A connection that sat idle may have been cut by a load balancer or
  a restarted server. Pinging on every checkout would catch that, at a round trip per query.
  The pool pings only connections idle for more than `ping_after_idle` (30 s): one used a
  moment ago is almost certainly alive. The cost is that a server restart can fail one query
  per recently used connection.
- **Clean connections.** A connection returned with an open transaction, an unread result or
  a held lock is reset in the background before anyone else gets it.
- **Reuse order.** The most recently used idle connection goes out first. Under low load a
  few connections do all the work and the others age until `idle_timeout` closes them, so the
  pool shrinks by itself.
- **Several servers.** TiDB servers are stateless and interchangeable, so the usual setup is
  several of them behind a load balancer. With the servers listed in the URL, the pool does
  that job itself: each new connection goes to the server that has the fewest, and a server
  that fails to connect is tried last for a while. Existing connections are never moved; a
  recovered server fills up as connections are replaced at `max_lifetime`.

## Time

TiDB sends and receives `DATETIME` and `TIMESTAMP` values as wall-clock times in the
session's time zone, without saying which zone that is. A naive Rust type (`NaiveDateTime`)
is that wall time, as is. An instant type (`DateTime<Utc>`) needs a conversion, and the
conversion needs the zone.

tisqlx sets the session to UTC unless told otherwise. Then there is nothing to convert, no
daylight-saving ambiguity, and every instant round-trips exactly. With another session zone
it converts on the client, using the system's time zone database, and follows TiDB's own
rules for the wall times that occur twice or not at all around a clock change, so that what
it decodes is the instant TiDB stored.

## Exact decoding

Reading a value into a Rust type that can't hold it is an error: a `DECIMAL` with more digits
than `Decimal` has, a fractional value into an integer, a `DATETIME` with a time of day into
a date. The alternative, rounding or cutting silently, turns a schema mismatch into wrong
numbers that look right. The exception is a float, where rounding is what the type means.

[sqlx]: https://github.com/launchbadge/sqlx
