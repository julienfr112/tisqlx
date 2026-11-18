fn main() {
    let _ = tisqlx::query!("SELECT id FROM users WHERE id = ? AND name = ?", 1);
}
