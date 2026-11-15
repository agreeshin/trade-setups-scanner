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
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            progress_timeout: Duration::from_secs(120),
            io_timeout: Duration::from_secs(3),
        }
    }
}
struct Shared {
    started: Instant,
    last_progress_ms: AtomicU64,
    state: AtomicU8,
    progress_timeout: Duration,
}
#[derive(Clone)]
pub(super) struct Permit(Arc<Shared>);
impl Permit {
    fn new(policy: Policy) -> Self {
        Self(Arc::new(Shared {
            started: Instant::now(),
            last_progress_ms: AtomicU64::new(0),
            state: AtomicU8::new(0),
            progress_timeout: policy.progress_timeout,
        }))
    }
    fn outcome(&self) -> Option<WarmupControlOutcome> {
        match self.0.state.load(Ordering::Acquire) {
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
    async fn renew(&self) -> Result<live::WarmupRenewal, setups_db::DbError>;
    async fn close(&self) {}
}
struct PgHeartbeat {
    pool: PgPool,
    lease: live::WarmupLease,
}
impl Heartbeat for PgHeartbeat {
    async fn renew(&self) -> Result<live::WarmupRenewal, setups_db::DbError> {
        live::renew_pending_warmup(&self.pool, &self.lease).await
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
            async fn renew(&self) -> Result<live::WarmupRenewal, setups_db::DbError> {
                Ok(live::WarmupRenewal::Renewed)
            }
        }
        Self::spawn(|| Healthy, lease.clone(), Policy::default()).unwrap()
    }
    pub fn start(pool: &PgPool, lease: &live::WarmupLease) -> Result<Self, WorkerError> {
        let options = pool.connect_options().as_ref().clone();
        let fence = lease.clone();
        Self::spawn(
            move || PgHeartbeat {
                pool: sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(2))
                    .connect_lazy_with(options),
                lease: fence,
            },
            lease.clone(),
            Policy::default(),
        )
    }
    fn spawn<B: Heartbeat>(
        factory: impl FnOnce() -> B + Send + 'static,
        lease: live::WarmupLease,
        policy: Policy,
    ) -> Result<Self, WorkerError> {
        let permit = Permit::new(policy);
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
async fn run(
    backend: &impl Heartbeat,
    lease: &live::WarmupLease,
    permit: &Permit,
    mut stop: watch::Receiver<bool>,
    policy: Policy,
) {
    lifecycle::emit(
        Level::Info,
        "live_supervision_started",
        live::warmup_diagnostic_fields(lease),
    );
    loop {
        if *stop.borrow() {
            break;
        }
        if let Some(reason) = permit.outcome() {
            permit.lose(reason);
            if reason != WarmupControlOutcome::SupervisorStopped {
                let mut fields = live::warmup_diagnostic_fields(lease);
                fields["reason"] = json!(reason);
                lifecycle::emit(Level::Warn, "live_supervision_lost", fields);
            }
            break;
        }
        let renewal = tokio::time::timeout(policy.io_timeout, backend.renew()).await;
        let reason = match &renewal {
            Ok(Ok(live::WarmupRenewal::Renewed)) => None,
            Ok(Ok(live::WarmupRenewal::Rejected)) => Some(WarmupControlOutcome::RenewalRejected),
            _ => Some(WarmupControlOutcome::RenewalUnavailable),
        };
        if let Some(reason) = reason {
            permit.lose(reason);
            let mut fields = live::warmup_diagnostic_fields(lease);
            fields["reason"] = json!(reason);
            if let Ok(Err(error)) = &renewal {
                fields["error_code"] = setups_db::diagnostics::db_error_code(error).into();
            }
            if renewal.is_err() {
                fields["error_code"] = "heartbeat_timeout".into();
            }
            lifecycle::emit(Level::Warn, "live_supervision_lost", fields);
            break;
        }
        lifecycle::emit(
            Level::Debug,
            "live_supervision_renewed",
            live::warmup_diagnostic_fields(lease),
        );
        tokio::select! {biased; _=stop.changed()=>break,_=tokio::time::sleep(policy.interval)=>{}}
    }
    let mut fields = live::warmup_diagnostic_fields(lease);
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
        async fn renew(&self) -> Result<live::WarmupRenewal, setups_db::DbError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if self.reject {
                live::WarmupRenewal::Rejected
            } else {
                live::WarmupRenewal::Renewed
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
