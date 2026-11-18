//! Connection pool.

use std::collections::VecDeque;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_core::future::BoxFuture;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::connection::TiConnection;
use crate::error::{Error, Result};
use crate::options::ConnectOptions;
use crate::tidb::{RetryPolicy, Retryable, TxnOptions};
use crate::transaction::Transaction;

type AfterConnect = Arc<dyn Fn(&mut TiConnection) -> BoxFuture<'_, Result<()>> + Send + Sync>;

/// Pool configuration.
#[derive(Clone)]
pub struct PoolOptions {
    max_connections: u32,
    min_connections: u32,
    acquire_timeout: Duration,
    idle_timeout: Option<Duration>,
    max_lifetime: Option<Duration>,
    test_before_acquire: bool,
    after_connect: Option<AfterConnect>,
}

impl fmt::Debug for PoolOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolOptions")
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_lifetime", &self.max_lifetime)
            .field("test_before_acquire", &self.test_before_acquire)
            .finish()
    }
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl PoolOptions {
    pub fn new() -> Self {
        Self {
            max_connections: 10,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(30),
            idle_timeout: Some(Duration::from_secs(10 * 60)),
            // TiDB load balancers commonly cut connections; recycle well before that.
            max_lifetime: Some(Duration::from_secs(30 * 60)),
            test_before_acquire: true,
            after_connect: None,
        }
    }

    pub fn max_connections(mut self, n: u32) -> Self {
        self.max_connections = n.max(1);
        self
    }

    pub fn min_connections(mut self, n: u32) -> Self {
        self.min_connections = n;
        self
    }

    pub fn acquire_timeout(mut self, d: Duration) -> Self {
        self.acquire_timeout = d;
        self
    }

    pub fn idle_timeout(mut self, d: Option<Duration>) -> Self {
        self.idle_timeout = d;
        self
    }

    pub fn max_lifetime(mut self, d: Option<Duration>) -> Self {
        self.max_lifetime = d;
        self
    }

    /// Ping idle connections before handing them out (default `true`).
    pub fn test_before_acquire(mut self, yes: bool) -> Self {
        self.test_before_acquire = yes;
        self
    }

    /// Run a callback on every new connection.
    pub fn after_connect<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut TiConnection) -> BoxFuture<'_, Result<()>> + Send + Sync + 'static,
    {
        self.after_connect = Some(Arc::new(f));
        self
    }

    pub async fn connect(self, url: &str) -> Result<Pool> {
        self.connect_with(url.parse()?).await
    }

    /// Create the pool and open `min_connections` (at least one, to validate the configuration).
    pub async fn connect_with(self, options: ConnectOptions) -> Result<Pool> {
        let pool = self.connect_lazy_with(options);
        let options = &pool.0.options;
        let n = options.min_connections.clamp(1, options.max_connections);
        let conns = futures_util::future::try_join_all((0..n).map(|_| pool.acquire())).await?;
        drop(conns);
        Ok(pool)
    }

    /// Create the pool without opening any connection.
    ///
    /// Inside a tokio runtime this also starts the maintenance task, which closes
    /// connections past `idle_timeout`/`max_lifetime` and reopens up to `min_connections`.
    pub fn connect_lazy_with(self, options: ConnectOptions) -> Pool {
        let inner = Arc::new(PoolInner {
            semaphore: Arc::new(Semaphore::new(self.max_connections as usize)),
            options: self,
            connect_options: options,
            idle: Mutex::new(VecDeque::new()),
            size: Arc::new(AtomicU32::new(0)),
            closed: AtomicBool::new(false),
        });
        if let (Some(period), Ok(handle)) = (inner.maintenance_period(), tokio::runtime::Handle::try_current()) {
            handle.spawn(maintain(Arc::downgrade(&inner), period));
        }
        Pool(inner)
    }
}

struct Live {
    raw: TiConnection,
    created_at: Instant,
    _slot: Slot,
}

/// Counts toward [`Pool::size`] while alive.
struct Slot(Arc<AtomicU32>);

