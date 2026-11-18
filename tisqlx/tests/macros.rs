//! Compile-time checked query tests.
//!
//! They compile from the committed offline data (`.tisqlx/`, see `tisqlx/.env`),
//! so building needs no database; `make prepare` regenerates it. Running the
//! queries needs the dedicated test TiDB (`TISQLX_TEST_URL`), else they are skipped.

use tisqlx::types::Decimal;
use tisqlx::types::chrono::{NaiveDate, NaiveDateTime};
use tisqlx::{FromRow, Pool};

static MIGRATOR: tisqlx::migrate::Migrator = tisqlx::migrate!("tests/migrations");

/// Pool on `<TISQLX_TEST_URL>/tisqlx_macros` with the test migrations applied, or `None` to skip.
async fn pool() -> Option<Pool> {
    let Some(url) = std::env::var("TISQLX_TEST_URL").ok().filter(|u| !u.is_empty()) else {
        assert!(
            std::env::var_os("TISQLX_TEST_REQUIRE").is_none(),
            "TISQLX_TEST_REQUIRE is set but TISQLX_TEST_URL is not"
        );
        eprintln!("skipped: set TISQLX_TEST_URL to run the macro queries");
        return None;
    };
    let mut admin = tisqlx::TiConnection::connect(&url).await.unwrap();
    admin.execute_raw("CREATE DATABASE IF NOT EXISTS tisqlx_macros").await.unwrap();
    let opts = url.parse::<tisqlx::ConnectOptions>().unwrap().database("tisqlx_macros");
    let pool = Pool::connect_with(opts).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    Some(pool)
}

