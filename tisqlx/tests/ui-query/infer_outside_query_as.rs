fn main() {
    let _ = tisqlx::query!("SELECT id AS `id: _` FROM users");
}