impl Slot {
    fn new(size: &Arc<AtomicU32>) -> Self {
        size.fetch_add(1, Ordering::AcqRel);
        Self(size.clone())
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Idle {
    live: Live,
    since: Instant,
}

struct PoolInner {
    options: PoolOptions,
    connect_options: ConnectOptions,
    semaphore: Arc<Semaphore>,
    idle: Mutex<VecDeque<Idle>>,
    size: Arc<AtomicU32>,
    closed: AtomicBool,
}

/// Periodically retire expired idle connections and top the pool up to
/// `min_connections`. Stops when the pool is dropped or closed.
async fn maintain(pool: std::sync::Weak<PoolInner>, period: Duration) {
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tick.tick().await;
        let Some(inner) = pool.upgrade() else { return };
        if inner.closed.load(Ordering::Acquire) {
            return;
        }
        let expired: Vec<Idle> = {
            let mut idle = inner.idle.lock().unwrap();
            let (keep, expired): (Vec<Idle>, Vec<Idle>) =
                idle.drain(..).partition(|i| !inner.is_idle_expired(i) && !inner.is_expired(&i.live));
            idle.extend(keep);
            expired
        };
        for Idle { live, .. } in expired {
            let Live { raw, .. } = live; // frees the slot before closing
            let _ = tokio::time::timeout(inner.io_timeout(), raw.close()).await;
        }
        while inner.size.load(Ordering::Acquire) < inner.options.min_connections {
            // A permit keeps the total within max_connections while connecting.
            let Ok(permit) = inner.semaphore.clone().try_acquire_owned() else { break };
            match inner.open().await {
                Ok(live) => inner.release(live),
                Err(e) => {
                    log::debug!("pool maintenance could not open a connection: {e}");
                    break;
                }
            }
            drop(permit);
        }
    }
}

impl PoolInner {
    fn is_expired(&self, live: &Live) -> bool {
        self.options.max_lifetime.is_some_and(|max| live.created_at.elapsed() >= max)
    }

    fn is_idle_expired(&self, idle: &Idle) -> bool {
        self.options.idle_timeout.is_some_and(|t| idle.since.elapsed() >= t)
    }

    /// How often the maintenance task runs, if it has anything to do.
    fn maintenance_period(&self) -> Option<Duration> {
        let o = &self.options;
        let shortest = [o.idle_timeout, o.max_lifetime].into_iter().flatten().min();
        match shortest {
            Some(d) => Some((d / 2).clamp(Duration::from_millis(50), Duration::from_secs(30))),
            None if o.min_connections > 0 => Some(Duration::from_secs(30)),
            None => None,
        }
    }

    /// Bound for background I/O (pings, resets, closes): a stuck socket must not hold a permit forever.
    fn io_timeout(&self) -> Duration {
        self.connect_options.connect_timeout
    }

    /// Open a new connection (counted in `size`) and run the `after_connect` hook.
    async fn open(&self) -> Result<Live> {
        let slot = Slot::new(&self.size);
        let mut raw = TiConnection::connect_with(&self.connect_options).await?;
        if let Some(hook) = &self.options.after_connect {
            hook(&mut raw).await?;
        }
        Ok(Live { raw, created_at: Instant::now(), _slot: slot })
    }

    fn release(self: &Arc<Self>, live: Live) {
        if live.raw.is_broken() || self.is_expired(&live) {
            return;
        }
        // Checked under the lock: `Pool::close` drains the queue under it after setting `closed`.
        let mut idle = self.idle.lock().unwrap();
        if !self.closed.load(Ordering::Acquire) {
            idle.push_back(Idle { live, since: Instant::now() });
        }
    }
}

/// A pool of TiDB connections. Cheap to clone.
#[derive(Clone)]
pub struct Pool(Arc<PoolInner>);

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("size", &self.size())
            .field("num_idle", &self.num_idle())
            .field("is_closed", &self.is_closed())
            .field("options", &self.0.options)
            .finish()
    }
}

impl Pool {
    /// Create a pool with default options.
    pub async fn connect(url: &str) -> Result<Self> {
        PoolOptions::new().connect(url).await
    }

