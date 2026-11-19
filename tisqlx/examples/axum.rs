//! A small HTTP API on axum and tisqlx, in one file.
//!
//! ```sh
//! tisqlx database create -D tidb://root@127.0.0.1:4000/blog
//! DATABASE_URL=tidb://root@127.0.0.1:4000/blog cargo run -p tisqlx --example axum
//!
//! curl -X POST localhost:3000/users -H 'content-type: application/json' -d '{"name": "alice"}'
//! curl 'localhost:3000/users?limit=10'
//! ```
//!
//! | route | shows |
//! |---|---|
//! | `GET /health` | a pooled connection, the pool's size |
//! | `POST /users` | a checked `INSERT`, a JSON column, the generated id |
//! | `GET /users` | `query_as!` into a struct, an enum column, an optional stale read |
//! | `GET /users/{id}` | `fetch_optional`, a 404 |
//! | `DELETE /users/{id}` | a plain transaction over two tables |
//! | `POST /users/import` | a bulk insert |
//! | `POST /users/{id}/posts` | a transaction retried on write conflicts, with an application error |
//!
//! The schema is `tests/migrations` (tables `users` and `posts`), embedded by `migrate!` and
//! applied at startup. The queries are checked at compile time, here against the offline data
//! in `.tisqlx/`.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tisqlx::{BulkInsert, Pool, PoolOptions, Retryable, StaleRead, TxnOptions};

static MIGRATOR: tisqlx::migrate::Migrator = tisqlx::migrate!("tests/migrations");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL")?;
    let pool = PoolOptions::new().max_connections(20).min_connections(2).connect(&url).await?;
    MIGRATOR.run(&pool).await?;

    let app = Router::new()
        .route("/health", get(health))
        .route("/users", get(list_users).post(create_user))
        .route("/users/import", post(import_users))
        .route("/users/{id}", get(get_user).delete(delete_user))
        .route("/users/{id}/posts", post(create_post))
        .with_state(pool.clone());

    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    // In-flight requests are done: close the idle connections cleanly.
    pool.close().await;
    Ok(())
}

// ------------------------------------------------------------------ errors

