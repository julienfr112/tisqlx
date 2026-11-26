//! RawKV against a real PD + TiKV: `make db-rawkv test-rawkv`.
//!
//! The tests read the PD address from `TISQLX_TEST_RAWKV_PD`; without it they skip (or fail,
//! with `TISQLX_TEST_REQUIRE_RAWKV=1`). Each test works under its own key prefix and deletes
//! it at the end, so they run in parallel on one cluster. The cluster has
//! `storage.enable-ttl` (see `scripts/tidb.sh rawkv-up`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tisqlx_rawkv::{ColumnFamily, Config, Error, KvPair, MAX_SCAN_LIMIT, RawClient};

async fn client() -> Option<RawClient> {
    let pd = match std::env::var("TISQLX_TEST_RAWKV_PD") {
        Ok(pd) => pd,
        Err(_) => {
            assert!(std::env::var_os("TISQLX_TEST_REQUIRE_RAWKV").is_none(), "TISQLX_TEST_RAWKV_PD is not set");
            return None;
        }
    };
    Some(RawClient::connect(pd.split(',').map(str::to_owned)).await.expect("connect to PD"))
}

macro_rules! require_rawkv {
    () => {
        match client().await {
            Some(c) => c,
            None => return,
        }
    };
}

/// A key prefix no other test (or earlier run) uses.
fn prefix(test: &str) -> Vec<u8> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("tisqlx-test/{test}/{nanos}/").into_bytes()
}

fn key(prefix: &[u8], suffix: impl AsRef<[u8]>) -> Vec<u8> {
    [prefix, suffix.as_ref()].concat()
}

/// The end of the range of every key starting with `prefix`.
fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    *end.last_mut().unwrap() += 1; // prefixes end with '/'
    end
}

async fn cleanup(kv: &RawClient, p: &[u8]) {
    kv.delete_range(p.to_vec(), prefix_end(p)).await.unwrap();
    assert!(kv.scan(p.to_vec(), prefix_end(p), 10).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "needs PD + TiKV: run with `make test-rawkv`"]
async fn put_get_delete() {
    let kv = require_rawkv!();
    let p = prefix("single");
    let k = key(&p, "a");
    assert_eq!(kv.get(k.clone()).await.unwrap(), None);
    kv.put(k.clone(), "1").await.unwrap();
    assert_eq!(kv.get(k.clone()).await.unwrap(), Some(b"1".to_vec()));
    kv.put(k.clone(), vec![0u8, 255, 0]).await.unwrap();
    assert_eq!(kv.get(k.clone()).await.unwrap(), Some(vec![0u8, 255, 0]), "binary values, overwritten");
    let big = vec![7u8; 3 << 20];
    kv.put(key(&p, "big"), big.clone()).await.unwrap();
    assert_eq!(
        kv.get(key(&p, "big")).await.unwrap(),
        Some(big),
        "values over tonic's 4 MiB default decode limit read back"
    );
    kv.delete(k.clone()).await.unwrap();
    assert_eq!(kv.get(k.clone()).await.unwrap(), None);
    kv.delete(k).await.unwrap(); // deleting a missing key is fine
    cleanup(&kv, &p).await;
}

#[tokio::test]
#[ignore = "needs PD + TiKV: run with `make test-rawkv`"]
async fn batches_and_scans() {
    let kv = require_rawkv!();
    let p = prefix("batch");
    let pairs: Vec<KvPair> = (0..300).map(|i| KvPair::new(key(&p, format!("k{i:04}")), format!("v{i}"))).collect();
    kv.batch_put(pairs.clone()).await.unwrap();

    let got = kv.batch_get([key(&p, "k0007"), key(&p, "missing"), key(&p, "k0001"), key(&p, "k0007")]).await.unwrap();
    assert_eq!(got, vec![pairs[1].clone(), pairs[7].clone()], "found keys only, sorted, once");

    let all = kv.scan(p.clone(), prefix_end(&p), MAX_SCAN_LIMIT).await.unwrap();
    assert_eq!(all, pairs);
    let page = kv.scan(key(&p, "k0010"), key(&p, "k0020"), 5).await.unwrap();
    assert_eq!(page, pairs[10..15]);
    let keys = kv.scan_keys(key(&p, "k0295"), prefix_end(&p), 100).await.unwrap();
    assert_eq!(keys, pairs[295..].iter().map(|p| p.key.clone()).collect::<Vec<_>>());
    assert!(kv.scan(key(&p, "z"), key(&p, "a"), 10).await.unwrap().is_empty(), "empty range");
    let err = kv.scan(p.clone(), prefix_end(&p), MAX_SCAN_LIMIT + 1).await.unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");

    kv.batch_delete((0..300).step_by(2).map(|i| key(&p, format!("k{i:04}")))).await.unwrap();
    let left = kv.scan(p.clone(), prefix_end(&p), MAX_SCAN_LIMIT).await.unwrap();
    assert_eq!(left, pairs.iter().skip(1).step_by(2).cloned().collect::<Vec<_>>());

    kv.delete_range(key(&p, "k0100"), key(&p, "k0200")).await.unwrap();
    assert_eq!(kv.scan(p.clone(), prefix_end(&p), MAX_SCAN_LIMIT).await.unwrap().len(), 100);
    let err = kv.delete_range(p.clone(), Vec::new()).await.unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "unbounded delete_range refused: {err}");
    cleanup(&kv, &p).await;
}

