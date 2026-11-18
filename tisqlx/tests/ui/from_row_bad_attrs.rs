#[derive(tisqlx::FromRow)]
struct S {
    #[tisqlx(json, try_from = "i64")]
    a: u8,
    #[tisqlx(nope)]
    b: i32,
}

fn main() {}