enum AppError {
    NotFound,
    /// The request is valid but the data refuses it.
    Conflict(&'static str),
    Db(tisqlx::Error),
}

impl From<tisqlx::Error> for AppError {
    fn from(e: tisqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Lets `Pool::transaction` retry a closure that returns `AppError`.
impl Retryable for AppError {
    fn is_retryable(&self) -> bool {
        matches!(self, Self::Db(e) if e.is_retryable())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound | Self::Db(tisqlx::Error::RowNotFound) => (StatusCode::NOT_FOUND, "not found".to_owned()),
            Self::Conflict(why) => (StatusCode::CONFLICT, why.to_owned()),
            Self::Db(tisqlx::Error::PoolTimedOut) => (StatusCode::SERVICE_UNAVAILABLE, "database is busy".to_owned()),
            Self::Db(e) => match e.as_database_error() {
                Some(db) if db.is_unique_violation() => (StatusCode::CONFLICT, "already exists".to_owned()),
                Some(db) if db.is_foreign_key_violation() || db.is_check_violation() => {
                    (StatusCode::UNPROCESSABLE_ENTITY, db.message.clone())
                }
                _ => {
                    eprintln!("database error: {e}");
                    (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_owned())
                }
            },
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

type ApiResult<T> = Result<T, AppError>;

// ------------------------------------------------------------------ model

/// `ENUM('active', 'banned')`, read and written as text.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, tisqlx::Type)]
#[tisqlx(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
enum Status {
    Active,
    Banned,
}

/// `query_as!` fills it by field name; the types are checked against the columns.
#[derive(Serialize)]
struct User {
    /// An `AUTO_RANDOM` id uses all 64 bits: a string in JSON, which JavaScript can't read as a number.
    #[serde(serialize_with = "as_string")]
    id: i64,
    name: String,
    email: Option<String>,
    status: Status,
    profile: Option<Value>,
}

fn as_string<S: serde::Serializer>(id: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(id)
}

#[derive(Deserialize)]
struct NewUser {
    name: String,
    email: Option<String>,
    profile: Option<Value>,
}

// ------------------------------------------------------------------ handlers

async fn health(State(pool): State<Pool>) -> ApiResult<Json<Value>> {
    pool.acquire().await?.ping().await?;
    Ok(Json(json!({ "connections": pool.size(), "idle": pool.num_idle() })))
}

async fn create_user(State(pool): State<Pool>, Json(new): Json<NewUser>) -> ApiResult<(StatusCode, Json<Value>)> {
    let done =
        tisqlx::query!("INSERT INTO users (name, email, profile) VALUES (?, ?, ?)", new.name, new.email, new.profile)
            .execute(&pool)
            .await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": done.last_insert_id().to_string() }))))
}

#[derive(Deserialize)]
struct ListParams {
    limit: Option<u32>,
    /// Accept data up to this old, so that any replica can answer.
    max_staleness_secs: Option<u64>,
}

async fn list_users(State(pool): State<Pool>, Query(params): Query<ListParams>) -> ApiResult<Json<Vec<User>>> {
    let options = match params.max_staleness_secs {
        Some(secs) => TxnOptions::stale_read(StaleRead::MaxStaleness(Duration::from_secs(secs))),
        None => TxnOptions::default(),
    };
    let mut tx = pool.begin_with(&options).await?;
    let users = tisqlx::query_as!(
        User,
        "SELECT id, name, email, status AS `status: Status`, profile FROM users ORDER BY created_at DESC LIMIT ?",
        params.limit.unwrap_or(50).min(1000)
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(users))
}

async fn get_user(State(pool): State<Pool>, Path(id): Path<i64>) -> ApiResult<Json<User>> {
    tisqlx::query_as!(User, "SELECT id, name, email, status AS `status: Status`, profile FROM users WHERE id = ?", id)
        .fetch_optional(&pool)
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

/// Both deletes or neither: an early return drops `tx`, which rolls it back.
async fn delete_user(State(pool): State<Pool>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let mut tx = pool.begin().await?;
    tisqlx::query!("DELETE FROM posts WHERE user_id = ?", id).execute(&mut *tx).await?;
    let done = tisqlx::query!("DELETE FROM users WHERE id = ?", id).execute(&mut *tx).await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Multi-row statements, sized to TiDB's limits. Each batch commits on its own.
async fn import_users(State(pool): State<Pool>, Json(users): Json<Vec<NewUser>>) -> ApiResult<Json<Value>> {
    let rows = users.iter().map(|u| (&u.name, &u.email, &u.profile));
    let imported = BulkInsert::new("users", &["name", "email", "profile"]).execute(&pool, rows).await?;
    Ok(Json(json!({ "imported": imported })))
}

#[derive(Clone, Deserialize)]
struct NewPost {
    title: String,
    body: Option<String>,
}

/// Lock the author, check it, insert the post and bump the score, as one transaction.
/// On a write conflict or a deadlock TiDB refuses it, and `transaction` runs the closure again:
/// it must hold nothing but SQL, and each run gets its own copy of the input.
async fn create_post(
    State(pool): State<Pool>,
    Path(user_id): Path<i64>,
    Json(post): Json<NewPost>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let id = pool
        .transaction(|tx| {
            let post = post.clone();
            Box::pin(async move {
                let author =
                    tisqlx::query!("SELECT status AS `status: Status` FROM users WHERE id = ? FOR UPDATE", user_id)
                        .fetch_optional(&mut **tx)
                        .await?
                        .ok_or(AppError::NotFound)?;
                if author.status == Status::Banned {
                    return Err(AppError::Conflict("this user is banned"));
                }
                let done = tisqlx::query!(
                    "INSERT INTO posts (user_id, title, body) VALUES (?, ?, ?)",
                    user_id,
                    post.title,
                    post.body
                )
                .execute(&mut **tx)
                .await?;
                tisqlx::query!("UPDATE users SET score = score + 1 WHERE id = ?", user_id).execute(&mut **tx).await?;
                Ok(done.last_insert_id())
            })
        })
        .await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}