#[derive(Debug, PartialEq, tisqlx::Type)]
#[tisqlx(rename_all = "lowercase")]
enum Status {
    Active,
    Banned,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
struct User {
    id: i64,
    name: String,
    email: Option<String>,
    score: Decimal,
}

#[derive(Debug, FromRow)]
#[tisqlx(rename_all = "snake_case")]
struct UserWithStatus {
    #[tisqlx(rename = "name")]
    display_name: String,
    status: Status,
    #[tisqlx(default)]
    missing_column: Option<i32>,
    #[tisqlx(json)]
    profile: Option<serde_json::Value>,
}

/// Names a user may have in scope where a macro expands.
struct Record;
impl Record {
    const MIN_ID: i64 = i64::MIN;
}

#[derive(Debug)]
struct Wrapper<T> {
    id: T,
}

#[tokio::test]
async fn generated_code_is_hygienic() {
    let Some(pool) = pool().await else { return };
    // `Record` here is the user's type, not the macro's generated struct.
    let rows = tisqlx::query!("SELECT id FROM users WHERE id >= ?", Record::MIN_ID).fetch_all(&pool).await.unwrap();
    let _: Vec<i64> = rows.into_iter().map(|r| r.id).collect();
    // A user variable named like the macro's internals binds its own value.
    let (__arg0, __args) = (i64::MIN, 0i64);
    let n: i64 = tisqlx::query_scalar!("SELECT COUNT(*) FROM users WHERE id >= ? AND ? = 0", __arg0, __args)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(n >= 0);
    // Generic record path.
    let w: Vec<Wrapper<i64>> =
        tisqlx::query_as!(Wrapper<i64>, "SELECT id FROM users WHERE id >= ?", __arg0).fetch_all(&pool).await.unwrap();
    assert_eq!(w.len() as i64, n);
    assert!(w.iter().all(|w| w.id >= __arg0));
}

#[cfg(all(feature = "bigdecimal", feature = "uuid"))]
#[tokio::test]
async fn macro_type_mapping() {
    use tisqlx::types::{BigDecimal, Decimal, Set, Uuid};
    let Some(pool) = pool().await else { return };
    let mut tx = pool.begin().await.unwrap();
    let id = Uuid::from_u128(42);
    tisqlx::query!(
        "INSERT INTO typed VALUES (1, ?, ?, ?, ?, ?)",
        Set::from_iter(["a", "c"]),
        true,
        "123456789012345678901234567890123.5".parse::<BigDecimal>().unwrap(),
        Decimal::new(1250, 2),
        id
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    // inferred: SET => Set, BIT(1) => bool, DECIMAL(65,30) => BigDecimal, DECIMAL(10,2) => Decimal
    let r = tisqlx::query!("SELECT tags, flag, big, small, ubin AS `ubin: Uuid` FROM typed WHERE id = 1")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let tags: Set = r.tags;
    assert!(tags.contains("c"));
    let flag: bool = r.flag;
    assert!(flag);
    let big: BigDecimal = r.big;
    assert_eq!(big.to_string().split('.').next().unwrap(), "123456789012345678901234567890123");
    let small: Decimal = r.small;
    assert_eq!(small, Decimal::new(1250, 2));
    assert_eq!(r.ubin, Some(id));
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn query_macros() {
    let Some(pool) = pool().await else { return };
    let mut tx = pool.begin().await.unwrap();

    let name = "alice";
    let r = tisqlx::query!(
        "INSERT INTO users (name, email, score) VALUES (?, ?, ?)",
        name,
        None::<String>,
        Decimal::new(1250, 2)
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let id = r.last_insert_id() as i64;
    assert!(id != 0, "AUTO_RANDOM id returned");

    // Anonymous record, inferred nullability.
    let row = tisqlx::query!("SELECT id, name, email, status, score, created_at FROM users WHERE id = ?", id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let _: i64 = row.id;
    let _: String = row.name;
    let _: Option<String> = row.email;
    let _: String = row.status;
    assert_eq!(row.score, Decimal::new(1250, 2));
    let _: NaiveDateTime = row.created_at;

    // query_as! into a struct.
    let u = tisqlx::query_as!(User, "SELECT id, name, email, score FROM users WHERE id = ?", id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(u.name, "alice");
    assert_eq!(u.email, None);

    // Overrides: force non-null, custom types, inferred type.
    let row = tisqlx::query!(
        "SELECT MAX(id) AS `max_id!`, COUNT(*) AS cnt, status AS `status: Status`, email AS `email!: String` FROM users WHERE id = ? GROUP BY status, email",
        id
    )
    .fetch_optional(&mut *tx)
    .await;
    // email is NULL but forced non-null: decoding errors at runtime instead of panicking.
    assert!(row.is_err());

    let s = tisqlx::query!(
        "SELECT MAX(id) AS `max_id!`, status AS `status: Status` FROM users WHERE id = ? GROUP BY status",
        id
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let _: i64 = s.max_id;
    assert_eq!(s.status, Status::Active);

    // Scalar + aggregate nullability (MAX is nullable).
    let max: Option<i64> = tisqlx::query_scalar!("SELECT MAX(id) FROM users").fetch_one(&mut *tx).await.unwrap();
    assert!(max.is_some());
    let count: i64 =
        tisqlx::query_scalar!("SELECT COUNT(*) FROM users WHERE name = ?", "alice").fetch_one(&mut *tx).await.unwrap();
    assert!(count >= 1);

    // LEFT JOIN makes the joined side nullable.
    let rows =
        tisqlx::query!("SELECT u.name, p.title FROM users u LEFT JOIN posts p ON p.user_id = u.id WHERE u.id = ?", id)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let _: Option<String> = rows[0].title.clone();

    // query_file!
    let n = tisqlx::query_file_scalar!("tests/queries/count_users.sql").fetch_one(&mut *tx).await.unwrap();
    assert!(n >= 1);

    // Runtime FromRow derive features.
    let u: UserWithStatus = tisqlx::query_as("SELECT name, status, profile FROM users WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(u.display_name, "alice");
    assert_eq!(u.status, Status::Active);
    assert_eq!(u.missing_column, None);
    assert_eq!(u.profile, None);

    tx.rollback().await.unwrap();
}

#[test]
fn migrate_macro_zero_padded_filenames() {
    // `tisqlx migrate add --sequential` writes `0001_...`; the macro must find those files.
    let m = tisqlx::migrate!("tests/migrations_padded");
    let v: Vec<(i64, &str, bool)> =
        m.migrations.iter().map(|m| (m.version, &*m.description, m.migration_type.is_up())).collect();
    assert_eq!(v, [(1, "add-table a", false), (1, "add-table a", true), (2, "b", false), (2, "b", true)]);
    let runtime =
        tisqlx::migrate::Migrator::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/migrations_padded")).unwrap();
    for (a, b) in m.migrations.iter().zip(runtime.migrations.iter()) {
        assert_eq!(a.checksum, b.checksum, "embedded and runtime checksums agree");
    }
}

#[derive(Debug, tisqlx::Type)]
struct UserId(i64);

/// A type with hand-written impls: tisqlx cannot classify it, so binding it is never rejected.
struct Shouted(&'static str);

impl tisqlx::Type for Shouted {
    fn type_info() -> tisqlx::TypeInfo {
        <str as tisqlx::Type>::type_info()
    }
}

impl tisqlx::Encode for Shouted {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<tisqlx::IsNull, tisqlx::error::BoxDynError> {
        tisqlx::Encode::encode(self.0.to_uppercase().as_str(), buf)
    }
}

impl<'r> tisqlx::Decode<'r> for Shouted {
    fn decode(_: tisqlx::ValueRef<'r>) -> Result<Self, tisqlx::error::BoxDynError> {
        Ok(Shouted("decoded"))
    }
}

/// A derived newtype over an unclassified type: unclassified too, so never rejected.
#[derive(tisqlx::Type)]
struct Loud(Shouted);

/// Generic arguments cannot be classified: never rejected.
fn by_name<T: tisqlx::Encode + Sync>(name: T) -> impl Sync {
    tisqlx::query!("SELECT id FROM users WHERE name = ?", name)
}

/// Bind arguments are checked against the column next to their `?`, by kind (number, text,
/// date, ...), not by exact Rust type. Compiling this is most of the test; the rejected cases
/// are in `tests/ui-query/bind_types.rs`.
#[tokio::test]
async fn bind_arguments_of_compatible_types() {
    let Some(pool) = pool().await else { return };
    let mut tx = pool.begin().await.unwrap();
    let since = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
    let n: i64 = tisqlx::query_scalar!(
        "SELECT COUNT(*) FROM users WHERE id > ? AND name <> ? AND email <=> ? AND score >= ? AND status IN (?, ?) AND created_at >= ? LIMIT ?",
        // any integer for BIGINT and DECIMAL, `Option`, `&str`, a derived enum, a date for DATETIME
        0i32,
        "nobody",
        None::<String>,
        0u8,
        Status::Active,
        Status::Banned,
        since,
        10u8
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(n >= 0);
    // text for JSON, a newtype like its field, an unclassified type, and `as _` to opt out
    tisqlx::query!(
        "UPDATE users SET profile = ?, email = ? WHERE id = ? OR name = ? OR id = ?",
        r#"{"a": 1}"#,
        Some(String::from("x")),
        UserId(-1),
        Shouted("nobody"),
        "-1" as _
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    // unsuffixed literals, and unclassified values
    tisqlx::query!("UPDATE users SET score = ? WHERE id = ? OR name = ?", 2.5, 5, Loud(Shouted("x")))
        .execute(&mut *tx)
        .await
        .unwrap();
    let _ = by_name("x");
    // Explicit conversion in SQL: a `?` inside a function call is not checked (README workaround).
    let id_text = String::from("-1");
    let found: Option<i64> = tisqlx::query_scalar!("SELECT id FROM users WHERE id = CAST(? AS SIGNED)", id_text)
        .fetch_optional(&mut *tx)
        .await
        .unwrap();
    assert_eq!(found, None);
    tisqlx::query!(
        "INSERT INTO posts (user_id, title, body) VALUES (?, ?, ?)",
        1u32,
        "t".to_owned(),
        Option::<&str>::None
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();
}
