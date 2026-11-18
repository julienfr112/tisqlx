use tisqlx_core::*;

use crate::common::*;

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn types_roundtrip() {
    require_db!();
    let opts = fresh_db("types").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw(
        "CREATE TABLE t (
            id BIGINT PRIMARY KEY,
            b BOOLEAN, i8 TINYINT, u8 TINYINT UNSIGNED, i16 SMALLINT, i32 INT, u32 INT UNSIGNED,
            u64 BIGINT UNSIGNED, f FLOAT, d DOUBLE, dm DECIMAL(20,6), s VARCHAR(50), txt TEXT,
            bin VARBINARY(10), dt DATETIME(6), ts TIMESTAMP(3) NULL, dd DATE, tm TIME(6), j JSON,
            bits BIT(10), e ENUM('x','y'), yr YEAR
        )",
    )
    .await
    .unwrap();

    use chrono::{NaiveDate, NaiveTime, TimeDelta, TimeZone, Utc};
    use rust_decimal::Decimal;
    let dt = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap().and_hms_micro_opt(13, 14, 15, 123456).unwrap();
    let ts = Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 5).unwrap();
    let dec: Decimal = "12345678901234.123456".parse().unwrap();
    let json = serde_json::json!({"a": [1, 2, {"b": null}]});

    query("INSERT INTO t VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,b'1010101010','y',2024)")
        .bind(1i64)
        .bind(true)
        .bind(-128i8)
        .bind(255u8)
        .bind(-32768i16)
        .bind(i32::MIN)
        .bind(u32::MAX)
        .bind(u64::MAX)
        .bind(1.5f32)
        .bind(std::f64::consts::PI)
        .bind(dec)
        .bind("héllo")
        .bind(String::from("x").repeat(1000))
        .bind(&[0u8, 1, 255][..])
        .bind(dt)
        .bind(ts)
        .bind(dt.date())
        .bind(TimeDelta::hours(-30) - TimeDelta::microseconds(5))
        .bind(Json(&json))
        .execute(&mut conn)
        .await
        .unwrap();
    query("INSERT INTO t (id) VALUES (2)").execute(&mut conn).await.unwrap();

    // Both protocols must decode identically.
    for prepared in [true, false] {
        let sql = "SELECT * FROM t ORDER BY id";
        let rows = if prepared {
            query(sql).fetch_all(&mut conn).await.unwrap()
        } else {
            raw_sql(sql).fetch_all(&mut conn).await.unwrap()
        };
        let r = &rows[0];
        assert!(r.get::<bool, _>("b"), "prepared={prepared}");
        assert_eq!(r.get::<i8, _>("i8"), -128);
        assert_eq!(r.get::<u8, _>("u8"), 255);
        assert_eq!(r.get::<i16, _>("i16"), -32768);
        assert_eq!(r.get::<i32, _>("i32"), i32::MIN);
        assert_eq!(r.get::<u32, _>("u32"), u32::MAX);
        assert_eq!(r.get::<u64, _>("u64"), u64::MAX);
        assert_eq!(r.get::<f32, _>("f"), 1.5);
        assert_eq!(r.get::<f64, _>("d"), std::f64::consts::PI);
        assert_eq!(r.get::<Decimal, _>("dm"), dec);
        assert_eq!(r.get::<&str, _>("s"), "héllo");
        assert_eq!(r.get::<String, _>("txt").len(), 1000);
        assert_eq!(r.get::<Vec<u8>, _>("bin"), vec![0, 1, 255]);
        assert_eq!(r.get::<chrono::NaiveDateTime, _>("dt"), dt);
        assert_eq!(r.get::<chrono::DateTime<Utc>, _>("ts"), ts);
        assert_eq!(r.get::<NaiveDate, _>("dd"), dt.date());
        assert_eq!(r.get::<TimeDelta, _>("tm"), TimeDelta::hours(-30) - TimeDelta::microseconds(5));
        assert_eq!(r.get::<serde_json::Value, _>("j"), json);
        assert_eq!(r.get::<u64, _>("bits"), 0b1010101010);
        assert_eq!(r.get::<String, _>("e"), "y");
        assert_eq!(r.get::<u16, _>("yr"), 2024);

        let n = &rows[1];
        assert_eq!(n.get::<Option<i32>, _>("i32"), None);
        assert_eq!(n.get::<Option<chrono::NaiveDateTime>, _>("dt"), None);
        assert!(n.try_get::<i32, _>("i32").is_err());
    }

    let t: NaiveTime = query_scalar("SELECT CAST('12:34:56.5' AS TIME(1))").fetch_one(&mut conn).await.unwrap();
    assert_eq!(t, NaiveTime::from_hms_milli_opt(12, 34, 56, 500).unwrap());

    // Overflow is an error, not a wrap.
    assert!(query_scalar::<i8>("SELECT 300").fetch_one(&mut conn).await.is_err());
    // Mismatched type is an error.
    assert!(query_scalar::<String>("SELECT 1").fetch_one(&mut conn).await.is_err());
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn describe_nullability() {
    require_db!();
    let opts = fresh_db("describe").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw(
        "CREATE TABLE a (id INT PRIMARY KEY, name VARCHAR(10) NOT NULL, note TEXT);
         CREATE TABLE b (id INT PRIMARY KEY, a_id INT NOT NULL)",
    )
    .await
    .unwrap();
    let d = conn
        .describe("SELECT a.id, a.name, a.note, b.a_id, 1 + 1 AS two, a.id + ? AS p FROM a LEFT JOIN b ON b.a_id = a.id WHERE a.name = ?")
        .await
        .unwrap();
    let names: Vec<&str> = d.columns.iter().map(|c| c.name()).collect();
    assert_eq!(names, ["id", "name", "note", "a_id", "two", "p"]);
    let not_null: Vec<bool> = d.columns.iter().map(|c| c.type_info().is_not_null()).collect();
    assert_eq!(d.params.len(), 2);
    assert!(not_null[0]);
    assert!(!not_null[2]);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn vector_columns() {
    require_db!();
    let opts = fresh_db("vector").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE v (id INT PRIMARY KEY, e VECTOR(3)); INSERT INTO v VALUES (1, '[1,2.5,3]')")
        .await
        .unwrap();
    // binary and text protocol
    let e: String = query_scalar("SELECT e FROM v WHERE id = ?").bind(1).fetch_one(&mut conn).await.unwrap();
    assert_eq!(e, "[1,2.5,3]");
    let row = raw_sql("SELECT e FROM v").fetch_one(&mut conn).await.unwrap();
    assert_eq!(row.get::<&str, _>(0), "[1,2.5,3]");
    // TiDB sends VECTOR as a text column
    assert_eq!(row.columns()[0].type_info().name(), "TEXT");
    let d: f64 =
        query_scalar("SELECT VEC_L2_DISTANCE(e, ?) FROM v").bind("[1,2.5,3]").fetch_one(&mut conn).await.unwrap();
    assert_eq!(d, 0.0);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn json_and_bytes_variants() {
    require_db!();
    let opts = fresh_db("jsonbytes").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE t (id INT PRIMARY KEY, j JSON, b VARBINARY(16), s VARCHAR(16))").await.unwrap();

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Doc {
        tags: Vec<String>,
    }
    let doc = Doc { tags: vec!["a".into(), "é".into()] };
    query("INSERT INTO t VALUES (1, ?, ?, ?), (2, NULL, NULL, NULL)")
        .bind(Json(&doc))
        .bind([1u8, 2, 3, 4])
        .bind(std::borrow::Cow::Borrowed("cow"))
        .execute(&mut conn)
        .await
        .unwrap();
    query("UPDATE t SET j = ? WHERE id = 2").bind(serde_json::json!(null)).execute(&mut conn).await.unwrap();

    for (sql, args) in [("SELECT j, b, s FROM t WHERE id = ?", true), ("SELECT j, b, s FROM t WHERE id = 1", false)] {
        let row = if args {
            query(sql).bind(1).fetch_one(&mut conn).await.unwrap()
        } else {
            raw_sql(sql).fetch_one(&mut conn).await.unwrap()
        };
        assert_eq!(row.get::<Json<Doc>, _>("j").0, doc, "{sql}");
        assert_eq!(row.get::<[u8; 4], _>("b"), [1, 2, 3, 4]);
        assert_eq!(row.get::<&[u8], _>("b"), &[1, 2, 3, 4]);
        assert!(row.try_get::<[u8; 3], _>("b").is_err(), "length checked");
        assert_eq!(row.get::<std::borrow::Cow<'_, str>, _>("s"), "cow");
        // text and bytes are not interchangeable with JSON
        assert!(row.try_get::<i64, _>("j").is_err());
    }
    // JSON `null` vs SQL NULL: Json<Option<T>> reads both as None, Option<Json<T>> tells them apart
    let row = query("SELECT j, b FROM t WHERE id = ?").bind(2).fetch_one(&mut conn).await.unwrap();
    assert_eq!(row.get::<Json<Option<Doc>>, _>("j").0, None);
    assert_eq!(row.get::<Option<Vec<u8>>, _>("b"), None);
    let null_json: Option<serde_json::Value> =
        query_scalar("SELECT j FROM t WHERE id = 2").fetch_one(&mut conn).await.unwrap();
    assert_eq!(null_json, Some(serde_json::Value::Null));
    let sql_null: Option<serde_json::Value> =
        query_scalar("SELECT CAST(NULL AS JSON)").fetch_one(&mut conn).await.unwrap();
    assert_eq!(sql_null, None);
    let cow = std::borrow::Cow::Borrowed(&b"cow"[..]);
    let echoed: Vec<u8> = query_scalar("SELECT ?").bind(cow).fetch_one(&mut conn).await.unwrap();
    assert_eq!(echoed, b"cow");
}

/// Optional and TiDB-specific types, through both protocols (prepared binds + binary
/// rows, and text rows).
#[cfg(all(feature = "time", feature = "uuid", feature = "bigdecimal"))]
#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn extended_types() {
    use std::str::FromStr;
    use std::sync::Arc;

    use bigdecimal::BigDecimal;

    require_db!();
    let opts = fresh_db("exttypes").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw(
        "CREATE TABLE x (
            id INT PRIMARY KEY,
            big DECIMAL(65, 30), ubin BINARY(16), utext CHAR(36), tags SET('a','b','c'), flag BIT(1),
            d DATE, dt DATETIME(6), ts TIMESTAMP(6), t TIME(6), neg TIME, txt TEXT, blb BLOB
        )",
    )
    .await
    .unwrap();

    let big = BigDecimal::from_str("12345678901234567890123456789012345.123456789012345678901234567890").unwrap();
    let id = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    let date = time::Date::from_calendar_date(2026, time::Month::November, 18).unwrap();
    let dt = date.with_hms_micro(9, 5, 7, 123_456).unwrap();
    let ts = dt.assume_utc();
    let t = time::Time::from_hms_micro(23, 59, 58, 1).unwrap();
    query("INSERT INTO x VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(&big)
        .bind(id)
        .bind(id.hyphenated())
        .bind(Set::from_iter(["a", "c"]))
        .bind(true)
        .bind(date)
        .bind(dt)
        .bind(ts)
        .bind(t)
        .bind(time::Duration::seconds(-90_061))
        .bind(Arc::<str>::from("shared"))
        .bind(Box::<[u8]>::from(&b"boxed"[..]))
        .execute(&mut conn)
        .await
        .unwrap();

    for prepared in [true, false] {
        let sql = "SELECT big, ubin, utext, tags, flag, d, dt, ts, t, neg, txt, blb FROM x WHERE id = 1";
        let row = if prepared {
            query(sql).fetch_one(&mut conn).await.unwrap()
        } else {
            raw_sql(sql).fetch_one(&mut conn).await.unwrap()
        };
        let p = if prepared { "binary" } else { "text" };
        assert_eq!(row.get::<BigDecimal, _>("big"), big, "{p}: 65-digit DECIMAL");
        assert!(row.try_get::<rust_decimal::Decimal, _>("big").is_err(), "{p}: too wide for rust_decimal");
        assert_eq!(row.get::<uuid::Uuid, _>("ubin"), id, "{p}");
        assert_eq!(row.get::<uuid::Uuid, _>("utext"), id, "{p}: textual uuid");
        assert_eq!(row.get::<uuid::fmt::Hyphenated, _>("utext"), id.hyphenated(), "{p}");
        assert_eq!(row.get::<Set, _>("tags"), Set::from_iter(["a", "c"]), "{p}");
        assert!(row.get::<Set, _>("tags").contains("c"));
        assert!(row.get::<bool, _>("flag"), "{p}: BIT(1)");
        assert_eq!(row.get::<time::Date, _>("d"), date, "{p}");
        assert_eq!(row.get::<time::PrimitiveDateTime, _>("dt"), dt, "{p}");
        assert_eq!(row.get::<time::OffsetDateTime, _>("ts"), ts, "{p}");
        assert_eq!(row.get::<time::Time, _>("t"), t, "{p}");
        assert_eq!(row.get::<std::time::Duration, _>("t"), std::time::Duration::from_micros(86_398_000_001), "{p}");
        assert_eq!(row.get::<time::Duration, _>("neg"), time::Duration::seconds(-90_061), "{p}: -25:01:01");
        assert!(row.try_get::<std::time::Duration, _>("neg").is_err(), "{p}: negative TIME");
        assert!(row.try_get::<time::Time, _>("neg").is_err(), "{p}: TIME outside a day");
        assert_eq!(&*row.get::<Arc<str>, _>("txt"), "shared", "{p}");
        assert_eq!(&*row.get::<Box<str>, _>("txt"), "shared", "{p}");
        assert_eq!(&*row.get::<Box<[u8]>, _>("blb"), b"boxed", "{p}");
        assert_eq!(&*row.get::<Arc<[u8]>, _>("blb"), b"boxed", "{p}");
        // chrono reads the same values
        assert_eq!(row.get::<chrono::NaiveDateTime, _>("dt").to_string(), "2026-11-18 09:05:07.123456");
    }

    // an empty SET, and a member that cannot be encoded
    let empty: Set = query_scalar("SELECT CAST('' AS CHAR)").fetch_one(&mut conn).await.unwrap();
    assert_eq!(empty, Set::default());
    assert!(query("SELECT ?").bind(Set::from_iter(["a,b"])).execute(&mut conn).await.is_err());
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn vector_type() {
    require_db!();
    let opts = fresh_db("vectortype").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE v (id INT PRIMARY KEY, e VECTOR(3))").await.unwrap();
    let a = Vector(vec![1.0, 2.5, -3.0]);
    query("INSERT INTO v VALUES (1, ?)").bind(&a).execute(&mut conn).await.unwrap();
    let got: Vector = query_scalar("SELECT e FROM v WHERE id = ?").bind(1).fetch_one(&mut conn).await.unwrap();
    assert_eq!(got, a);
    let row = raw_sql("SELECT e FROM v").fetch_one(&mut conn).await.unwrap();
    assert_eq!(row.get::<Vector, _>(0), a);
    // as a function argument
    let d: f64 = query_scalar("SELECT VEC_L2_DISTANCE(e, ?) FROM v").bind(&a).fetch_one(&mut conn).await.unwrap();
    assert_eq!(d, 0.0);
    // NaN cannot be sent
    assert!(query("SELECT ?").bind(Vector(vec![f32::NAN])).execute(&mut conn).await.is_err());
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn wide_integers_and_raw_json() {
    require_db!();
    let opts = fresh_db("wideint").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw(
        "CREATE TABLE w (id INT PRIMARY KEY, d DECIMAL(38, 0), u BIGINT UNSIGNED, f DECIMAL(10, 2), j JSON)",
    )
    .await
    .unwrap();
    let big = 12_345_678_901_234_567_890_123_456_789_012_345_678i128;
    query("INSERT INTO w VALUES (1, ?, ?, 1.50, ?)")
        .bind(big)
        .bind(u64::MAX)
        .bind(serde_json::value::RawValue::from_string(r#"{"a": [1, 2]}"#.into()).unwrap())
        .execute(&mut conn)
        .await
        .unwrap();
    for prepared in [true, false] {
        let sql = "SELECT d, u, f, j FROM w";
        let row =
            if prepared { query(sql).fetch_one(&mut conn).await } else { raw_sql(sql).fetch_one(&mut conn).await }
                .unwrap();
        assert_eq!(row.get::<i128, _>("d"), big);
        assert_eq!(row.get::<u128, _>("u"), u64::MAX as u128);
        assert!(row.try_get::<i128, _>("f").is_err(), "fractional DECIMAL");
        let raw: &serde_json::value::RawValue = row.get("j");
        let v: serde_json::Value = serde_json::from_str(raw.get()).unwrap();
        assert_eq!(v, serde_json::json!({"a": [1, 2]}));
    }
    assert!(query("SELECT ?").bind(-big).fetch_one(&mut conn).await.is_ok());
}

/// A NULL parameter still declares its Rust type's column type (`Option::<T>::None.produces()`);
/// TiDB must ignore it for every pairing, in writes and in comparisons.
#[cfg(all(feature = "chrono", feature = "time", feature = "decimal", feature = "bigdecimal", feature = "uuid"))]
#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn null_of_every_type_into_every_column() {
    require_db!();
    let opts = fresh_db("nulls").await;
    let mut conn = opts.connect().await.unwrap();
    let columns = [
        "BIGINT",
        "DECIMAL(10,2)",
        "DOUBLE",
        "BIT(8)",
        "YEAR",
        "VARCHAR(20)",
        "ENUM('a')",
        "SET('a')",
        "VARBINARY(20)",
        "JSON",
        "DATE",
        "DATETIME(6)",
        "TIMESTAMP(6) NULL",
        "TIME",
    ];
    let defs: Vec<String> = columns.iter().enumerate().map(|(i, c)| format!("c{i} {c}")).collect();
    conn.execute_raw(&format!("CREATE TABLE t (id INT PRIMARY KEY, {})", defs.join(", "))).await.unwrap();
    let mut id = 0;
    macro_rules! nulls {
        ($($t:ty),* $(,)?) => {$({
            for i in 0..columns.len() {
                id += 1;
                let sql = format!("INSERT INTO t (id, c{i}) VALUES (?, ?)");
                query(&sql).bind(id).bind(None::<$t>).execute(&mut conn).await
                    .unwrap_or_else(|e| panic!("None::<{}> into {}: {e}", stringify!($t), columns[i]));
                let sql = format!("SELECT c{i} IS NULL AND (c{i} = ?) IS NULL FROM t WHERE id = ?");
                let ok: bool = query_scalar(&sql).bind(None::<$t>).bind(id).fetch_one(&mut conn).await.unwrap();
                assert!(ok, "None::<{}> vs {}", stringify!($t), columns[i]);
            }
        })*};
    }
    nulls!(
        i8,
        i16,
        i32,
        i64,
        i128,
        u8,
        u16,
        u32,
        u64,
        u128,
        f32,
        f64,
        bool,
        String,
        Vec<u8>,
        [u8; 4],
        rust_decimal::Decimal,
        bigdecimal::BigDecimal,
        serde_json::Value,
        Json<Vec<i32>>,
        Set,
        Vector,
        uuid::Uuid,
        uuid::fmt::Hyphenated,
        chrono::NaiveDate,
        chrono::NaiveDateTime,
        chrono::DateTime<chrono::Utc>,
        chrono::NaiveTime,
        chrono::TimeDelta,
        time::Date,
        time::PrimitiveDateTime,
        time::OffsetDateTime,
        time::Time,
        time::Duration,
        std::time::Duration,
    );
}

/// A conversion that cannot be exact is an error, and FLOAT reads the same through both protocols.
#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn inexact_conversions_are_errors_in_both_protocols() {
    require_db!();
    let opts = fresh_db("inexact").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw(
        "CREATE TABLE t (f FLOAT, d DOUBLE, dm DECIMAL(40, 30));
         INSERT INTO t VALUES (0.1, 1e300, 1.123456789012345678901234567891)",
    )
    .await
    .unwrap();
    for prepared in [true, false] {
        let sql = "SELECT f, d, dm FROM t";
        let row =
            if prepared { query(sql).fetch_one(&mut conn).await } else { raw_sql(sql).fetch_one(&mut conn).await }
                .unwrap();
        assert_eq!(row.get::<f32, _>("f"), 0.1, "prepared={prepared}");
        assert_eq!(row.get::<f64, _>("f"), 0.1f32 as f64, "prepared={prepared}: the stored FLOAT, widened");
        assert!(row.try_get::<f32, _>("d").is_err(), "prepared={prepared}: 1e300 is not an f32");
        assert_eq!(row.get::<f64, _>("dm"), 1.123_456_789_012_345_7, "prepared={prepared}");
        assert!(
            row.try_get::<rust_decimal::Decimal, _>("dm").is_err(),
            "prepared={prepared}: 31 digits must not be rounded to rust_decimal's 28"
        );
    }
}