#[tokio::test]
#[ignore = "needs PD + TiKV: run with `make test-rawkv`"]
async fn large_batch_is_chunked() {
    let kv = require_rawkv!();
    let p = prefix("large");
    // 6000 pairs of 1 KiB: several requests by count and by size.
    let pairs: Vec<KvPair> = (0..6000).map(|i| KvPair::new(key(&p, format!("{i:05}")), vec![b'x'; 1024])).collect();
    kv.batch_put(pairs.clone()).await.unwrap();
    let got = kv.batch_get(pairs.iter().map(|p| p.key.clone())).await.unwrap();
    assert_eq!(got.len(), 6000);
    assert_eq!(kv.scan_keys(p.clone(), prefix_end(&p), MAX_SCAN_LIMIT).await.unwrap().len(), 6000);
    cleanup(&kv, &p).await;
}

/// Operations spanning several regions, from a client whose cached regions predate the split.
#[tokio::test]
#[ignore = "needs PD + TiKV: run with `make test-rawkv`"]
async fn across_regions_and_a_stale_cache() {
    let kv = require_rawkv!();
    let stale = require_rawkv!();
    let p = prefix("regions");
    let pairs: Vec<KvPair> = (0..200).map(|i| KvPair::new(key(&p, format!("k{i:03}")), format!("{i}"))).collect();
    kv.batch_put(pairs.clone()).await.unwrap();
    // `stale` caches the region holding the whole prefix.
    assert_eq!(stale.get(key(&p, "k000")).await.unwrap(), Some(b"0".to_vec()));

    let splits: Vec<Vec<u8>> = ["k050", "k100", "k150"].iter().map(|s| key(&p, s)).collect();
    kv.split_regions(splits.clone()).await.unwrap();
    kv.split_regions(splits).await.unwrap(); // splitting at existing boundaries is a no-op

    // `stale` still routes everything to the old region: TiKV answers EpochNotMatch, and the
    // client must recover without the caller noticing.
    assert_eq!(stale.scan(p.clone(), prefix_end(&p), 1000).await.unwrap(), pairs, "scan over four regions");
    assert_eq!(stale.scan(key(&p, "k040"), key(&p, "k160"), 70).await.unwrap(), pairs[40..110], "limit across regions");
    let some: Vec<Vec<u8>> = (0..200).step_by(7).map(|i| key(&p, format!("k{i:03}"))).collect();
    assert_eq!(stale.batch_get(some.clone()).await.unwrap().len(), some.len());
    stale.batch_put((0..200).map(|i| (key(&p, format!("k{i:03}")), "new"))).await.unwrap();
    assert!(kv.scan(p.clone(), prefix_end(&p), 1000).await.unwrap().iter().all(|p| p.value == b"new"));
    stale.delete_range(key(&p, "k025"), key(&p, "k175")).await.unwrap();
    assert_eq!(kv.scan_keys(p.clone(), prefix_end(&p), 1000).await.unwrap().len(), 50);
    cleanup(&kv, &p).await;
}

