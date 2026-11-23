//! One control thread, one private DB connection, one scalar progress permit per
//! owned claim. No scan futures/history/payloads enter the supervisor. Explicit
//! stop joins it; Drop signals bounded fallback cleanup on cancellation/panic.
use super::*;
use std::sync::atomic::{AtomicU8, AtomicU64};
use tokio::sync::watch;

#[derive(Clone, Copy)]
pub(super) struct Policy {
    pub interval: Duration,
    pub progress_timeout: Duration,
    pub io_timeout: Duration,
    pub lease_margin: Duration,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            progress_timeout: Duration::from_secs(120),
            io_timeout: Duration::from_secs(3),
            lease_margin: Duration::from_secs(2),
        }
    }
}
struct Shared {
    started: Instant,
    last_progress_ms: AtomicU64,
    state: AtomicU8,
    progress_timeout: Duration,
    safe_until_ms: AtomicU64,
}
#[derive(Clone)]
pub(super) struct Permit(Arc<Shared>);
impl Permit {
    fn new(policy: Policy, valid_until: Instant) -> Self {
        let started = Instant::now();
        Self(Arc::new(Shared {
            started,
            last_progress_ms: AtomicU64::new(0),
            state: AtomicU8::new(0),
            progress_timeout: policy.progress_timeout,
            safe_until_ms: AtomicU64::new(
                valid_until
                    .saturating_duration_since(started)
                    .saturating_sub(policy.lease_margin)
                    .as_millis() as u64,
            ),
        }))
    }
    fn outcome(&self) -> Option<WarmupControlOutcome> {
        match self.0.state.load(Ordering::Acquire) {
            0 if self.safe_remaining().is_zero() => Some(WarmupControlOutcome::RenewalUnavailable),
            0 if self
                .0
                .started
                .elapsed()
                .saturating_sub(Duration::from_millis(
                    self.0.last_progress_ms.load(Ordering::Acquire),
                ))
                < self.0.progress_timeout =>
            {
                None
            }
            0 | 3 => Some(WarmupControlOutcome::ProgressBudgetExpired),
            1 => Some(WarmupControlOutcome::RenewalRejected),
            2 => Some(WarmupControlOutcome::RenewalUnavailable),
            _ => Some(WarmupControlOutcome::SupervisorStopped),
        }
    }
    pub fn current(&self) -> bool {
        self.outcome().is_none()
    }
    fn safe_remaining(&self) -> Duration {
        Duration::from_millis(self.0.safe_until_ms.load(Ordering::Acquire))
            .saturating_sub(self.0.started.elapsed())
    }
    fn renewed(&self, valid_until: Instant, policy: Policy) {
        // An acknowledgement arriving after the last confirmed horizon does
        // not resurrect ownership, even if the server committed the renewal.
        if let Some(reason) = self.outcome() {
            self.lose(reason);
            return;
        }
        self.0.safe_until_ms.store(
            valid_until
                .saturating_duration_since(self.0.started)
                .saturating_sub(policy.lease_margin)
                .as_millis() as u64,
            Ordering::Release,
        );
    }
    pub fn check(&self) -> Result<(), WorkerError> {
        self.outcome()
            .map_or(Ok(()), |reason| Err(WorkerError::WarmupControl(reason)))
    }
    /// Only an actual candidate cursor advancement earns another finite window.
    /// Completing stale work after the deadline cannot resurrect the permit.
    pub fn progress(&self) {
        if self.current() {
            self.0.last_progress_ms.store(
                self.0.started.elapsed().as_millis() as u64,
                Ordering::Release,
            );
        }
    }
    pub(super) fn lose(&self, reason: WarmupControlOutcome) {
        let state = match reason {
            WarmupControlOutcome::RenewalRejected => 1,
            WarmupControlOutcome::RenewalUnavailable => 2,
            WarmupControlOutcome::ProgressBudgetExpired => 3,
            WarmupControlOutcome::SupervisorStopped => 4,
        };
        let _ = self
            .0
            .state
            .compare_exchange(0, state, Ordering::AcqRel, Ordering::Acquire);
    }
}
#[allow(async_fn_in_trait)]
trait Heartbeat: Send + 'static {
    async fn renew(&self) -> Result<Option<Instant>, setups_db::DbError>;
    async fn close(&self) {}
}
struct PgHeartbeat {
    pool: PgPool,
    lease: live::WarmupLease,
}
impl PgHeartbeat {
    fn new(options: sqlx::postgres::PgConnectOptions, lease: live::WarmupLease) -> Self {
        let options = options.application_name("setups-live-warmup-supervisor")
            .options([("statement_timeout", "750ms"), ("lock_timeout", "500ms")]);
        Self {
            pool: sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(2))
                .connect_lazy_with(options),
            lease,
        }
    }
}
impl Heartbeat for PgHeartbeat {
    async fn renew(&self) -> Result<Option<Instant>, setups_db::DbError> {
        live::renew_pending_warmup_until(&self.pool, &self.lease).await
    }
    async fn close(&self) {
        self.pool.close().await;
    }
}
pub(super) struct Supervisor {
    permit: Permit,
    stop: watch::Sender<bool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Supervisor {
    #[cfg(test)]
    pub(super) fn fixture(lease: &live::WarmupLease) -> Self {
        struct Healthy;
        impl Heartbeat for Healthy {
            async fn renew(&self) -> Result<Option<Instant>, setups_db::DbError> {
                Ok(Some(Instant::now() + Duration::from_secs(30)))
            }
        }
        Self::spawn(|| Healthy, lease.clone(), Policy::default()).unwrap()
    }
    pub fn start(pool: &PgPool, lease: &live::WarmupLease) -> Result<Self, WorkerError> {
        let options = pool.connect_options().as_ref().clone();
        let fence = lease.clone();
        Self::spawn(
            move || PgHeartbeat::new(options, fence),
            lease.clone(),
            Policy::default(),
        )
    }
    fn spawn<B: Heartbeat>(
        factory: impl FnOnce() -> B + Send + 'static,
        lease: live::WarmupLease,
        policy: Policy,
    ) -> Result<Self, WorkerError> {
        let permit = Permit::new(policy, lease.lease_valid_until);
        let control = permit.clone();
        let (stop, receiver) = watch::channel(false);
        let thread = std::thread::Builder::new()
            .name("adaptive-warmup-control".into())
            .stack_size(1024 * 1024)
            .spawn(move || {
                struct Exit(Permit);
                impl Drop for Exit {
                    fn drop(&mut self) {
                        self.0.lose(WarmupControlOutcome::SupervisorStopped);
                    }
                }
                let _exit = Exit(control.clone());
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .max_blocking_threads(1)
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        control.lose(WarmupControlOutcome::RenewalUnavailable);
                        return;
                    }
                };
                runtime.block_on(async move {
                    let backend = factory();
                    run(&backend, &lease, &control, receiver, policy).await;
                });
                runtime.shutdown_timeout(Duration::from_secs(1));
            })
            .map_err(|_| invalid("cannot start bounded warmup supervisor"))?;
        Ok(Self {
            permit,
            stop,
            thread: Some(thread),
        })
    }
    pub fn permit(&self) -> &Permit {
        &self.permit
    }
    pub async fn stop(&mut self) -> Result<(), WorkerError> {
        self.permit.lose(WarmupControlOutcome::SupervisorStopped);
        let _ = self.stop.send(true);
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || thread.join())
                .await
                .map_err(|_| WorkerError::WarmupControl(WarmupControlOutcome::SupervisorStopped))?
                .map_err(|_| WorkerError::WarmupControl(WarmupControlOutcome::SupervisorStopped))?;
        }
        Ok(())
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.permit.lose(WarmupControlOutcome::SupervisorStopped);
        let _ = self.stop.send(true);
        if self
            .thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            let _ = self.thread.take().unwrap().join();
        }
        // On unwinding, the signalled thread finishes its bounded DB call and
        // closes its private pool; never block a Tokio data-plane worker here.
    }
}
#[derive(Default)]
struct RenewalStats {
    attempts: u64,
    failures: u64,
    consecutive_failures: u64,
    last_error: Option<&'static str>,
}
impl RenewalStats {
    fn fields(&self, lease: &live::WarmupLease, permit: &Permit, policy: Policy) -> Value {
        let mut fields = live::warmup_diagnostic_fields(lease);
        fields["renewal_attempts"] = self.attempts.into();
        fields["renewal_failures"] = self.failures.into();
        fields["consecutive_failures"] = self.consecutive_failures.into();
        fields["lease_safe_remaining_ms"] = (permit.safe_remaining().as_millis() as u64).into();
        fields["lease_margin_ms"] = (policy.lease_margin.as_millis() as u64).into();
        if let Some(error) = self.last_error {
            fields["error_code"] = error.into();
        }
        fields
    }
}
async fn run(
    backend: &impl Heartbeat,
    lease: &live::WarmupLease,
    permit: &Permit,
    mut stop: watch::Receiver<bool>,
    policy: Policy,
) {
    let mut stats = RenewalStats::default();
    lifecycle::emit(
        Level::Info,
        "live_supervision_started",
        stats.fields(lease, permit, policy),
    );
    loop {
        if *stop.borrow() {
            break;
        }
        if let Some(reason) = permit.outcome() {
            permit.lose(reason);
            if reason != WarmupControlOutcome::SupervisorStopped {
                let mut fields = stats.fields(lease, permit, policy);
                fields["reason"] = json!(reason);
                lifecycle::emit(Level::Warn, "live_supervision_lost", fields);
            }
            break;
        }
        let renewal_started = Instant::now();
        stats.attempts = stats.attempts.saturating_add(1);
        // Bound both an in-flight call and sleeps by the CONFIRMED safe lease
        // horizon, not the six-hour attempt deadline or a fresh TTL on failure.
        let renewal = tokio::select! {
            biased;
            _ = stop.changed() => break,
            result = tokio::time::timeout(policy.io_timeout.min(permit.safe_remaining()), backend.renew()) => result,
        };
        match renewal {
            Ok(Ok(Some(valid_until))) => {
                permit.renewed(valid_until, policy);
                let recovered = stats.consecutive_failures;
                stats.consecutive_failures = 0;
                stats.last_error = None;
                let mut fields = stats.fields(lease, permit, policy);
                fields["recovered_failures"] = recovered.into();
                if permit.current() {
                    lifecycle::emit(
                        if recovered > 0 {
                            Level::Info
                        } else {
                            Level::Debug
                        },
                        if recovered > 0 {
                            "live_supervision_recovered"
                        } else {
                            "live_supervision_renewed"
                        },
                        fields,
                    );
                }
            }
            Ok(Ok(None)) => {
                // An authoritative fence/expiry rejection is not an I/O miss.
                permit.lose(WarmupControlOutcome::RenewalRejected);
                continue;
            }
            error => {
                stats.failures = stats.failures.saturating_add(1);
                stats.consecutive_failures = stats.consecutive_failures.saturating_add(1);
                stats.last_error = Some(match error {
                    Ok(Err(ref error)) => setups_db::diagnostics::db_error_code(error),
                    _ => "heartbeat_timeout",
                });
                if !permit.current() {
                    continue;
                }
                lifecycle::emit(
                    Level::Warn,
                    "live_supervision_renewal_retry",
                    stats.fields(lease, permit, policy),
                );
            }
        }
        // Start-to-start cadence: a 3s miss must not add 5 more seconds before
        // every retry. A 30s lease normally allows five 5s-spaced attempts.
        let delay = (renewal_started + policy.interval)
            .saturating_duration_since(Instant::now())
            .min(permit.safe_remaining());
        tokio::select! {biased; _=stop.changed()=>break,_=tokio::time::sleep(delay)=>{}}
    }
    let mut fields = stats.fields(lease, permit, policy);
    fields["outcome"] = json!(permit.outcome());
    lifecycle::emit(Level::Info, "live_supervision_stopped", fields);
    let _ = tokio::time::timeout(Duration::from_secs(1), backend.close()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    #[derive(Clone)]
    struct Fake {
        calls: Arc<AtomicUsize>,
        reject: bool,
    }
    impl Heartbeat for Fake {
        async fn renew(&self) -> Result<Option<Instant>, setups_db::DbError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if self.reject {
                None
            } else {
                Some(Instant::now() + Duration::from_secs(30))
            })
        }
    }
    fn lease() -> live::WarmupLease {
        live::WarmupLease {
            owner_id: "admin".into(),
            algorithm_id: "fixture".into(),
            slot_id: 1,
            generation_id: 2,
            config_revision: 1,
            worker: "supervision-test".into(),
            token: 3,
            initialization: live::LiveInitialization {
                version: 1,
                anchor_close_unix: 60,
                detector_from_open_unix: 0,
                emit_from_close_unix: 120,
            },
            attempt: 1,
            attempt_deadline_unix: chrono::Utc::now().timestamp() + 21600,
            lease_valid_until: Instant::now() + Duration::from_secs(30),
        }
    }
    #[tokio::test]
    async fn slow_successful_cpu_tick_does_not_queue_heartbeats_and_stop_joins_cleanup() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fake = Fake {
            calls: calls.clone(),
            reject: false,
        };
        let mut owner = Supervisor::spawn(
            move || fake,
            lease(),
            Policy {
                interval: Duration::from_millis(5),
                progress_timeout: Duration::from_secs(2),
                io_timeout: Duration::from_millis(100),
                ..Policy::default()
            },
        )
        .unwrap();
        // This blocks the sole test/data-plane Tokio thread for several scaled
        // lease periods. A same-runtime heartbeat task could not be polled.
        std::thread::sleep(Duration::from_millis(100));
        assert!(calls.load(Ordering::SeqCst) >= 3);
        owner.permit().check().unwrap();
        owner.permit().progress();
        owner.stop().await.unwrap();
        let count = calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(calls.load(Ordering::SeqCst), count);
        assert!(owner.thread.is_none());
    }
    #[tokio::test]
    async fn transient_heartbeat_timeouts_retry_without_losing_the_current_claim() {
        struct Transient(Arc<AtomicUsize>);
        impl Heartbeat for Transient {
            async fn renew(&self) -> Result<Option<Instant>, setups_db::DbError> {
                if self.0.fetch_add(1, Ordering::SeqCst) < 3 {
                    std::future::pending::<()>().await;
                }
                Ok(Some(Instant::now() + Duration::from_secs(30)))
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = Transient(calls.clone());
        let mut owner = Supervisor::spawn(
            move || backend,
            lease(),
            Policy {
                interval: Duration::from_millis(10),
                io_timeout: Duration::from_millis(10),
                progress_timeout: Duration::from_secs(2),
                ..Policy::default()
            },
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            calls.load(Ordering::SeqCst) >= 4,
            "a transient I/O miss must retry within the still-valid lease"
        );
        owner.permit().check().unwrap();
        owner.stop().await.unwrap();
    }

    #[tokio::test]
    async fn unavailable_renewals_stop_only_at_confirmed_horizon_and_progress_cannot_extend_it() {
        struct Unavailable {
            calls: Arc<AtomicUsize>,
            hang: bool,
        }
        impl Heartbeat for Unavailable {
            async fn renew(&self) -> Result<Option<Instant>, setups_db::DbError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if self.hang {
                    std::future::pending::<()>().await;
                }
                Err(setups_db::DbError::Sqlx(sqlx::Error::PoolTimedOut))
            }
        }
        for hang in [false, true] {
            let mut lease = lease();
            lease.lease_valid_until = Instant::now() + Duration::from_millis(400);
            let horizon = lease.lease_valid_until - Duration::from_millis(40);
            let calls = Arc::new(AtomicUsize::new(0));
            let backend = Unavailable {
                calls: calls.clone(),
                hang,
            };
            let mut owner = Supervisor::spawn(
                move || backend,
                lease,
                Policy {
                    interval: Duration::from_millis(60),
                    io_timeout: Duration::from_millis(30),
                    progress_timeout: Duration::from_secs(2),
                    lease_margin: Duration::from_millis(40),
                },
            )
            .unwrap();
            // Permit intentionally floors its scalar horizon to milliseconds.
            // Check that this is conservative by less than 1ms, then compare
            // loss against that actual horizon, not the unquantized Instant.
            let permit = owner.permit();
            let safe_horizon = permit.0.started
                + Duration::from_millis(permit.0.safe_until_ms.load(Ordering::Acquire));
            assert!(safe_horizon <= horizon);
            assert!(horizon.duration_since(safe_horizon) < Duration::from_millis(1));
            tokio::time::sleep(Duration::from_millis(160)).await;
            assert!(Instant::now() < horizon);
            owner.permit().check().unwrap();
            owner.permit().progress();
            tokio::time::timeout(Duration::from_secs(2), async {
                while owner.permit().current() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                Instant::now() >= safe_horizon,
                "one failed renewal must not spend a still-maintainable lease"
            );
            assert!(
                calls.load(Ordering::SeqCst) >= 5,
                "outages must retry, not fail on their first miss"
            );
            assert!(matches!(
                owner.permit().check(),
                Err(WorkerError::WarmupControl(
                    WarmupControlOutcome::RenewalUnavailable
                ))
            ));
            owner.permit().progress();
            assert!(!owner.permit().current());
            owner.stop().await.unwrap();
            let count = calls.load(Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                count,
                "loss/stop must release all retry work"
            );
        }
    }

    #[test]
    fn delayed_start_or_late_ack_cannot_manufacture_lease_credit() {
        let policy = Policy {
            lease_margin: Duration::from_millis(10),
            ..Policy::default()
        };
        let expired = Permit::new(policy, Instant::now() - Duration::from_millis(1));
        assert!(!expired.current());
        expired.renewed(Instant::now() + Duration::from_secs(30), policy);
        assert!(!expired.current());

        let permit = Permit::new(policy, Instant::now() + Duration::from_millis(100));
        permit.renewed(Instant::now() + Duration::from_millis(200), policy);
        assert!(permit.safe_remaining() > Duration::from_millis(150));
        assert!(permit.safe_remaining() < Duration::from_millis(200));
        let fields = RenewalStats {
            attempts: 5,
            failures: 3,
            consecutive_failures: 3,
            last_error: Some("statement_timeout_or_cancel"),
        }
        .fields(&lease(), &permit, policy);
        assert_eq!(fields["renewal_attempts"], 5);
        assert_eq!(fields["renewal_failures"], 3);
        assert_eq!(fields["consecutive_failures"], 3);
        assert_eq!(fields["error_code"], "statement_timeout_or_cancel");
        assert!(fields["lease_safe_remaining_ms"].as_u64().unwrap() <= 200);
    }

    #[tokio::test]
    #[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
    async fn postgres_supervisor_survives_six_second_data_plane_and_lane_locks_and_cancel_storm() {
        use futures::FutureExt;
        use sqlx::Connection;
        let options: sqlx::postgres::PgConnectOptions =
            std::env::var("TEST_DATABASE_URL").unwrap().parse().unwrap();
        assert!(options.get_database().unwrap_or("").ends_with("_test"));
        let name = format!("warmup_supervision_{}_test", std::process::id());
        let mut admin = sqlx::PgConnection::connect_with(&options).await.unwrap();
        sqlx::query(&format!("create database {name}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.database(&name))
            .await
            .unwrap();
        let baseline = include_str!("../../../../db/migrations/0010_baseline.sql")
            .lines()
            .filter(|line| !line.starts_with('\\') && !line.starts_with("SET transaction_timeout"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut connection = pool.acquire().await.unwrap();
        sqlx::raw_sql(&baseline)
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::raw_sql("set search_path=public; create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null); insert into kysely_migration values('0010_baseline.sql','fixture');")
            .execute(&mut *connection).await.unwrap();
        drop(connection);
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        continuous::configure(
            &pool,
            "admin",
            "price_volume_spike",
            &json!({"adaptiveEnabled":true}),
            &[0.0],
        )
        .await
        .unwrap();
        continuous::start(&pool, "admin", "price_volume_spike")
            .await
            .unwrap();
        setups_db::settings_set(&pool, "live", &json!({"enabled":true}))
            .await
            .unwrap();
        let mut fence = lease();
        fence.algorithm_id = "price_volume_spike".into();
        fence.slot_id = sqlx::query_scalar("select min(id) from continuous_slots")
            .fetch_one(&pool)
            .await
            .unwrap();
        let mut generations = Vec::<i64>::new();
        for _ in 0..2 {
            generations.push(sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,phase,spec,result,accepted,execution_started_at,observation_cutoff) values('admin','price_volume_spike',$1,1,1,'completed','completed','{}','{}',true,now(),180) returning id")
                .bind(fence.slot_id).fetch_one(&pool).await.unwrap());
        }
        fence.generation_id = generations[1];
        continuous::profiles::set_permission(&pool,"admin","price_volume_spike",true).await.unwrap();
        sqlx::query("insert into adaptive_profiles(id,owner_id,algorithm_id,min_setups_per_day,accepted_generation_id) select id,owner_id,algorithm_id,min_setups_per_day,$2 from continuous_slots where id=$1")
            .bind(fence.slot_id).bind(fence.generation_id).execute(&pool).await.unwrap();
        sqlx::query("update continuous_generations set live_init_attempts=1,live_init_status='running',live_init_deadline=clock_timestamp()+interval '6 hours' where id=$1")
            .bind(fence.generation_id).execute(&pool).await.unwrap();
        sqlx::query("update continuous_slots set active_generation_id=$2,pending_generation_id=$3,pending_status='warming' where id=$1")
            .bind(fence.slot_id).bind(generations[0]).bind(fence.generation_id).execute(&pool).await.unwrap();
        fence.lease_valid_until = Instant::now() + Duration::from_secs(30);
        sqlx::query("update adaptive_live_warmup_lane set slot_id=$1,generation_id=$2,lease_owner=$3,lease_token=$4,lease_until=clock_timestamp()+interval '30 seconds' where id=1")
            .bind(fence.slot_id).bind(fence.generation_id).bind(&fence.worker).bind(fence.token).execute(&pool).await.unwrap();
        struct Observed {
            pg: PgHeartbeat,
            successes: Arc<AtomicUsize>,
            failures: Arc<AtomicUsize>,
        }
        impl Heartbeat for Observed {
            async fn renew(&self) -> Result<Option<Instant>, setups_db::DbError> {
                let result = self.pg.renew().await;
                if result.is_err() {
                    self.failures.fetch_add(1, Ordering::SeqCst);
                }
                if matches!(result, Ok(Some(_))) {
                    self.successes.fetch_add(1, Ordering::SeqCst);
                }
                result
            }
            async fn close(&self) {
                self.pg.close().await;
            }
        }
        let successes = Arc::new(AtomicUsize::new(0));
        let failures = Arc::new(AtomicUsize::new(0));
        let (observed_successes, observed_failures) = (successes.clone(), failures.clone());
        let options = pool.connect_options().as_ref().clone();
        let claim = fence.clone();
        let mut owner = Supervisor::spawn(
            move || Observed {
                pg: PgHeartbeat::new(options, claim),
                successes: observed_successes,
                failures: observed_failures,
            },
            fence.clone(),
            Policy {
                interval: Duration::from_millis(200),
                ..Policy::default()
            },
        )
        .unwrap();
        let permit = owner.permit().clone();
        let result = std::panic::AssertUnwindSafe(async {
            let mut held = pool.begin().await.unwrap();
            sqlx::query("select pg_advisory_xact_lock(hashtextextended('admin',72423415))").execute(&mut *held).await.unwrap();
            sqlx::query("select owner_id from continuous_processes where owner_id='admin' for update").execute(&mut *held).await.unwrap();
            sqlx::query("select id from continuous_slots where id=$1 for update").bind(fence.slot_id).execute(&mut *held).await.unwrap();
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                permit.check().unwrap();permit.progress();
            }
            assert!(successes.load(Ordering::SeqCst) >= 5, "heartbeat cannot queue behind cold persistence's owner/process/slot locks");
            held.rollback().await.unwrap();

            let mut held = pool.begin().await.unwrap();
            sqlx::query("select id from adaptive_live_warmup_lane where id=1 for update").execute(&mut *held).await.unwrap();
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                permit.check().unwrap();permit.progress();
            }
            assert!(failures.load(Ordering::SeqCst) >= 3, "actual lane lock timeouts must be retried while lease margin remains");
            held.rollback().await.unwrap();
            let before = successes.load(Ordering::SeqCst);
            tokio::time::timeout(Duration::from_secs(3), async {
                while successes.load(Ordering::SeqCst) == before { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await.unwrap();

            sqlx::raw_sql("create function slow_renewal() returns trigger language plpgsql as $$begin perform pg_sleep(2); return new; end$$; create trigger slow_renewal before update of lease_until on adaptive_live_warmup_lane for each row execute function slow_renewal();")
                .execute(&pool).await.unwrap();
            let before = failures.load(Ordering::SeqCst);
            tokio::time::timeout(Duration::from_secs(5), async {
                while failures.load(Ordering::SeqCst) < before + 3 { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await.unwrap();
            permit.check().unwrap();
            sqlx::query("drop trigger slow_renewal on adaptive_live_warmup_lane").execute(&pool).await.unwrap();
            let before = successes.load(Ordering::SeqCst);
            tokio::time::timeout(Duration::from_secs(3), async {
                while successes.load(Ordering::SeqCst) == before { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await.unwrap();
            permit.check().unwrap();
            assert_eq!(sqlx::query_scalar::<_,i32>("select live_init_attempts from continuous_generations where id=$1")
                .bind(fence.generation_id).fetch_one(&pool).await.unwrap(),1);
            assert_eq!(sqlx::query_scalar::<_,i64>("select active_generation_id from continuous_slots where id=$1")
                .bind(fence.slot_id).fetch_one(&pool).await.unwrap(),generations[0]);
            assert_eq!(live::recover_pending_warmups(&pool,"admin").await.unwrap(),0);

            // Now keep actual PostgreSQL 57014 cancellations going for an
            // entire scaled lease. They must not spend it before its horizon.
            owner.stop().await.unwrap();
            fence.lease_valid_until = Instant::now() + Duration::from_secs(3);
            sqlx::query("update adaptive_live_warmup_lane set lease_until=clock_timestamp()+interval '3 seconds'")
                .execute(&pool).await.unwrap();
            sqlx::query("create trigger slow_renewal before update of lease_until on adaptive_live_warmup_lane for each row execute function slow_renewal()")
                .execute(&pool).await.unwrap();
            let policy = Policy { interval: Duration::from_millis(200), lease_margin: Duration::from_millis(200), ..Policy::default() };
            let horizon = fence.lease_valid_until - policy.lease_margin;
            let options = pool.connect_options().as_ref().clone();
            let claim = fence.clone();
            let (observed_successes, observed_failures) = (successes.clone(), failures.clone());
            let before = failures.load(Ordering::SeqCst);
            owner = Supervisor::spawn(move || Observed {
                pg: PgHeartbeat::new(options, claim), successes: observed_successes, failures: observed_failures,
            }, fence.clone(), policy).unwrap();
            tokio::time::sleep(Duration::from_millis(1600)).await;
            owner.permit().check().unwrap();
            owner.permit().progress();
            tokio::time::timeout(Duration::from_secs(3), async {
                while owner.permit().current() { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await.unwrap();
            assert!(Instant::now() >= horizon);
            assert!(failures.load(Ordering::SeqCst) >= before + 3);
            assert!(matches!(owner.permit().check(), Err(WorkerError::WarmupControl(WarmupControlOutcome::RenewalUnavailable))));
            assert_eq!(sqlx::query_scalar::<_,i32>("select live_init_attempts from continuous_generations where id=$1")
                .bind(fence.generation_id).fetch_one(&pool).await.unwrap(),1,"heartbeat retries never manufacture new claim attempts");
        }).catch_unwind().await;
        owner.stop().await.unwrap();
        pool.close().await;
        sqlx::query(&format!("drop database {name} with (force)"))
            .execute(&mut admin)
            .await
            .unwrap();
        admin.close().await.unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    #[tokio::test]
    async fn rejected_renewal_and_stalled_progress_are_typed_and_never_resurrect() {
        for reject in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let fake = Fake { calls, reject };
            let mut owner = Supervisor::spawn(
                move || fake,
                lease(),
                Policy {
                    interval: Duration::from_millis(5),
                    progress_timeout: Duration::from_millis(30),
                    io_timeout: Duration::from_millis(100),
                    ..Policy::default()
                },
            )
            .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let expected = if reject {
                WarmupControlOutcome::RenewalRejected
            } else {
                WarmupControlOutcome::ProgressBudgetExpired
            };
            assert!(
                matches!(owner.permit().check(),Err(WorkerError::WarmupControl(reason)) if reason==expected)
            );
            owner.permit().progress();
            assert!(!owner.permit().current());
            owner.stop().await.unwrap();
        }
    }
}