    pub async fn connect_with(options: ConnectOptions) -> Result<Self> {
        PoolOptions::new().connect_with(options).await
    }

    pub fn connect_lazy(url: &str) -> Result<Self> {
        Ok(PoolOptions::new().connect_lazy_with(url.parse()?))
    }

    pub fn connect_options(&self) -> &ConnectOptions {
        &self.0.connect_options
    }

    /// Number of open connections (idle + in use).
    pub fn size(&self) -> u32 {
        self.0.size.load(Ordering::Acquire)
    }

    pub fn num_idle(&self) -> usize {
        self.0.idle.lock().unwrap().len()
    }

    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    /// Check out a connection.
    pub async fn acquire(&self) -> Result<PoolConnection> {
        if self.is_closed() {
            return Err(Error::PoolClosed);
        }
        tokio::time::timeout(self.0.options.acquire_timeout, self.acquire_inner())
            .await
            .map_err(|_| Error::PoolTimedOut)?
    }

    async fn acquire_inner(&self) -> Result<PoolConnection> {
        let inner = &self.0;
        let permit = inner.semaphore.clone().acquire_owned().await.map_err(|_| Error::PoolClosed)?;
        loop {
            // Most recently used first (LIFO): surplus connections stay idle long
            // enough for `idle_timeout` to retire them.
            let idle = inner.idle.lock().unwrap().pop_back();
            let Some(idle) = idle else { break };
            let expired = inner.is_idle_expired(&idle) || inner.is_expired(&idle.live);
            let mut live = idle.live;
            if expired {
                // Free the slot now: this acquirer may open a replacement right away.
                let Live { raw, .. } = live;
                tokio::spawn(async move {
                    let _ = raw.close().await;
                });
                continue;
            }
            if inner.options.test_before_acquire {
                match tokio::time::timeout(inner.io_timeout(), live.raw.ping()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        log::debug!("discarding idle connection that failed ping: {e}");
                        continue;
                    }
                    Err(_) => {
                        log::debug!("discarding idle connection: ping timed out");
                        continue;
                    }
                }
            }
            return Ok(PoolConnection { live: Some(live), pool: inner.clone(), permit: Some(permit) });
        }

