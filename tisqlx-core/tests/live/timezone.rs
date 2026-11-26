//! Instant types (`DateTime<Utc>`, `DateTime<Local>`, ...) in a session whose time zone is not UTC.

use std::time::Duration;

use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use tisqlx_core::pool::PoolOptions;
use tisqlx_core::query::{query, query_scalar, raw_sql};
use tisqlx_core::*;

use crate::common::*;

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap() + chrono::TimeDelta::microseconds(123_456)
}

fn wall(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_hms_micro_opt(h, mi, 0, 123_456).unwrap()
}

/// `NOW()` decoded as an instant matches the client clock (only if the zone is converted right).
async fn assert_now_is_correct(conn: &mut TiConnection) {
    let now: DateTime<Utc> = query_scalar("SELECT NOW(6)").fetch_one(&mut *conn).await.unwrap();
    let skew = (now - Utc::now()).num_seconds().abs();
    assert!(skew < 60, "NOW() decoded {skew}s away from the client clock: {now}");
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn instants_round_trip_in_a_non_utc_session() {
    require_db!();
    let opts = fresh_db("tz").await.timezone(Some("Europe/Paris"));
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE t (id INT PRIMARY KEY, dt DATETIME(6), ts TIMESTAMP(6) NULL)").await.unwrap();
    // summer (+02:00) and winter (+01:00)
    let cases = [
        (1, utc(2026, 7, 1, 12, 0), wall(2026, 7, 1, 14, 0), 7200),
        (2, utc(2026, 1, 15, 23, 30), wall(2026, 1, 16, 0, 30), 3600),
    ];
    for (id, at, _, _) in cases {
        query("INSERT INTO t VALUES (?, ?, ?)").bind(id).bind(at).bind(at).execute(&mut conn).await.unwrap();
    }
    for (id, at, local, offset) in cases {
        // The server received session wall times, and TIMESTAMP stored the right instant.
        let row = query("SELECT dt, ts, UNIX_TIMESTAMP(ts) AS epoch FROM t WHERE id = ?")
            .bind(id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(row.get::<NaiveDateTime, _>("dt"), local);
        assert_eq!(row.get::<NaiveDateTime, _>("ts"), local);
        let epoch: rust_decimal::Decimal = row.get("epoch");
        assert_eq!(epoch.trunc().to_string(), at.timestamp().to_string());
        // Both column types decode back to the same instant, in every instant type and both protocols.
        let text = raw_sql(format!("SELECT dt, ts FROM t WHERE id = {id}")).fetch_one(&mut conn).await.unwrap();
        for r in [&row, &text] {
            for col in ["dt", "ts"] {
                assert_eq!(r.get::<DateTime<Utc>, _>(col), at, "{col}");
                let fixed = r.get::<DateTime<FixedOffset>, _>(col);
                assert_eq!((fixed, fixed.offset().local_minus_utc()), (at.fixed_offset(), offset), "{col}");
                assert_eq!(r.get::<DateTime<Local>, _>(col), at, "{col}");
            }
        }
        // A bound instant is compared in the session zone too.
        let n: i64 = query_scalar("SELECT COUNT(*) FROM t WHERE ts = ? AND dt = ?")
            .bind(at)
            .bind(at)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let fixed = at.with_timezone(&FixedOffset::east_opt(offset).unwrap());
        let n: i64 =
            query_scalar("SELECT COUNT(*) FROM t WHERE ts = ?").bind(fixed).fetch_one(&mut conn).await.unwrap();
        assert_eq!(n, 1);
    }
    let echoed: DateTime<Utc> = query_scalar("SELECT ?").bind(cases[0].1).fetch_one(&mut conn).await.unwrap();
    assert_eq!(echoed, cases[0].1);
    assert_now_is_correct(&mut conn).await;
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn daylight_saving_wall_times_decode_to_the_instant_tidb_stores() {
    // Around DST changes a wall time can be ambiguous or not exist; whatever TiDB makes of it
    // (`UNIX_TIMESTAMP`), decoding must agree, for DATETIME values and stored TIMESTAMPs alike.
    require_db!();
    let opts = fresh_db("tzdst").await;
    for (zone, days) in [("Europe/Paris", [(3, 29), (10, 25)]), ("America/New_York", [(3, 8), (11, 1)])] {
        let mut conn = opts.clone().timezone(Some(zone)).connect().await.unwrap();
        conn.execute_raw("DROP TABLE IF EXISTS t; CREATE TABLE t (dt DATETIME, ts TIMESTAMP NULL)").await.unwrap();
        for (month, day) in days {
            for minutes in (0..5 * 60).step_by(10) {
                let wall = NaiveDate::from_ymd_opt(2026, month, day)
                    .unwrap()
                    .and_hms_opt(minutes / 60, minutes % 60, 0)
                    .unwrap();
                query("INSERT INTO t VALUES (?, ?)").bind(wall).bind(wall).execute(&mut conn).await.unwrap();
            }
        }
        let rows = query("SELECT dt, ts, UNIX_TIMESTAMP(dt) AS dt_epoch, UNIX_TIMESTAMP(ts) AS ts_epoch FROM t")
            .fetch_all(&mut conn)
            .await
            .unwrap();
        assert_eq!(rows.len(), 60);
        for r in &rows {
            let dt: NaiveDateTime = r.get("dt");
            assert_eq!(
                r.get::<DateTime<Utc>, _>("dt").timestamp(),
                r.get::<i64, _>("dt_epoch"),
                "{zone} DATETIME {dt}"
            );
            assert_eq!(
                r.get::<DateTime<Utc>, _>("ts").timestamp(),
                r.get::<i64, _>("ts_epoch"),
                "{zone} TIMESTAMP from {dt}"
            );
        }
    }
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn session_zone_follows_the_server_hooks_and_reloads() {
    require_db!();
    let opts = fresh_db("tzserver").await;
    // `timezone=server`: whatever the server runs in (`SYSTEM` -> @@system_time_zone).
    let mut conn = opts.clone().timezone(None).connect().await.unwrap();
    assert_now_is_correct(&mut conn).await;
    // A raw SET is not reported by TiDB; `reload_time_zone` picks it up.
    conn.execute_raw("SET time_zone = '-07:00'").await.unwrap();
    conn.reload_time_zone().await.unwrap();
    assert_now_is_correct(&mut conn).await;
    // A pool hook that changes the zone is followed.
    let pool = PoolOptions::new()
        .max_connections(1)
        .after_connect(|c| Box::pin(async move { c.execute_raw("SET time_zone = 'Asia/Shanghai'").await.map(drop) }))
        .connect_with(opts)
        .await
        .unwrap();
    let mut c = pool.acquire().await.unwrap();
    assert_now_is_correct(&mut c).await;
    tokio::time::timeout(Duration::from_secs(5), pool.close()).await.unwrap();
}
