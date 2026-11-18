//! `#[derive(FromRow, Type)]` expansion tests (no database), and compile-fail
//! tests for macro errors (`tests/ui`).

use tisqlx::{FromRow, Type};

// FromRow borrowing from the row through the struct's own lifetime.
#[derive(Debug, FromRow)]
#[allow(dead_code)]
struct Borrowed<'a> {
    name: &'a str,
    bytes: &'a [u8],
}

// Generic struct with a flattened part and attribute variety.
#[derive(Debug, FromRow)]
#[tisqlx(rename_all = "camelCase")]
#[allow(dead_code)]
struct Page<T> {
    #[tisqlx(flatten)]
    item: T,
    total_count: i64,
    #[tisqlx(default)]
    missing: Option<String>,
    #[tisqlx(skip)]
    local: u8,
    #[tisqlx(try_from = "i64")]
    small: u8,
}

// Type newtypes: generic, borrowed, and nested.
#[derive(Debug, Type)]
#[allow(dead_code)]
struct Id<T>(T);

#[derive(Debug, Type)]
#[allow(dead_code)]
struct Name<'a>(&'a str);

#[derive(Debug, Type)]
#[allow(dead_code)]
struct UserId(Id<i64>);

#[derive(Debug, PartialEq, Type)]
#[tisqlx(rename_all = "SCREAMING_SNAKE_CASE")]
enum Status {
    InProgress,
    #[tisqlx(rename = "done")]
    Done,
}

fn assert_traits<'r, T: tisqlx::Decode<'r> + Type>() {}
fn assert_encode<T: tisqlx::Encode>() {}
fn assert_from_row<'r, T: FromRow<'r>>() {}

#[test]
fn derived_impls_exist() {
    assert_from_row::<Borrowed<'_>>();
    assert_from_row::<Page<Borrowed<'_>>>();
    assert_traits::<Id<i64>>();
    assert_traits::<Name<'_>>();
    assert_traits::<UserId>();
    assert_traits::<Status>();
    assert_encode::<Id<String>>();
    assert_encode::<UserId>();
    assert_encode::<Status>();
    assert_eq!(<UserId as Type>::type_info(), <i64 as Type>::type_info());
}

/// Derive errors: no database involved.
#[test]
fn derive_compile_errors() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}

/// Query macro errors need the queries described by a live database: run by
/// `make test-live`, which compiles with `DATABASE_URL` set and `TISQLX_OFFLINE=false`.
#[test]
#[ignore = "needs DATABASE_URL and TISQLX_OFFLINE=false: run with `make test-live`"]
fn query_compile_errors() {
    let online = std::env::var("DATABASE_URL").is_ok_and(|u| !u.is_empty())
        && std::env::var("TISQLX_OFFLINE").is_ok_and(|v| v == "false");
    if !online {
        assert!(
            std::env::var_os("TISQLX_TEST_REQUIRE").is_none(),
            "TISQLX_TEST_REQUIRE needs DATABASE_URL and TISQLX_OFFLINE=false"
        );
        eprintln!("skipped: query compile-fail tests need DATABASE_URL and TISQLX_OFFLINE=false");
        return;
    }
    trybuild::TestCases::new().compile_fail("tests/ui-query/*.rs");
}
