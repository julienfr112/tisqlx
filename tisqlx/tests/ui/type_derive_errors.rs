#[derive(tisqlx::Type)]
struct Two(i32, i32);

#[derive(tisqlx::Type)]
enum WithData {
    A(i32),
}

#[derive(tisqlx::Type)]
enum BadVariantAttr {
    #[tisqlx(skip)]
    A,
}

#[derive(tisqlx::Type)]
#[tisqlx(transparent)]
struct Old(i32);

#[derive(tisqlx::Type)]
enum Generic<const N: usize> {
    A,
}

#[derive(tisqlx::Type)]
#[tisqlx(rename_all = "snake_case")]
struct Renamed(i32);

fn main() {}