        // The permit guarantees size < max_connections here.
        let live = inner.open().await?;
        Ok(PoolConnection { live: Some(live), pool: inner.clone(), permit: Some(permit) })
    }

    /// Begin a transaction on a pooled connection.
    pub async fn begin(&self) -> Result<Transaction<'static>> {
        self.acquire().await?.begin_owned(&TxnOptions::default()).await
    }

    /// Begin a transaction with TiDB options (mode, stale read, isolation...).
    pub async fn begin_with(&self, options: &TxnOptions) -> Result<Transaction<'static>> {
        self.acquire().await?.begin_owned(options).await
    }

    /// Run `f` in a transaction and commit, retrying the whole transaction
    /// on TiDB write conflicts / deadlocks with the default [`RetryPolicy`].
    ///
    /// ```ignore
    /// let id = pool.transaction(|tx| Box::pin(async move {
    ///     tisqlx::query("UPDATE accounts SET balance = balance - 10 WHERE id = ?").bind(1).execute(&mut **tx).await?;
    ///     Ok::<_, tisqlx::Error>(())
    /// })).await?;
    /// ```
    pub async fn transaction<T, E, F>(&self, f: F) -> Result<T, E>
    where
        F: for<'t> FnMut(&'t mut Transaction<'static>) -> BoxFuture<'t, Result<T, E>>,
        E: From<Error> + Retryable,
    {
        self.transaction_with(&TxnOptions::default(), RetryPolicy::default(), f).await
    }

    pub async fn transaction_with<T, E, F>(&self, options: &TxnOptions, policy: RetryPolicy, f: F) -> Result<T, E>
    where
        F: for<'t> FnMut(&'t mut Transaction<'static>) -> BoxFuture<'t, Result<T, E>>,
        E: From<Error> + Retryable,
    {
        self.transaction_retry_if(options, policy, E::is_retryable, f).await
    }

    /// Like [`transaction_with`](Self::transaction_with), deciding retries with
    /// `should_retry` instead of the [`Retryable`] trait: for error types tisqlx cannot
    /// implement it for, such as `anyhow::Error`.
    ///
    /// ```ignore
    /// pool.transaction_retry_if(&TxnOptions::optimistic(), RetryPolicy::default(),
    ///     |e: &anyhow::Error| tisqlx::error::is_retryable(e.as_ref()),
    ///     |tx| Box::pin(async move { /* ... */ Ok::<_, anyhow::Error>(()) }),
    /// ).await?;
    /// ```
    pub async fn transaction_retry_if<T, E, F, P>(
        &self,
        options: &TxnOptions,
        policy: RetryPolicy,
        should_retry: P,
        mut f: F,
    ) -> Result<T, E>
    where
        F: for<'t> FnMut(&'t mut Transaction<'static>) -> BoxFuture<'t, Result<T, E>>,
        E: From<Error>,
        P: Fn(&E) -> bool,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = async {
                let mut tx = self.begin_with(options).await?;
                match f(&mut tx).await {
                    Ok(value) => {
                        tx.commit().await?;
                        Ok::<T, E>(value)
                    }
                    Err(e) => {
                        // Release this attempt's locks before retrying, rather than in the
                        // background after the connection is dropped.
                        let _ = tx.rollback().await;
                        Err(e)
                    }
                }
            }
            .await;
            match result {
                Err(e) if attempt < policy.max_attempts && should_retry(&e) => {
                    let delay = policy.backoff(attempt);
                    log::debug!("retrying transaction after retryable error (attempt {attempt}), backoff {delay:?}");
                    tokio::time::sleep(delay).await;
                }
                other => return other,
            }
        }
    }

    /// Close every idle connection and refuse new acquisitions. Connections in
    /// use are closed when returned.
    pub async fn close(&self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.semaphore.close();
        let idle: Vec<Idle> = self.0.idle.lock().unwrap().drain(..).collect();
        let timeout = self.0.io_timeout();
        for Idle { live, .. } in idle {
            let Live { raw, .. } = live;
            let _ = tokio::time::timeout(timeout, raw.close()).await;
        }
    }
}

/// A connection checked out of a [`Pool`]; returned to it on drop.
pub struct PoolConnection {
    live: Option<Live>,
    pool: Arc<PoolInner>,
    permit: Option<OwnedSemaphorePermit>,
}

impl fmt::Debug for PoolConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PoolConnection").field(&self.live.as_ref().map(|l| &l.raw)).finish()
    }
}

impl PoolConnection {
    /// Remove the connection from the pool permanently.
    pub fn detach(mut self) -> TiConnection {
        self.live.take().expect("connection already taken").raw
    }

    /// Close this connection instead of returning it to the pool.
    pub async fn close(mut self) -> Result<()> {
        let live = self.live.take().expect("connection already taken");
        live.raw.close().await
    }
}

impl Deref for PoolConnection {
    type Target = TiConnection;

    fn deref(&self) -> &TiConnection {
        &self.live.as_ref().expect("connection already taken").raw
    }
}

impl DerefMut for PoolConnection {
    fn deref_mut(&mut self) -> &mut TiConnection {
        &mut self.live.as_mut().expect("connection already taken").raw
    }
}

impl Drop for PoolConnection {
    fn drop(&mut self) {
        let Some(mut live) = self.live.take() else { return };
        let pool = self.pool.clone();
        let permit = self.permit.take();
        if !live.raw.needs_reset() || live.raw.is_broken() {
            pool.release(live);
            return;
        }
        // Roll back eagerly: an open pessimistic transaction holds its locks.
        // Without a runtime the connection is simply discarded.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                match tokio::time::timeout(pool.io_timeout(), live.raw.reset_session()).await {
                    Ok(Ok(())) => pool.release(live),
                    Ok(Err(e)) => log::debug!("discarding connection that failed to reset: {e}"),
                    Err(_) => log::debug!("discarding connection: reset timed out"),
                }
                drop(permit);
            });
        }
    }
}