#[tokio::test]
#[ignore = "needs PD + TiKV: run with `make test-rawkv`"]
async fn ttl() {
    let kv = require_rawkv!();
    let p = prefix("ttl");
    kv.put_with_ttl(key(&p, "short"), "x", 1).await.unwrap();
    kv.put(key(&p, "forever"), "y").await.unwrap();
    kv.batch_put_with_ttl([(key(&p, "b1"), "1"), (key(&p, "b2"), "2")], 1).await.unwrap();
    let ttl = kv.get_key_ttl_secs(key(&p, "short")).await.unwrap();
    assert!(matches!(ttl, Some(1) | Some(0)), "{ttl:?}");
    assert_eq!(kv.get_key_ttl_secs(key(&p, "forever")).await.unwrap(), Some(0), "no TTL");
    assert_eq!(kv.get_key_ttl_secs(key(&p, "missing")).await.unwrap(), None);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(kv.get(key(&p, "short")).await.unwrap(), None, "expired");
    assert_eq!(kv.batch_get([key(&p, "b1"), key(&p, "b2")]).await.unwrap(), vec![], "batch expired");
    assert_eq!(kv.get(key(&p, "forever")).await.unwrap(), Some(b"y".to_vec()));
    cleanup(&kv, &p).await;
}

#[tokio::test]
#[ignore = "needs PD + TiKV: run with `make test-rawkv`"]
async fn compare_and_swap() {
    let kv = require_rawkv!();
    let p = prefix("cas");
    let k = key(&p, "c");
    let err = kv.compare_and_swap(k.clone(), None, "1").await.unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "not in atomic mode: {err}");

    let atomic = kv.with_atomic_for_cas();
    let r = atomic.compare_and_swap(k.clone(), None, "1").await.unwrap();
    assert!(r.swapped);
    assert_eq!(r.previous, None);
    let r = atomic.compare_and_swap(k.clone(), None, "2").await.unwrap();
    assert!(!r.swapped, "the key exists");
    assert_eq!(r.previous, Some(b"1".to_vec()));
    let r = atomic.compare_and_swap(k.clone(), Some(b"0".to_vec()), "2").await.unwrap();
    assert!(!r.swapped);
    let r = atomic.compare_and_swap(k.clone(), Some(b"1".to_vec()), "2").await.unwrap();
    assert!(r.swapped);
    assert_eq!(atomic.get(k.clone()).await.unwrap(), Some(b"2".to_vec()));

    // A counter incremented by concurrent CAS loops loses no update.
    let ctr = key(&p, "counter");
    atomic.put(ctr.clone(), "0").await.unwrap();
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let (kv, ctr) = (atomic.clone(), ctr.clone());
            tokio::spawn(async move {
                for _ in 0..10 {
                    loop {
                        let cur = kv.get(ctr.clone()).await.unwrap().unwrap();
                        let n: u32 = String::from_utf8(cur.clone()).unwrap().parse().unwrap();
                        if kv.compare_and_swap(ctr.clone(), Some(cur), (n + 1).to_string()).await.unwrap().swapped {
                            break;
                        }
                    }
                }
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(atomic.get(ctr).await.unwrap(), Some(b"80".to_vec()));
    let err = atomic.delete_range(p.clone(), prefix_end(&p)).await.unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
    atomic.batch_delete([k, key(&p, "counter")]).await.unwrap();
    cleanup(&kv, &p).await;
}

#[tokio::test]
#[ignore = "needs PD + TiKV: run with `make test-rawkv`"]
async fn column_families_are_separate() {
    let kv = require_rawkv!();
    let p = prefix("cf");
    let k = key(&p, "a");
    let write = kv.with_cf(ColumnFamily::Write);
    write.put(k.clone(), "in write").await.unwrap();
    assert_eq!(kv.get(k.clone()).await.unwrap(), None);
    assert_eq!(write.get(k.clone()).await.unwrap(), Some(b"in write".to_vec()));
    write.delete_range(p.clone(), prefix_end(&p)).await.unwrap();
    assert_eq!(write.get(k).await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs PD + TiKV: run with `make test-rawkv`"]
async fn unreachable_pd() {
    if client().await.is_none() {
        return;
    }
    let config = Config {
        connect_timeout: Duration::from_millis(300),
        timeout: Duration::from_millis(500),
        ..Config::default()
    };
    // Port 1 on loopback: refused at once.
    let err = RawClient::connect_with(["127.0.0.1:1"], config).await.unwrap_err();
    assert!(matches!(err, Error::NoPdLeader { .. }), "{err}");
    assert!(err.is_transient());
}
