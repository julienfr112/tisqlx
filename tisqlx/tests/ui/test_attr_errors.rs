#[tisqlx::test]
fn not_async() {}

#[tisqlx::test(fixtures = "x")]
async fn unknown_argument() {}

#[tisqlx::test]
async fn two_arguments(a: tisqlx::Pool, b: tisqlx::Pool) {}

fn main() {}
