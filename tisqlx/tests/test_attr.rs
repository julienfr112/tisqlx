//! `#[tisqlx::test]` itself. Needs `DATABASE_URL`, so the tests are `#[ignore]`d and
//! `make test-live` runs them with `--include-ignored`.

mod needs_database_url {
    use tisqlx::Pool;
    #[tisqlx::test(migrations = "tests/migrations")]
    #[ignore = "needs DATABASE_URL (make test-live)"]
    async fn gets_a_migrated_database(pool: Pool) {
        let db: String = tisqlx::query_scalar("SELECT DATABASE()").fetch_one(&pool).await.unwrap();
        assert_eq!(db, tisqlx::testing::database_name(concat!(module_path!(), "::gets_a_migrated_database")));
        // the schema from tests/migrations is there, and empty
        let n: i64 = tisqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tisqlx::test(migrations = "tests/migrations", fixtures("tests/fixtures/users.sql", "tests/fixtures/posts.sql"))]
    #[ignore = "needs DATABASE_URL (make test-live)"]
    async fn applies_fixtures_in_order(pool: Pool) {
        let names: Vec<String> =
            tisqlx::query_scalar("SELECT name FROM users ORDER BY name").fetch_all(&pool).await.unwrap();
        assert_eq!(names, ["alice", "bob"]);
        // the second fixture saw the first one's rows
        let titles: Vec<String> = tisqlx::query_scalar("SELECT title FROM posts").fetch_all(&pool).await.unwrap();
        assert_eq!(titles, ["hello"]);
    }

    #[tisqlx::test]
    #[ignore = "needs DATABASE_URL (make test-live)"]
    async fn returns_results(pool: Pool) -> tisqlx::Result<()> {
        tisqlx::raw_sql("CREATE TABLE t (id INT PRIMARY KEY)").execute(&pool).await?;
        let n: i64 = tisqlx::query_scalar("SELECT COUNT(*) FROM t").fetch_one(&pool).await?;
        assert_eq!(n, 0);
        Ok(())
    }

    #[tisqlx::test]
    #[ignore = "needs DATABASE_URL (make test-live)"]
    async fn without_a_pool() {}
}
