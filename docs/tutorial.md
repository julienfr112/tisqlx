# Tutorial: a first program with tisqlx

You will write a small program that stores users in TiDB: connect, create a table, insert and
read rows, map them to a struct, have the compiler check the SQL, and move the schema into
migrations. It takes about twenty minutes. Each step ends with something you can run.

You need Rust 1.88 or newer and [tiup](https://docs.pingcap.com/tidb/stable/tiup-overview).

## 1. Start a TiDB and a project

In one terminal, start a local TiDB and leave it running. It listens on `127.0.0.1:4000`, with
the user `root` and no password.

```sh
tiup playground v8.5.8
```

In another, create the project:

```sh
cargo new users && cd users
```

```toml
# Cargo.toml
[dependencies]
tisqlx = { git = "https://github.com/julienfr112/tisqlx" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

## 2. Connect and run a query

A `Pool` holds the connections. It is cheap to clone, and it is what you pass around. Values
go in with `?` and `bind`, never formatted into the SQL.

```rust
// src/main.rs
use tisqlx::Pool;

#[tokio::main]
async fn main() -> tisqlx::Result<()> {
    let pool = Pool::connect("tidb://root@127.0.0.1:4000/test").await?;

    tisqlx::query(
        "CREATE TABLE IF NOT EXISTS users (
            id BIGINT PRIMARY KEY AUTO_RANDOM,
            name VARCHAR(100) NOT NULL,
            email VARCHAR(255)
        )",
    )
    .execute(&pool)
    .await?;

    // A write: `execute` returns the affected rows and the generated id.
    let done = tisqlx::query("INSERT INTO users (name, email) VALUES (?, ?)")
        .bind("alice")
        .bind("alice@example.com")
        .execute(&pool)
        .await?;
    let id = done.last_insert_id();

    // A read, into a tuple.
    let (name, email): (String, Option<String>) =
        tisqlx::query_as("SELECT name, email FROM users WHERE id = ?").bind(id).fetch_one(&pool).await?;
    println!("{name} <{email:?}>");
    Ok(())
}
```

```sh
cargo run
# alice <Some("alice@example.com")>
```

Two things to notice. `email` is an `Option<String>` because the column may be `NULL`. And the
id is a large, scattered number: `AUTO_RANDOM` is TiDB's way to spread inserts over the
cluster instead of piling them on one node.

`fetch_one` wants a row and fails without one. Its siblings are `fetch_optional`, `fetch_all`
and `fetch` (a stream); the [reference](reference.md#queries) lists them.

## 3. Map rows to a struct

Tuples get unwieldy. Derive `FromRow`, and `query_as` fills the struct by column name. Add
this above `main`, and replace the read:

```rust
#[derive(Debug, tisqlx::FromRow)]
struct User {
    id: i64,
    name: String,
    email: Option<String>,
}
```

```rust
    let users: Vec<User> = tisqlx::query_as("SELECT id, name, email FROM users").fetch_all(&pool).await?;
    println!("{users:?}");
```

Run it again: there is one more user each time.

## 4. Let the compiler check the SQL

Change `name` to `nam` in the `SELECT` and run: the program compiles, then fails at run time
with "Unknown column". The functions check nothing before they run.

The macros of the same names do. While compiling, they ask TiDB about the query. Tell them
where the database is, in a `.env` file next to `Cargo.toml`:

```sh
DATABASE_URL=tidb://root@127.0.0.1:4000/test
```

Then replace the read with:

```rust
    let users = tisqlx::query_as!(User, "SELECT id, nam, email FROM users").fetch_all(&pool).await?;
```

`cargo build` now stops on the typo, with TiDB's own message. Fix it (`name`) and it builds.
The macro also checked that each column fits its field: declare `email: String` and the build
fails again, because the column is nullable. `User` no longer needs `FromRow` for this.

`query!` needs no struct at all, and checks the arguments too:

```rust
    let user = tisqlx::query!("SELECT name, email FROM users WHERE id = ?", id).fetch_one(&pool).await?;
    println!("{} <{:?}>", user.name, user.email);
```

Pass `"7"` instead of `id` and it does not compile: the column is a `BIGINT`, the argument is
text.

## 5. Move the schema into migrations

A `CREATE TABLE IF NOT EXISTS` in `main` can't evolve a table that already exists. Keep the
schema in versioned SQL files instead. Install the command-line tool, from a checkout of the
tisqlx repository:

```sh
cargo install --path tisqlx-cli
```

Back in the project, create the first migration:

```sh
tisqlx migrate add create_users       # creates migrations/<timestamp>_create_users.sql
```

Put the `CREATE TABLE users (…)` statement of step 2 in that file, without `IF NOT EXISTS`. The
table already exists, so start again from an empty database: `reset` drops it, creates it and
applies the migrations. The tool reads `DATABASE_URL` from `.env`, like the macros.

```sh
tisqlx database reset -y
tisqlx migrate info
# VERSION         STATUS   TYPE    TIME  DESCRIPTION
# 20261119142739  applied  simple  12ms  create users
```

In `main`, replace the `CREATE TABLE` query with one line. `migrate!` embeds the `migrations`
directory in the binary, and `run` applies whatever is pending when the program starts:

```rust
    tisqlx::migrate!().run(&pool).await?;
```

Cargo doesn't watch that directory by itself. Run this once; it writes a `build.rs` that
rebuilds the program when a migration is added:

```sh
tisqlx migrate build-script
```

Now add a second migration: `tisqlx migrate add add_bio`, with
`ALTER TABLE users ADD COLUMN bio TEXT;` in the file. This time don't apply it by hand:
`cargo run` does, and `tisqlx migrate info` shows both as applied.

One TiDB fact to remember: it commits every DDL statement on its own, so a migration that
fails half-way is not rolled back. tisqlx then marks it *dirty* and refuses to go on until you
have [repaired it](how-to.md#repair-a-failed-migration).

## Where to go next

- A task to get done: the [how-to guides](how-to.md), starting with
  [transactions](how-to.md#use-transactions) and [tests](how-to.md#test-against-a-database).
- A larger program to read: the [axum example](how-to.md#serve-an-http-api-with-axum).
- Why it behaves this way: the [explanation](explanation.md).
- Every option: the [reference](reference.md).
