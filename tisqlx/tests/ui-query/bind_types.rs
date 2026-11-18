#[derive(tisqlx::Type)]
struct UserId(i64);

fn main() {
    // text for an integer column
    let _ = tisqlx::query!("SELECT id FROM users WHERE id = ?", "1");
    // a number for a DATETIME column
    let since = 5i64;
    let _ = tisqlx::query!("SELECT id FROM users WHERE created_at > ?", since);
    // a float for a VARCHAR column, in INSERT ... VALUES
    let _ = tisqlx::query!("INSERT INTO posts (user_id, title) VALUES (?, ?)", 1i64, 2.5f64);
    // an unsuffixed integer literal for a VARCHAR column
    let _ = tisqlx::query!("SELECT id FROM users WHERE name = ?", 5);
    // a derived newtype is checked like its field
    let _ = tisqlx::query!("SELECT id FROM users WHERE name = ?", UserId(1));
    // a number for the pattern of a VARCHAR column
    let _ = tisqlx::query!("SELECT id FROM users WHERE name LIKE ?", 5i64);
    // text for LIMIT
    let _ = tisqlx::query!("SELECT id FROM users LIMIT ?", "10");
}
