//! Which TiDB server a pool opens its next connection on (see [`ConnectOptions::hosts`]).
//!
//! Least connections first, rotating between ties. A server that failed to connect is put in
//! backoff (1 s, doubling up to 30 s) and only tried after the others; when every server is in
//! backoff, the one whose backoff ends first is tried first.
//!
//! [`ConnectOptions::hosts`]: crate::ConnectOptions::hosts

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::connection::Target;

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub(crate) struct Balancer {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    servers: Vec<Server>,
    rotation: usize,
}

#[derive(Debug)]
struct Server {
    target: Target,
    live: Arc<AtomicU32>,
    failures: u32,
    retry_at: Option<Instant>,
}

/// Counts one open connection on its server; uncounted when dropped, however the connection ends.
#[derive(Debug)]
pub(crate) struct ServerSlot(Arc<AtomicU32>);

impl Drop for ServerSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Balancer {
    /// `targets` (the servers as currently configured or resolved), in the order to try them.
    pub fn plan(&self, targets: Vec<Target>) -> Vec<Target> {
        self.plan_at(targets, Instant::now())
    }

    fn plan_at(&self, targets: Vec<Target>, now: Instant) -> Vec<Target> {
        let mut state = self.state.lock().unwrap();
        // Forget servers that are gone (DNS answers change), once their connections are closed.
        state.servers.retain(|s| s.live.load(Ordering::Acquire) > 0 || targets.contains(&s.target));
        let rotation = state.rotation;
        state.rotation = state.rotation.wrapping_add(1);
        let n = targets.len().max(1);
        let mut keyed: Vec<_> = targets
            .into_iter()
            .enumerate()
            .map(|(i, target)| {
                let server = state.server(&target);
                let backoff = server.retry_at.filter(|&t| t > now);
                let live = if backoff.is_some() { 0 } else { server.live.load(Ordering::Acquire) };
                ((backoff.is_some(), live, backoff, (i + n - rotation % n) % n), target)
            })
            .collect();
        keyed.sort_by_key(|(key, _)| *key);
        keyed.into_iter().map(|(_, t)| t).collect()
    }

    /// The outcome of a connection attempt to `target`.
    pub fn report(&self, target: &Target, ok: bool) {
        self.report_at(target, ok, Instant::now());
    }

    fn report_at(&self, target: &Target, ok: bool, now: Instant) {
        let mut state = self.state.lock().unwrap();
        let server = state.server(target);
        if ok {
            server.failures = 0;
            server.retry_at = None;
        } else {
            server.failures += 1;
            let backoff = FIRST_BACKOFF.saturating_mul(1 << (server.failures - 1).min(5)).min(MAX_BACKOFF);
            server.retry_at = Some(now + backoff);
        }
    }

    /// Count a new connection on `target`.
    pub fn slot(&self, target: &Target) -> ServerSlot {
        let mut state = self.state.lock().unwrap();
        let live = state.server(target).live.clone();
        live.fetch_add(1, Ordering::AcqRel);
        ServerSlot(live)
    }

    /// Open connections per server.
    pub fn counts(&self) -> Vec<(String, u32)> {
        let state = self.state.lock().unwrap();
        state.servers.iter().map(|s| (s.target.to_string(), s.live.load(Ordering::Acquire))).collect()
    }
}

impl State {
    fn server(&mut self, target: &Target) -> &mut Server {
        let i = match self.servers.iter().position(|s| s.target == *target) {
            Some(i) => i,
            None => {
                let target = target.clone();
                self.servers.push(Server { target, live: Arc::default(), failures: 0, retry_at: None });
                self.servers.len() - 1
            }
        };
        &mut self.servers[i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(host: &str) -> Target {
        Target { host: host.into(), port: 4000, addr: None }
    }

    fn hosts(plan: &[Target]) -> Vec<&str> {
        plan.iter().map(|t| t.host.as_str()).collect()
    }

    #[test]
    fn least_connections_then_rotation() {
        let b = Balancer::default();
        let all = || vec![t("a"), t("b"), t("c")];
        // ties rotate
        assert_eq!(hosts(&b.plan(all()))[0], "a");
        assert_eq!(hosts(&b.plan(all()))[0], "b");
        // fewest connections first
        let _a = b.slot(&t("a"));
        let _b = (b.slot(&t("b")), b.slot(&t("b")));
        let _c = b.slot(&t("c"));
        let first: Vec<_> = (0..4).map(|_| hosts(&b.plan(all()))[0].to_owned()).collect();
        assert!(first.iter().all(|h| h != "b"), "{first:?}");
        drop(_b);
        assert_eq!(b.counts().iter().map(|(_, n)| *n).sum::<u32>(), 2, "slots uncount on drop");
    }

    #[test]
    fn servers_no_longer_listed_are_forgotten_once_empty() {
        let b = Balancer::default();
        let names = |b: &Balancer| b.counts().into_iter().map(|(name, _)| name).collect::<Vec<_>>();
        b.plan(vec![t("a"), t("b")]);
        let on_a = b.slot(&t("a"));
        b.report(&t("b"), false);
        // the list changes: `b` goes at once, `a` stays while it has a connection
        b.plan(vec![t("c")]);
        assert_eq!(names(&b), ["a:4000", "c:4000"]);
        drop(on_a);
        b.plan(vec![t("c")]);
        assert_eq!(names(&b), ["c:4000"]);
    }

    #[test]
    fn failed_servers_back_off() {
        let b = Balancer::default();
        let now = Instant::now();
        let all = || vec![t("a"), t("b")];
        b.report_at(&t("a"), false, now);
        // in backoff: tried last, even with fewer connections
        let _b = b.slot(&t("b"));
        assert_eq!(hosts(&b.plan_at(all(), now)), ["b", "a"]);
        assert_eq!(hosts(&b.plan_at(all(), now + Duration::from_millis(500))), ["b", "a"]);
        // backoff over (1 s): back in the normal order, fewest connections first
        assert_eq!(hosts(&b.plan_at(all(), now + Duration::from_secs(1))), ["a", "b"]);
        // repeated failures double the backoff, up to 30 s
        for _ in 0..10 {
            b.report_at(&t("a"), false, now);
        }
        assert_eq!(hosts(&b.plan_at(all(), now + Duration::from_secs(29))), ["b", "a"]);
        assert_eq!(hosts(&b.plan_at(all(), now + Duration::from_secs(30))), ["a", "b"]);
        // everyone in backoff: the one whose backoff ends first is tried first
        b.report_at(&t("a"), true, now);
        b.report_at(&t("a"), false, now + Duration::from_millis(200));
        b.report_at(&t("b"), false, now);
        assert_eq!(hosts(&b.plan_at(all(), now + Duration::from_millis(300))), ["b", "a"]);
    }
}
