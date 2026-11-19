//! End-to-end tests of the `tisqlx` binary. The live ones use the dedicated test
//! TiDB (`TISQLX_TEST_URL`) and are `#[ignore]`d.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tisqlx(dir: &Path, url: Option<&str>, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tisqlx"));
    cmd.current_dir(dir).args(args).env_remove("DATABASE_URL");
    if let Some(url) = url {
        cmd.env("DATABASE_URL", url);
    }
    cmd.output().unwrap()
}

fn ok(out: Output) -> String {
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {}", String::from_utf8_lossy(&out.stderr));
    stdout
}

fn fail(out: Output) -> String {
    assert!(!out.status.success(), "expected failure, got: {}", String::from_utf8_lossy(&out.stdout));
    String::from_utf8(out.stderr).unwrap()
}

fn temp_dir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cli_{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    v.sort();
    v
}

#[test]
fn migrate_add_without_database() {
    let d = temp_dir("add");
    let out = ok(tisqlx(&d, None, &["migrate", "add", "--sequential", "-r", "create users"]));
    assert!(out.contains("Congratulations"), "first migration explains migrate!()");
    ok(tisqlx(&d, None, &["migrate", "add", "add", "index"]));
    // reversible-ness and sequential numbering are inferred from the existing files
    assert_eq!(
        files(&d.join("migrations")),
        ["0001_create_users.down.sql", "0001_create_users.up.sql", "0002_add_index.down.sql", "0002_add_index.up.sql"]
    );
    let e = fail(tisqlx(&d, None, &["migrate", "add", "--simple", "x"]));
    assert!(e.contains("cannot add a simple migration after reversible ones"), "{e}");
    let e = fail(tisqlx(&d, None, &["migrate", "add", "fix.down"]));
    assert!(e.contains("must not contain"), "{e}");

    ok(tisqlx(&d, None, &["migrate", "build-script"]));
    assert!(std::fs::read_to_string(d.join("build.rs")).unwrap().contains(r#"rerun-if-changed={}", "migrations""#));
    assert!(fail(tisqlx(&d, None, &["migrate", "build-script"])).contains("--force"));
}

/// The path is embedded in a `println!` literal: braces, quotes and backslashes are escaped.
#[test]
fn build_script_compiles_for_any_path() {
    let d = temp_dir("build_script");
    let source = r#"mig{0}"\x}"#;
    ok(tisqlx(&d, None, &["migrate", "build-script", "--source", source]));
    let out =
        Command::new("rustc").current_dir(&d).args(["--edition", "2024", "build.rs", "-o", "build"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = Command::new(d.join("build")).output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("cargo:rerun-if-changed={source}\n"));
}

/// The nearest `.env` *file* of the current directory and its ancestors is read (a directory of
/// that name is skipped), and the real environment wins. A malformed line ends the file, with a
/// warning. (The macros differ: they start at the crate's directory and skip a malformed line.)
#[test]
fn dotenv_lookup() {
    let d = temp_dir("dotenv");
    let sub = d.join("sub");
    std::fs::create_dir_all(sub.join(".env")).unwrap();
    std::fs::write(d.join(".env"), "DATABASE_URL=tidb://root@127.0.0.1:1/from_dotenv\n").unwrap();
    let e = fail(tisqlx(&sub, None, &["database", "create"]));
    assert!(e.contains("failed to connect to 127.0.0.1:1"), "{e}");
    let e = fail(tisqlx(&sub, Some("tidb://root@127.0.0.1:2/x"), &["database", "create"]));
    assert!(e.contains("failed to connect to 127.0.0.1:2"), "{e}");

    std::fs::write(d.join(".env"), "not a valid line\nDATABASE_URL=tidb://root@127.0.0.1:1/from_dotenv\n").unwrap();
    let e = fail(tisqlx(&sub, None, &["database", "create"]));
    assert!(e.contains("warning: ") && e.contains(".env") && e.contains("no database URL"), "{e}");
}

#[test]
fn commands_needing_a_database_say_so() {
    let d = temp_dir("nourl");
    let e = fail(tisqlx(&d, None, &["migrate", "info"]));
    assert!(e.contains("reading migrations directory migrations"), "{e}");
    std::fs::create_dir_all(d.join("migrations")).unwrap();
    let e = fail(tisqlx(&d, None, &["migrate", "info"]));
    assert!(e.contains("DATABASE_URL"), "{e}");
    let e = fail(tisqlx(&d, Some("tidb://root@127.0.0.1:1/x"), &["database", "create"]));
    assert!(e.contains("failed to connect"), "{e}");
    let e = fail(tisqlx(&d, Some("tidb://root@127.0.0.1:1"), &["database", "create"]));
    assert!(e.contains("does not name a database"), "{e}");
}

#[test]
#[ignore = "needs a TiDB: run with `make test-live`"]
fn migration_lifecycle_against_tidb() {
    let Some(base) = std::env::var("TISQLX_TEST_URL").ok().filter(|u| !u.is_empty()) else {
        assert!(
            std::env::var_os("TISQLX_TEST_REQUIRE").is_none(),
            "TISQLX_TEST_REQUIRE is set but TISQLX_TEST_URL is not"
        );
        eprintln!("skipped: set TISQLX_TEST_URL to run the live CLI test");
        return;
    };
    let url = format!("{base}/tisqlx_t_cli");
    let url = Some(url.as_str());
    let d = temp_dir("live");
    let m = d.join("migrations");
    std::fs::create_dir_all(&m).unwrap();
    std::fs::write(m.join("1_a.up.sql"), "CREATE TABLE a (id INT PRIMARY KEY);").unwrap();
    std::fs::write(m.join("1_a.down.sql"), "DROP TABLE a;").unwrap();
    std::fs::write(m.join("2_b.up.sql"), "CREATE TABLE b (id INT PRIMARY KEY);").unwrap();
    std::fs::write(m.join("2_b.down.sql"), "DROP TABLE b;").unwrap();

    ok(tisqlx(&d, url, &["database", "drop", "-y"]));
    let out = ok(tisqlx(&d, url, &["database", "setup"]));
    assert!(out.contains("Applied 1/a") && out.contains("Applied 2/b"), "{out}");
    assert!(ok(tisqlx(&d, url, &["migrate", "run", "--dry-run"])).contains("No pending migrations"));
    let info = ok(tisqlx(&d, url, &["migrate", "info"]));
    assert_eq!(info.lines().filter(|l| l.contains("applied")).count(), 2, "{info}");

    // reset reads the migrations before it drops anything
    let e = fail(tisqlx(&d, url, &["database", "reset", "-y", "--source", "nowhere"]));
    assert!(e.contains("reading migrations directory nowhere"), "{e}");
    let info = ok(tisqlx(&d, url, &["migrate", "info"]));
    assert_eq!(info.lines().filter(|l| l.contains("applied")).count(), 2, "{info}");

    assert!(ok(tisqlx(&d, url, &["migrate", "revert", "--dry-run"])).contains("Would revert 2/b"));
    assert!(ok(tisqlx(&d, url, &["migrate", "revert", "--all"])).contains("Reverted 1/a"));
    assert!(ok(tisqlx(&d, url, &["migrate", "info"])).contains("pending"));

    // a failing migration is reported dirty and blocks later runs until repaired
    std::fs::write(m.join("3_c.up.sql"), "CREATE TABLE c (id INT PRIMARY KEY); INSERT INTO nope VALUES (1);").unwrap();
    std::fs::write(m.join("3_c.down.sql"), "DROP TABLE c;").unwrap();
    let out = tisqlx(&d, url, &["migrate", "run"]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let e = fail(out);
    assert!(stdout.contains("FAILED  3/c (now DIRTY)"), "{stdout}");
    assert!(e.contains("tisqlx migrate repair 3"), "{e}");
    assert!(fail(tisqlx(&d, url, &["migrate", "run"])).contains("dirty"));
    assert!(ok(tisqlx(&d, url, &["migrate", "info"])).contains("DIRTY"));
    // removing a healthy migration's record is refused; the dirty one can be removed
    assert!(fail(tisqlx(&d, url, &["migrate", "repair", "1", "--remove"])).contains("not dirty"));
    std::fs::write(m.join("3_c.up.sql"), "CREATE TABLE IF NOT EXISTS c (id INT PRIMARY KEY);").unwrap();
    ok(tisqlx(&d, url, &["migrate", "repair", "3", "--remove"]));
    assert!(ok(tisqlx(&d, url, &["migrate", "run"])).contains("Applied 3/c"));
    assert!(fail(tisqlx(&d, url, &["migrate", "repair", "9", "--remove"])).contains("no record"));

    // a failing down migration is reported as such, not as reverted
    std::fs::write(m.join("3_c.down.sql"), "DROP TABLE c; DROP TABLE nope;").unwrap();
    let out = tisqlx(&d, url, &["migrate", "revert"]);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    fail(out);
    assert!(stdout.contains("FAILED   3/c (now DIRTY)") && !stdout.contains("Reverted 3/c"), "{stdout}");

    ok(tisqlx(&d, url, &["database", "drop", "-y"]));
}
