//! Naming of the offline query data shared by the macros and `tisqlx prepare`.

use sha2::{Digest, Sha256};

/// Directory holding offline query data (in the crate or workspace root).
pub const DIR: &str = ".tisqlx";

/// File name of the offline data for `sql`.
pub fn file_name(sql: &str) -> String {
    format!("query-{}.json", crate::util::hex(Sha256::digest(sql.as_bytes())))
}

/// Whether `name` looks like an offline data file.
pub fn is_file_name(name: &str) -> bool {
    name.starts_with("query-") && name.ends_with(".json")
}

#[cfg(test)]
mod tests {
    #[test]
    fn names() {
        let name = super::file_name("SELECT 1");
        assert_eq!(name, "query-e004ebd5b5532a4b85984a62f8ad48a81aa3460c1ca07701f386135d72cdecf5.json");
        assert!(super::is_file_name(&name));
        assert!(!super::is_file_name("query-x.json.tmp1-0"));
    }
}
