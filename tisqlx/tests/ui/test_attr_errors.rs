#[tisqlx::test]
fn not_async() {}

#[tisqlx::test(seed = "x")]
async fn unknown_argument() {}

#[tisqlx::test(fixtures("tests/fixtures/missing.sql"))]
async fn missing_fixture() {}

#[tisqlx::test]
async fn two_arguments(a: tisqlx::Pool, b: tisqlx::Pool) {}

fn main() {}
