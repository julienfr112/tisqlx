fn main() {
    let _ = tisqlx::query_scalar!("SELECT id, name FROM users");
}
