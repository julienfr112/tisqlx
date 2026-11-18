fn main() {
    let _ = tisqlx::query!("SELECT COUNT(*) FROM users");
}
