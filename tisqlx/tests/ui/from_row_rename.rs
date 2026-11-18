#[derive(tisqlx::FromRow)]
struct Tuple(#[tisqlx(rename = "id")] i64);

#[derive(tisqlx::FromRow)]
struct Skipped {
    #[tisqlx(skip, rename = "x")]
    a: i64,
}

#[derive(tisqlx::FromRow)]
struct Inner {
    id: i64,
}

#[derive(tisqlx::FromRow)]
struct Flattened {
    #[tisqlx(flatten, rename = "x")]
    inner: Inner,
}

#[derive(tisqlx::FromRow)]
#[tisqlx(rename_all = "camelCase")]
struct RenamedTuple(i64);

fn main() {}
