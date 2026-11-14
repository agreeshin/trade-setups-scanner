//! One retained frozen input per leased owner, through search -> durable replay -> analytics.
use super::ContinuousRuntimeConfig;
use crate::optimization::{JobError,snapshot};
use serde_json::{Value,json};
use setups_db::continuous::scheduler::{self as db,ChainJob,OwnerLease,ServiceReplayRun};
use setups_db::continuous::recovery;
use setups_optimizer::{self as core,CancelToken,StudyRequest,StudyCandidate,ContinuousContext,ContinuousReplay,TerminationReason};
use sqlx::PgPool;
use std::{sync::{Arc,atomic::{AtomicBool,Ordering}},time::{Duration,Instant}};
use tokio::sync::{Notify,watch};
use tokio::time::Instant as LeaseInstant;
use tokio_util::sync::CancellationToken;

pub struct ContinuousHandle {
    stop: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    pub notify: Arc<Notify>,
    pub available: Arc<AtomicBool>,
}
impl ContinuousHandle {
    pub async fn stop(self) {
        self.available.store(false,Ordering::SeqCst);
        self.stop.cancel();
        for task in self.tasks {if let Err(error)=task.await {eprintln!("continuous runtime join failed: {error}");}}
    }
}

pub async fn start_continuous_runtime(pool:PgPool,market_url:String,market_key:String,config:ContinuousRuntimeConfig)->Result<ContinuousHandle,String> {
    if pool.options().get_max_connections()<2 {return Err("continuous runtime requires at least two shared database connections".into());}
    sqlx::query("select owner_id from continuous_owner_lanes limit 0").execute(&pool).await.map_err(|e|e.to_string())?;
    recovery::reclaim_orphans(&pool).await.map_err(|error|error.to_string())?;
    if config.workers==0 || config.workers>4 {return Err("continuous workers must be in 1..=4".into());}
    if config.max_concurrent_chains==0 || config.max_concurrent_chains>config.workers {return Err("continuous resident chain admission must be in 1..=workers".into());}
    let resident = Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_chains));
    let stop=CancellationToken::new();
    let notify=Arc::new(Notify::new());
    let available=Arc::new(AtomicBool::new(true));
    let mut tasks=Vec::with_capacity(config.workers);
    for index in 0..config.workers {
        let pool=pool.clone();let url=market_url.clone();let key=market_key.clone();let config=config.clone();
        let stop=stop.clone();let notify=notify.clone();let available=available.clone();
        let resident=resident.clone();
        let worker=format!("continuous-{}-{:016x}-{index}",std::process::id(),rand::random::<u64>());
        tasks.push(tokio::spawn(async move {
            while !stop.is_cancelled() {
                // A permit owns the entire snapshot/CPU lifecycle, through the final join.
                // Extra pollers cannot multiply resident snapshots or CPU kernels.
                let permit=tokio::select!{_=stop.cancelled()=>break,result=resident.clone().acquire_owned()=>match result {Ok(permit)=>permit,Err(_)=>break}};
                let claim_started=LeaseInstant::now();
                match db::claim_owner(&pool,&worker).await {
                    Ok(Some(lease))=>{
                        available.store(true,Ordering::SeqCst);
                        let release=match run_owner(&pool,&lease,&url,&key,&config,&stop,claim_started,&Persistence::default()).await {
                            Ok(())=>true,
                            Err(error)=>{
                                eprintln!("continuous owner failed: owner={} code={}",lease.owner_id,error.code);
                                let keep_abandoned=error.code=="persistence_defer_unavailable" || uncertain(&error);
                                if keep_abandoned {tokio::time::sleep(Duration::from_secs(1)).await;}
                                !keep_abandoned
                            },
                        };
                        // When defer could not persist, retain the non-null owner.
                        // Its expiry lets the next claim account for this failure.
                        if release && let Err(error)=db::release(&pool,&lease).await {eprintln!("continuous lease release failed: {error}");}
                    }
                    Ok(None)=>{
                        // A healthy idle claim must restore Start after a transient DB outage.
                        available.store(true,Ordering::SeqCst);
                        tokio::select!{_=stop.cancelled()=>{},_=notify.notified()=>{},_=tokio::time::sleep(config.poll)=>{}}
                    },
                    Err(error)=>{
                        available.store(false,Ordering::SeqCst);
                        eprintln!("continuous claim failed: {error}");
                        tokio::select!{_=stop.cancelled()=>{},_=tokio::time::sleep(Duration::from_secs(1))=>{}}
                    }
                }
                drop(permit);
            }
        }));
    }
    Ok(ContinuousHandle{stop,tasks,notify,available})
}

fn database(error:impl std::fmt::Display)->JobError {JobError::new("database_error",error.to_string())}
fn persistence_error(error:setups_db::DbError)->JobError {
    JobError::new(if recovery::is_transient(&error){"persistence_unconfirmed"}else{"database_error"},error.to_string())
}
/// Per-chain control owns at most one test hook. Production retains no fault
/// state/history; every operation retries only typed transient errors, three
/// times total, using its idempotent durable head as the acknowledgement.
#[cfg(test)]
type PersistenceFault=Arc<dyn Fn(&'static str)->Result<(),setups_db::DbError>+Send+Sync>;
#[derive(Clone,Default)]
struct Persistence {
    #[cfg(test)]
    after_success:Option<PersistenceFault>,
}
impl Persistence {
    fn after_success(&self,_operation:&'static str)->Result<(),setups_db::DbError> {
        #[cfg(test)]
        if let Some(hook)=&self.after_success {return hook(_operation);}
        Ok(())
    }
    async fn call<T,F,Fut>(&self,operation:&'static str,mut run:F)->Result<T,JobError>
    where F:FnMut()->Fut,Fut:std::future::Future<Output=Result<T,setups_db::DbError>> {
        for attempt in 0..3 {
            let result=run().await.and_then(|value|{self.after_success(operation)?;Ok(value)});
            match result {
                Ok(value)=>return Ok(value),
                Err(error) if recovery::is_transient(&error) && attempt<2=>tokio::time::sleep(Duration::from_millis(25*(attempt+1))).await,
                Err(error)=>return Err(persistence_error(error)),
            }
        }
        unreachable!("bounded persistence retry always returns")
    }
    async fn bridge<T,F,Fut>(&self,mut run:F)->Result<T,JobError>
    where F:FnMut()->Fut,Fut:std::future::Future<Output=Result<T,JobError>> {
        for attempt in 0..3 {
            match run().await {
                Ok(value)=>return Ok(value),
                Err(error) if error.code=="persistence_unconfirmed" && attempt<2=>tokio::time::sleep(Duration::from_millis(25*(attempt+1))).await,
                Err(error)=>return Err(error),
            }
        }
        unreachable!("bounded bridge retry always returns")
    }
}
fn interrupted()->JobError {JobError::new("chain_interrupted","Цепочка прервана; незавершённая стадия не является непрохождением")}
fn suspended()->JobError {JobError::new("chain_suspended","Continuous chain suspended at a durable boundary")}
fn uncertain(error:&JobError)->bool {matches!(error.code,"persistence_unconfirmed"|"monitor_lease_unconfirmed"|"monitor_current_unconfirmed"|"monitor_idle_stop_unconfirmed")}
fn check_shutdown(stop:&CancellationToken)->Result<(),JobError> {if stop.is_cancelled(){Err(suspended())}else{Ok(())}}
fn compute_error(error:core::StudyError)->JobError {
    let code=if matches!(error,core::StudyError::ResourceLimit{..}){"resource_limit"}else{"evaluation_failed"};
    JobError::new(code,error.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn run_owner(pool:&PgPool,lease:&OwnerLease,url:&str,key:&str,config:&ContinuousRuntimeConfig,stop:&CancellationToken,claim_started:LeaseInstant,persistence:&Persistence)->Result<(),JobError> {
    persistence.call("recover_owner",||db::recover_owner(pool,lease)).await?;
    if stop.is_cancelled() {return Ok(());}
    let job=match persistence.call("active_job",||recovery::active_job(pool,lease)).await? {
        Some(job)=>job,
        None=>{let Some(job)=persistence.call("prepare_next",||db::prepare_next(pool,lease)).await? else{return Ok(());};job},
    };
    let started=Instant::now();
    println!("continuous chain start: owner={} generation={} study={} revision={} leaseToken={} phase=acquiring",lease.owner_id,job.generation_id,job.study_id,job.config_revision,lease.token);
    let cancel=CancellationToken::new();
    // Keep renewing while cooperative CPU/materialization work joins, including Stop.
    let monitor_stop=CancellationToken::new();
    let monitor=tokio::spawn(monitor_job(pool.clone(),job.clone(),cancel.clone(),monitor_stop.clone(),claim_started));
    let outcome=run_chain(pool,&job,url,key,config,&cancel,stop,persistence).await;
    monitor_stop.cancel();
    let monitor_reason=monitor.await.map_err(|error|JobError::new("monitor_failed",error.to_string()))?;
    // A final current=false can race a successful finalizer clearing its pointer.
    // Never replace success or another precise computation/database error.
    let outcome=chain_outcome(outcome,monitor_reason);
    let outcome_kind=if outcome.is_ok() {"completed"}else if outcome.as_ref().is_err_and(|error|error.code=="chain_suspended") {"suspended"}else if outcome.as_ref().is_err_and(uncertain) {"deferred"}else{"interrupted_or_failed"};
    if let Err(error)=outcome && error.code!="chain_suspended" {
        let error=json!({"code":if outcome_kind=="deferred"{"persistence_unconfirmed"}else{error.code},"causeCode":error.code,"message":error.message});
        if outcome_kind=="deferred" {
            // This may itself have an uncertain ACK. Its epoch marker makes a
            // retry charge at most one attempt; never fall back to terminal fail.
            let deferred=persistence.call("defer",||recovery::defer(pool,&job,&error)).await;
            // A false fence can mean the lease expired before the error marker
            // was written. Do not erase that abandoned-owner evidence unless
            // the generation is authoritatively terminal/stopped/reconfigured.
            let releasable=if matches!(deferred,Ok(false)) {
                persistence.call("defer_state",||async {sqlx::query_scalar::<_,bool>("select not exists(select 1 from continuous_generations g join continuous_processes p on p.active_generation_id=g.id where g.id=$1 and g.owner_id=$2 and g.config_revision=$3 and g.status='running' and p.desired_running and p.config_revision=g.config_revision)")
                    .bind(job.generation_id).bind(&job.lease.owner_id).bind(job.config_revision).fetch_one(pool).await.map_err(Into::into)}).await
            }else{Ok(true)};
            if deferred.is_err() || !matches!(releasable,Ok(true)) {
                let error=deferred.err().or_else(||releasable.err()).unwrap_or_else(||JobError::new("persistence_defer_unavailable","Lease expired before the recovery error could be persisted"));
                eprintln!("continuous persistence defer unavailable: generation={} code={}",job.generation_id,error.code);
                tokio::time::sleep(Duration::from_secs(1)).await;
                return Err(JobError::new("persistence_defer_unavailable",error.message));
            }
        }else{
            eprintln!("continuous chain failed: generation={} code={}",job.generation_id,error["code"]);
            persistence.call("fail",||db::fail(pool,&job,&error)).await?;
        }
    }
    db::drain_stops(pool,lease).await.map_err(database)?;
    println!("continuous chain resources released: generation={} elapsedSeconds={:.3} outcome={outcome_kind}",job.generation_id,started.elapsed().as_secs_f64());
    Ok(())
}

fn chain_outcome(outcome:Result<(),JobError>,reason:Option<JobError>)->Result<(),JobError> {
    match (outcome,reason) {
        (Err(error),Some(reason)) if error.code=="chain_interrupted"=>Err(reason),
        (outcome,_)=>outcome,
    }
}

const MONITOR_QUERY_TIMEOUT:Duration=Duration::from_secs(2);
// No longer than the existing 30s DB lease, with 5s reserved before expiry.
const MONITOR_CONFIRMED_WINDOW:Duration=Duration::from_secs(db::LEASE_SECONDS as u64-5);
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
enum MonitorOperation {Heartbeat,Current,IdleStopDrain}
impl MonitorOperation {
    fn index(self)->usize {match self {Self::Heartbeat=>0,Self::Current=>1,Self::IdleStopDrain=>2}}
    fn name(self)->&'static str {match self {Self::Heartbeat=>"heartbeat",Self::Current=>"current",Self::IdleStopDrain=>"idle_stop_drain"}}
}
#[derive(Debug)]
struct MonitorIssue {operation:MonitorOperation,stage:&'static str,cause:String,transient:bool,acquire_ms:Option<u128>,elapsed_ms:u128}
impl MonitorIssue {
    fn summary(&self)->String {format!("operation={} stage={} cause={} acquireMs={} elapsedMs={}",self.operation.name(),self.stage,self.cause,self.acquire_ms.map_or_else(||"unknown".into(),|ms|ms.to_string()),self.elapsed_ms)}
}
fn monitor_db_cause(error:setups_db::DbError)->(String,bool) {
    use setups_db::DbError;
    match error {
        DbError::Sqlx(sqlx::Error::PoolTimedOut)=>("pool_acquire_timeout".into(),true),
        DbError::Sqlx(sqlx::Error::Io(_))=>("connection_io".into(),true),
        DbError::Sqlx(sqlx::Error::PoolClosed)=>("pool_closed".into(),false),
        DbError::Sqlx(sqlx::Error::Database(error))=>{
            // SQLSTATE only: never log server detail, SQL parameters or payloads.
            let code=error.code().filter(|code|code.len()==5 && code.bytes().all(|b|b.is_ascii_alphanumeric()));
            let transient=code.as_deref().is_some_and(|code|code.starts_with("08") || code.starts_with("40") || matches!(code,"55P03"|"57014"|"53300"));
            (format!("sqlstate_{}",code.as_deref().unwrap_or("unknown")),transient)
        }
        DbError::Json(_)=>("invalid_database_state".into(),false),
        _=>("database_error".into(),false),
    }
}
struct MonitorProof {
    confirmed:[LeaseInstant;3],
    pending:[bool;3],
    issue:Option<MonitorIssue>,
    failures:u32,
}
impl MonitorProof {
    fn new(claim_started:LeaseInstant)->Self {Self{confirmed:[claim_started;3],pending:[false;3],issue:None,failures:0}}
    fn deadline(&self)->LeaseInstant {*self.confirmed.iter().min().unwrap()+MONITOR_CONFIRMED_WINDOW}
    fn confirm(&mut self,operation:MonitorOperation,request_started:LeaseInstant,ack:LeaseInstant)->bool {
        // Request START is a lower bound on clock_timestamp()+30s; ACK is not.
        // Do not revive local permission with an ACK arriving after the old bound.
        if ack>=self.deadline() || ack>=request_started+MONITOR_CONFIRMED_WINDOW {return false;}
        self.confirmed[operation.index()]=request_started;
        self.pending[operation.index()]=false;
        true
    }
    fn expired(&self)->JobError {
        let oldest=if self.confirmed[0]<=self.confirmed[1] && self.confirmed[0]<=self.confirmed[2] {MonitorOperation::Heartbeat}
            else if self.confirmed[1]<=self.confirmed[2] {MonitorOperation::Current}else{MonitorOperation::IdleStopDrain};
        JobError::new(match oldest {MonitorOperation::Heartbeat=>"monitor_lease_unconfirmed",MonitorOperation::Current=>"monitor_current_unconfirmed",MonitorOperation::IdleStopDrain=>"monitor_idle_stop_unconfirmed"},
            format!("Не удалось подтвердить {} до безопасной границы lease; lastIssue={:?}",oldest.name(),self.issue.as_ref().map(MonitorIssue::summary)))
    }
}

async fn monitor_probe(pool:&PgPool,job:&ChainJob,operation:MonitorOperation,safe_deadline:LeaseInstant)->Result<bool,MonitorIssue> {
    let started=LeaseInstant::now();
    let deadline=safe_deadline.min(started+MONITOR_QUERY_TIMEOUT);
    let issue=|stage,cause,transient,acquire_ms|MonitorIssue{operation,stage,cause,transient,acquire_ms,elapsed_ms:started.elapsed().as_millis()};
    if operation==MonitorOperation::IdleStopDrain {
        // Other idle algorithms' Stop cleanup is not proof of this job's fence.
        return match tokio::time::timeout_at(deadline,db::drain_idle_stops(pool,&job.lease,&job.algorithm_id)).await {
            Ok(Ok(()))=>Ok(true),
            Ok(Err(error))=>{let (cause,transient)=monitor_db_cause(error);Err(issue("housekeeping",cause,transient,None))},
            Err(_)=>Err(issue("housekeeping","query_timeout".into(),true,None)),
        };
    }
    let mut connection=match tokio::time::timeout_at(deadline,pool.acquire()).await {
        Ok(Ok(connection))=>connection,
        Ok(Err(error))=>{let (cause,transient)=monitor_db_cause(error.into());return Err(issue("acquire",cause,transient,Some(started.elapsed().as_millis())));},
        Err(_)=>return Err(issue("acquire","acquire_timeout".into(),true,Some(started.elapsed().as_millis()))),
    };
    let acquire_ms=Some(started.elapsed().as_millis());
    match tokio::time::timeout_at(deadline,async {
        match operation {MonitorOperation::Heartbeat=>db::heartbeat(&mut *connection,&job.lease).await,MonitorOperation::Current=>db::current(&mut *connection,job).await,MonitorOperation::IdleStopDrain=>unreachable!()}
    }).await {
        Ok(Ok(accepted))=>Ok(accepted),
        Ok(Err(error))=>{let (cause,transient)=monitor_db_cause(error);Err(issue("query",cause,transient,acquire_ms))},
        Err(_)=>Err(issue("query","query_timeout".into(),true,acquire_ms)),
    }
}
fn monitor_log(event:&str,pool:&PgPool,job:&ChainJob,proof:&MonitorProof,code:&str) {
    eprintln!("continuous monitor {event}: owner={} generation={} study={} revision={} leaseToken={} code={} poolSize={} poolIdle={} poolMax={} failures={} issue={:?}",
        job.lease.owner_id,job.generation_id,job.study_id,job.config_revision,job.lease.token,code,pool.size(),pool.num_idle(),pool.options().get_max_connections(),proof.failures,proof.issue.as_ref().map(MonitorIssue::summary));
}
fn monitor_cancel(reason:&mut Option<JobError>,error:JobError,cancel:&CancellationToken,pool:&PgPool,job:&ChainJob,proof:&MonitorProof) {
    if reason.is_none() {monitor_log("cancelled",pool,job,proof,error.code);*reason=Some(error);cancel.cancel();}
}
async fn monitor_job(pool:PgPool,job:ChainJob,cancel:CancellationToken,stop:CancellationToken,claim_started:LeaseInstant)->Option<JobError> {
    let mut interval=tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut proof=MonitorProof::new(claim_started);
    let mut reason=None;
    let mut last_heartbeat=None;
    'monitor: loop {
        tokio::select!{biased;
            _=stop.cancelled()=>return reason,
            _=tokio::time::sleep_until(proof.deadline()),if reason.is_none()=>{let error=proof.expired();monitor_cancel(&mut reason,error,&cancel,&pool,&job,&proof);},
            _=interval.tick()=>{},
        }
        for operation in [MonitorOperation::Heartbeat,MonitorOperation::Current,MonitorOperation::IdleStopDrain] {
            if operation==MonitorOperation::Heartbeat && last_heartbeat.is_some_and(|last:LeaseInstant|last.elapsed()<Duration::from_secs(1)) {continue;}
            if reason.is_some() && operation!=MonitorOperation::Heartbeat {continue;}
            let started=LeaseInstant::now();
            if operation==MonitorOperation::Heartbeat {last_heartbeat=Some(started);}
            let deadline=if reason.is_some() {started+MONITOR_QUERY_TIMEOUT}else{proof.deadline()};
            let result=tokio::select!{biased;
                _=stop.cancelled()=>return reason,
                result=monitor_probe(&pool,&job,operation,deadline)=>result,
            };
            // Cancel is irreversible, but the owner must stay leased through join.
            if reason.is_some() {continue;}
            match result {
                Ok(true)=>{if !proof.confirm(operation,started,LeaseInstant::now()) {let error=proof.expired();monitor_cancel(&mut reason,error,&cancel,&pool,&job,&proof);continue 'monitor;}},
                Ok(false)=>{
                    let code=if operation==MonitorOperation::Heartbeat {"continuous_lease_lost"}else{"continuous_fence_lost"};
                    monitor_cancel(&mut reason,JobError::new(code,format!("{}: authoritative predicate rejected owner/revision/generation",operation.name())),&cancel,&pool,&job,&proof);
                    continue 'monitor;
                }
                Err(issue)=>{
                    let first=proof.issue.is_none();let transient=issue.transient;
                    proof.pending[operation.index()]=true;proof.failures=proof.failures.saturating_add(1);proof.issue=Some(issue);
                    if first {monitor_log("degraded",&pool,&job,&proof,"monitor_probe_failed");}
                    if !transient {monitor_cancel(&mut reason,JobError::new("monitor_database_error",format!("Контроль цепочки завершился ошибкой: {:?}",proof.issue.as_ref().map(MonitorIssue::summary))),&cancel,&pool,&job,&proof);continue 'monitor;}
                    if LeaseInstant::now()>=proof.deadline() {let error=proof.expired();monitor_cancel(&mut reason,error,&cancel,&pool,&job,&proof);continue 'monitor;}
                }
            }
        }
        // Current must have succeeded after uncertainty, not only heartbeat.
        if reason.is_none() && proof.issue.is_some() && proof.pending.iter().all(|pending|!*pending) {
            monitor_log("recovered",&pool,&job,&proof,"monitor_recovered");proof.issue=None;
        }
    }
}

/// Never drop a live spawn_blocking handle: cancellation requests cooperation, then joins.
async fn blocking<T:Send+'static>(cancel:&CancellationToken,deadline:Option<tokio::time::Instant>,run:impl FnOnce(Arc<CancelToken>)->Result<T,core::StudyError>+Send+'static)->Result<(T,bool),JobError> {
    let core_cancel=Arc::new(CancelToken::new());
    let worker_cancel=core_cancel.clone();
    let mut task=tokio::task::spawn_blocking(move||run(worker_cancel));
    let timer=async {if let Some(deadline)=deadline {tokio::time::sleep_until(deadline).await;}else{std::future::pending::<()>().await;}};
    let (result,timed_out)=tokio::select!{
        result=&mut task=>(result,false),
        _=cancel.cancelled()=>{core_cancel.cancel();(task.await,false)},
        _=timer=>{core_cancel.cancel();(task.await,true)},
    };
    let value=result.map_err(|_|JobError::new("worker_failed","Вычислительная задача завершилась с ошибкой"))?.map_err(compute_error)?;
    Ok((value,timed_out))
}

async fn change_phase(pool:&PgPool,job:&ChainJob,phase:&str,persistence:&Persistence)->Result<(),JobError> {
    if persistence.call("phase",||db::phase(pool,job,phase,None)).await? {
        println!("continuous chain phase: generation={} study={} phase={phase}",job.generation_id,job.study_id);
        Ok(())
    }else{Err(interrupted())}
}

#[allow(clippy::too_many_arguments)]
async fn run_chain(pool:&PgPool,job:&ChainJob,url:&str,key:&str,config:&ContinuousRuntimeConfig,cancel:&CancellationToken,shutdown:&CancellationToken,persistence:&Persistence)->Result<(),JobError> {
    let spec:StudyRequest=serde_json::from_value(job.spec.clone()).map_err(|e|JobError::new("invalid_persisted_spec",e.to_string()))?;
    let context=ContinuousContext{observation_cutoff:job.observation_cutoff};
    core::validate_continuous_request(&spec,context).map_err(compute_error)?;
    let identity=super::frozen::identity(job);
    let mut state=persistence.call("load",||recovery::load(pool,job,&identity)).await?;
    check_shutdown(shutdown)?;
    if let Some(gate)=state.gate {
        if persistence.call("finalize",||db::finalize(pool,job,&gate.result,gate.accepted,gate.mean,gate.selected.as_ref())).await? {return Ok(());}
        return Err(interrupted());
    }
    let estimated=config.optimizer.max_setups_per_candidate.saturating_mul(4096)
        .saturating_add(spec.tp_levels.len().saturating_mul(spec.sl_levels.len()).saturating_mul(4096))
        .saturating_add(config.optimizer.max_request_bytes.saturating_mul(2)).saturating_add(8*1024*1024);
    let budget=persistence.call("reserve",||db::reserve_storage(pool,job,i64::try_from(estimated).unwrap_or(i64::MAX),config.max_database_bytes,&config.replay_admission)).await?.ok_or_else(interrupted)?;
    if !budget.admitted {return Err(JobError::new("resource_limit",format!("Continuous database budget exceeded: limit={}, requested={}; completed history is preserved",budget.limit,budget.requested)));}
    let absolute_deadline=job.research_deadline.ok_or_else(||JobError::new("resume_deadline_missing","Original research deadline is missing"))?;
    let remaining=||absolute_deadline.signed_duration_since(chrono::Utc::now()).to_std().unwrap_or(Duration::ZERO);
    let deadline=tokio::time::Instant::now()+remaining();
    let input=if state.snapshot_committed {
        let io_cancel=cancel.child_token();
        let load=persistence.bridge(||super::frozen::load(pool,job,config.optimizer.max_snapshot_bytes,&io_cancel));tokio::pin!(load);
        let result=tokio::select!{
            result=&mut load=>result,
            _=shutdown.cancelled()=>{io_cancel.cancel();load.await},
        };
        if cancel.is_cancelled() {return Err(interrupted());}check_shutdown(shutdown)?;
        Arc::new(result?)
    }else{
        let input=tokio::select!{
            result=snapshot::acquire_continuous(&spec,context,url,key,&config.optimizer)=>Arc::new(result?),
            _=cancel.cancelled()=>return Err(interrupted()),
            _=shutdown.cancelled()=>return Err(suspended()),
            _=tokio::time::sleep_until(deadline)=>return Err(JobError::new("acquisition_timeout","Истёк бюджет acquisition; отсутствие снимка не считается непрохождением")),
        };
        let io_cancel=cancel.child_token();
        let persist_input=input.clone();
        let persist=persistence.bridge(||super::frozen::persist(pool,job,persist_input.clone(),config.optimizer.max_snapshot_bytes,config.max_runtime_bytes,config.max_database_bytes,&config.replay_admission,&io_cancel));tokio::pin!(persist);
        let result=tokio::select!{
            result=&mut persist=>result,
            _=shutdown.cancelled()=>{io_cancel.cancel();persist.await},
        };
        if cancel.is_cancelled() {return Err(interrupted());}check_shutdown(shutdown)?;result?;
        input
    };
    check_shutdown(shutdown)?;
    let outcome=if let Some(result)=state.search_result.take() {
        serde_json::from_value::<core::StudyOutcome>(result).map_err(|error|JobError::new("resume_search_result_invalid",error.to_string()))?
    }else{
        change_phase(pool,job,"searching",persistence).await?;
        search_resumable(pool,job,&spec,input.clone(),context,config,cancel,shutdown,state.checkpoint.take(),absolute_deadline,identity.clone(),persistence).await?
    };
    if cancel.is_cancelled() || outcome.reason==TerminationReason::Cancelled {return Err(interrupted());}
    if let Some(best)=&outcome.best
        && (best.provenance.snapshot_hash!=input.snapshot_hash || best.provenance.optimizer_version!=core::CONTINUOUS_OPTIMIZER_VERSION || best.provenance.seed!=spec.seed) {
        return Err(JobError::new("resume_search_identity_incompatible","Completed search differs from frozen input/optimizer/seed"));
    }
    check_shutdown(shutdown)?;
    let Some(best)=outcome.best else {
        let gate=recovery::Gate {result:json!({"best":null,"reason":"no_winner"}),accepted:false,mean:None,selected:None};
        if !persistence.call("gate",||recovery::save_gate(pool,job,&identity,&gate)).await? {return Err(interrupted());}
        if !persistence.call("finalize",||db::finalize(pool,job,&gate.result,false,None,None)).await? {return Err(interrupted());}
        return Ok(());
    };
    let candidate=StudyCandidate{params:best.params.clone(),analytics:core::AnalyticsParams{
        cluster_enabled:best.analytics.cluster_enabled,cluster_gap_minutes:best.analytics.cluster_gap_minutes.unwrap_or(spec.analytics_params.cluster_gap_minutes),
        time_shift_minutes:best.analytics.time_shift_minutes,price_change_pct:best.analytics.price_change_pct,
    }};
    if !state.replay_committed {
        change_phase(pool,job,"replay",persistence).await?;
        let cpu_spec=spec.clone();let cpu_input=input.clone();let cpu_candidate=candidate.clone();let limits=config.optimizer.study_limits();
        let (prepared,_)=blocking(cancel,None,move|token| {
            let Some(replay)=core::materialize_continuous(&cpu_spec,&cpu_input,&cpu_candidate,context,&limits,&token)? else {return Ok(None);};
            let bootstrap=core::continuous_bootstrap_with_frozen_prefix(&cpu_spec,&replay,&cpu_input)?;
            Ok(Some((replay,bootstrap)))
        }).await?;
        let (replay,bootstrap)=prepared.ok_or_else(interrupted)?;
        let mut manifest=replay_manifest(&replay);
        manifest["bootstrap"]=json!(bootstrap);
        let runs=replay.instruments.iter().map(|raw|Ok(ServiceReplayRun{
            exchange:raw.instrument.exchange.clone(),category:raw.instrument.category.clone(),symbol:raw.instrument.symbol.clone(),
            setups:serde_json::to_value(&raw.setups).map_err(|e|JobError::new("serialization_failed",e.to_string()))?,
            warmup_minutes:raw.warmup_minutes as i64,evaluated_minutes:raw.evaluated_minutes as i64,
        })).collect::<Result<Vec<_>,JobError>>()?;
        let params=json!(candidate.params);
        let persisted=persistence.call("replay",||db::persist_replay(pool,job,&params,manifest.clone(),&runs)).await?;
        drop(runs);drop(replay);
        if persisted.is_none() {return Err(interrupted());}
    }
    check_shutdown(shutdown)?;
    change_phase(pool,job,"analytics",persistence).await?;
    let stored=persistence.call("load_replay",||db::load_replay(pool,job,config.optimizer.max_working_bytes)).await?;
    let bootstrap=stored.get("bootstrap").filter(|value|value.is_object()).cloned().ok_or_else(||JobError::new("resume_bootstrap_missing","Durable replay has no compact bootstrap proof"))?;
    let persisted:ContinuousReplay=serde_json::from_value(stored).map_err(|e|JobError::new("invalid_persisted_replay",e.to_string()))?;
    let cpu_spec=spec.clone();let cpu_input=input.clone();let cpu_candidate=candidate.clone();let limits=config.optimizer.study_limits();
    let (evaluation,_)=blocking(cancel,None,move|token|core::analyze_continuous(&cpu_spec,&cpu_input,&persisted,&cpu_candidate,&limits,&token)).await?;
    let evaluation=evaluation.ok_or_else(interrupted)?;
    if evaluation.evaluation.matrix!=best.matrix || evaluation.evaluation.summary.selected_setup_count!=best.support.selected_setup_count {
        return Err(JobError::new("post_chain_mismatch","Сохранённый replay/аналитика не совпали с frozen исследованием"));
    }
    let recommendation=core::recommend_continuous(&spec,&candidate,&evaluation,&best.provenance,context,job.config["minMeanExpectancy"].as_f64().ok_or_else(||JobError::new("invalid_persisted_config","Нет mean gate"))?).map_err(compute_error)?;
    let selected=core::select_robust_cell(&evaluation.evaluation.matrix,&spec.tp_levels,&spec.sl_levels,spec.min_closed_per_cell).map_err(compute_error)?;
    let selected=selected.map(|cell|json!(cell));
    let mean=evaluation.evaluation.summary.score;
    let accepted=recommendation.is_some();
    let batch=persistence.call("batch_id",||async {sqlx::query_scalar::<_,i32>("select replay_batch_id from continuous_generations where id=$1").bind(job.generation_id).fetch_one(pool).await.map_err(Into::into)}).await?;
    let result=json!({"best":best,"selectedCell":selected,"meanExpectancy":mean,"analytics":evaluation,"recommendation":recommendation,"bootstrap":bootstrap,"setupSetId":format!("batch-{batch}")});
    drop(input);
    let gate=recovery::Gate {result,accepted,mean,selected};
    if !persistence.call("gate",||recovery::save_gate(pool,job,&identity,&gate)).await? {return Err(interrupted());}
    check_shutdown(shutdown)?;
    if !persistence.call("finalize",||db::finalize(pool,job,&gate.result,gate.accepted,gate.mean,gate.selected.as_ref())).await? {return Err(interrupted());}
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn search_resumable(pool:&PgPool,job:&ChainJob,spec:&StudyRequest,input:Arc<core::StudyInput>,context:ContinuousContext,config:&ContinuousRuntimeConfig,cancel:&CancellationToken,shutdown:&CancellationToken,checkpoint:Option<Vec<u8>>,deadline:chrono::DateTime<chrono::Utc>,identity:Value,persistence:&Persistence)->Result<core::StudyOutcome,JobError> {
    let cpu_spec=spec.clone();let limits=config.optimizer.study_limits();
    let (sender,mut receiver)=watch::channel(Value::Null);
    let core_cancel=Arc::new(CancelToken::new());let cpu_cancel=core_cancel.clone();
    let core_pause=Arc::new(CancelToken::new());let cpu_pause=core_pause.clone();
    let handle=tokio::runtime::Handle::current();let checkpoint_pool=pool.clone();let checkpoint_job=job.clone();let max_runtime=config.max_runtime_bytes;
    // A single owned error slot crosses the core callback's public StudyError
    // seam without string-parsing or a process-wide registry. Read after join.
    let checkpoint_error=Arc::new(std::sync::Mutex::new(None));let saved_error=checkpoint_error.clone();
    let checkpoint_persistence=persistence.clone();
    let mut search=tokio::task::spawn_blocking(move||core::search_continuous_resumable(&cpu_spec,&input,context,&limits,
        core::ContinuousSearchControl {cancel:&cpu_cancel,pause:&cpu_pause,remaining_budget:deadline.signed_duration_since(chrono::Utc::now()).to_std().unwrap_or(Duration::ZERO),resume_checkpoint:checkpoint.as_deref(),max_checkpoint_bytes:recovery::MAX_CHECKPOINT_BYTES},
        move|update|{sender.send_replace(json!(update.progress));},
        |info,bytes| {
            let operation=if info.progress.evaluated_candidates==0 {"initial_checkpoint"}else{"checkpoint"};
            if let Err(error)=handle.block_on(checkpoint_persistence.call(operation,||recovery::save_checkpoint(&checkpoint_pool,&checkpoint_job,&identity,bytes,max_runtime))) {
                *saved_error.lock().expect("owned checkpoint error slot poisoned")=Some(error);
                return Err(core::StudyError::Execution("checkpoint_persistence_failed".into()));
            }
            // Public progress is ancillary; only the checkpoint ACK gates core.
            Ok(if cpu_pause.is_cancelled() {core::CheckpointAction::Suspend}else{core::CheckpointAction::Continue})
        }));
    let mut progress_tick=tokio::time::interval(Duration::from_millis(500));
    let mut progress_error=None;
    let mut progress_degraded=false;
    let mut cancelled=false;let mut paused=false;
    progress_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let outcome=loop {
        tokio::select!{
            result=&mut search=>break result.map_err(|error|JobError::new("worker_failed",error.to_string()))?,
            _=cancel.cancelled(),if !cancelled=>{core_cancel.cancel();cancelled=true;},
            _=shutdown.cancelled(),if !paused=>{core_pause.cancel();paused=true;},
            _=progress_tick.tick()=>{
                let progress=receiver.borrow_and_update().clone();
                if !progress.is_null() && !cancel.is_cancelled() {
                    match persistence.call("progress",||db::phase(pool,job,"searching",Some(&progress))).await {
                        Ok(true)=>progress_degraded=false,Ok(false)=>cancel.cancel(),
                        Err(error) if error.code=="persistence_unconfirmed"=>{
                            if !progress_degraded {eprintln!("continuous progress update deferred: generation={}",job.generation_id);progress_degraded=true;}
                        },
                        Err(error)=>{progress_error=Some(error);cancel.cancel();}
                    }
                }
            }
        }
    };
    if let Some(error)=checkpoint_error.lock().expect("owned checkpoint error slot poisoned").take() {return Err(error);}
    if let Some(error)=progress_error {return Err(error);}
    let outcome=outcome.map_err(compute_error)?;
    match outcome {
        core::ContinuousSearchExit::Suspended(_)=>Err(suspended()),
        core::ContinuousSearchExit::Completed(outcome)=>{
            if outcome.reason==TerminationReason::Cancelled || cancel.is_cancelled() {return Err(interrupted());}
            let result=json!(outcome);
            persistence.call("finish_search",||db::finish_search(pool,job,&result,true)).await?;
            Ok(outcome)
        },
    }
}

fn replay_manifest(replay:&ContinuousReplay)->Value {
    json!({"fromUnix":replay.from_unix,"toUnix":replay.to_unix,"context":replay.context,"params":replay.params,"provenance":replay.provenance,
        "instruments":replay.instruments.iter().map(|raw|json!({"instrument":raw.instrument,"previousRawCloseUnix":raw.previous_raw_close_unix,"lastRawCloseUnix":raw.last_raw_close_unix,"evaluatedMinutes":raw.evaluated_minutes,"warmupMinutes":raw.warmup_minutes})).collect::<Vec<_>>()})
}

#[cfg(test)]
mod monitor_tests {
    use super::*;

    #[test]
    fn initial_confirmation_is_anchored_before_claim_not_monitor_creation() {
        let now=LeaseInstant::now();
        let proof=MonitorProof::new(now-Duration::from_secs(24));
        assert_eq!(proof.deadline(),now+Duration::from_secs(1));
    }

    #[test]
    fn heartbeat_ack_uses_request_start_and_cannot_revive_expired_permission() {
        let start=LeaseInstant::now();let mut proof=MonitorProof::new(start);
        assert!(proof.confirm(MonitorOperation::Heartbeat,start+Duration::from_secs(1),start+Duration::from_secs(3)));
        assert_eq!(proof.confirmed[0],start+Duration::from_secs(1));
        // Current/housekeeping still have their independent earlier bounds.
        assert_eq!(proof.deadline(),start+MONITOR_CONFIRMED_WINDOW);
        let mut stale=MonitorProof::new(start);
        assert!(!stale.confirm(MonitorOperation::Heartbeat,start+Duration::from_secs(24),start+Duration::from_secs(26)));
        assert_eq!(stale.confirmed[0],start);
    }

    #[test]
    fn successful_heartbeats_do_not_extend_unknown_current_or_housekeeping() {
        for missing in [MonitorOperation::Current,MonitorOperation::IdleStopDrain] {
            let start=LeaseInstant::now();let mut proof=MonitorProof::new(start);
            for seconds in [5,10,15,20,24] {
                let at=start+Duration::from_secs(seconds);
                for operation in [MonitorOperation::Heartbeat,MonitorOperation::Current,MonitorOperation::IdleStopDrain] {
                    if operation!=missing {assert!(proof.confirm(operation,at,at));}
                }
            }
            assert_eq!(proof.deadline(),start+MONITOR_CONFIRMED_WINDOW);
            assert_eq!(proof.expired().code,if missing==MonitorOperation::Current {"monitor_current_unconfirmed"}else{"monitor_idle_stop_unconfirmed"});
        }
    }

    #[test]
    fn successful_current_checks_do_not_extend_unknown_heartbeat() {
        let start=LeaseInstant::now();let mut proof=MonitorProof::new(start);
        for seconds in [5,10,15,20,24] {
            let at=start+Duration::from_secs(seconds);
            assert!(proof.confirm(MonitorOperation::Current,at,at));
            assert!(proof.confirm(MonitorOperation::IdleStopDrain,at,at));
        }
        assert_eq!(proof.deadline(),start+MONITOR_CONFIRMED_WINDOW);
        assert_eq!(proof.expired().code,"monitor_lease_unconfirmed");
    }

    #[test]
    fn finalizer_success_and_other_precise_errors_win_monitor_races() {
        assert!(chain_outcome(Ok(()),Some(JobError::new("continuous_fence_lost","final current=false"))).is_ok());
        let error=chain_outcome(Err(JobError::new("resource_limit","original limit")),Some(JobError::new("monitor_lease_unconfirmed","later"))).unwrap_err();
        assert_eq!(error.code,"resource_limit");assert_eq!(error.message,"original limit");
        assert_eq!(chain_outcome(Err(interrupted()),Some(JobError::new("monitor_current_unconfirmed","current timeout"))).unwrap_err().code,"monitor_current_unconfirmed");
    }

    #[test]
    fn database_issue_classification_does_not_expose_payloads_or_hide_invariants() {
        assert_eq!(monitor_db_cause(setups_db::DbError::Sqlx(sqlx::Error::PoolTimedOut)),("pool_acquire_timeout".into(),true));
        assert_eq!(monitor_db_cause(setups_db::DbError::Json("private diagnostic payload".into())),("invalid_database_state".into(),false));
        assert_eq!(monitor_db_cause(setups_db::DbError::Sqlx(sqlx::Error::PoolClosed)),("pool_closed".into(),false));
    }

    #[test]
    fn only_unknown_monitor_proofs_defer_and_definite_control_errors_do_not() {
        for code in ["monitor_lease_unconfirmed","monitor_current_unconfirmed","monitor_idle_stop_unconfirmed"] {
            assert!(uncertain(&chain_outcome(Err(interrupted()),Some(JobError::new(code,"unknown proof"))).unwrap_err()));
        }
        for code in ["continuous_lease_lost","continuous_fence_lost","monitor_database_error","evaluation_failed","resource_limit","chain_suspended"] {
            assert!(!uncertain(&JobError::new(code,"definite control or computation outcome")));
        }
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use sqlx::{Row,postgres::{PgConnectOptions,PgPoolOptions}};
    use std::{str::FromStr,sync::atomic::AtomicUsize};

    async fn isolated<F,Fut>(body:F) where F:FnOnce(PgPool)->Fut+'static,Fut:std::future::Future<Output=()>+'static {
        let options=PgConnectOptions::from_str(&std::env::var("TEST_DATABASE_URL").expect("isolated TEST_DATABASE_URL required")).unwrap();
        assert!(options.get_database().is_some_and(|name|name.ends_with("_test")));
        let admin=PgPoolOptions::new().max_connections(2).connect_with(options.clone()).await.unwrap();
        let name=format!("persistence_{}_{}_test",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap());
        sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
        let pool=PgPoolOptions::new().max_connections(8).connect_with(options.database(&name)).await.unwrap();let child=pool.clone();
        let result=tokio::task::LocalSet::new().run_until(async move {tokio::task::spawn_local(async move {
            let baseline=include_str!("../../../db/migrations/0010_baseline.sql").lines().filter(|line|!line.starts_with('\\')&&!line.starts_with("SET transaction_timeout")&&!line.contains("set_config('search_path'")).collect::<Vec<_>>().join("\n");
            sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
            sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','fixture')").execute(&child).await.unwrap();
            setups_db::migrate::ensure_migrations(&child).await.unwrap();body(child).await;
        }).await}).await;
        pool.close().await;sqlx::query(&format!("drop database {name} with(force)")).execute(&admin).await.unwrap();admin.close().await;
        if let Err(error)=result {std::panic::resume_unwind(error.into_panic());}
    }

    async fn committed_input(pool:&PgPool)->(ChainJob,Arc<core::StudyInput>,ContinuousRuntimeConfig) {
        let config=ContinuousRuntimeConfig::default();
        let request=json!({"lookbackDays":1.0/24.0,"minSetupsPerDay":[0],"minMeanExpectancy":0.15,"adaptiveEnabled":true,"study":{
            "algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","algorithmParams":{"priceGrowthPct":"0","windowMinutes":1,"baselineLongDays":1,"baselineShortDays":1,"volumeMultiplier":"0.000001","cooldownMinutes":1,"maxGapPercent":"1","maxConsecutiveGapMinutes":5,"requiredRecentMinutes":1},
            "algorithmSearch":{"cooldownMinutes":[1,2,3,4]},"analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":0,"priceChangePct":null},"analyticsSearch":{},
            "instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}],"direction":"short","tpLevels":[1.3],"slLevels":[0.7],"horizonMinutes":2,"minClosedPerCell":1,"maxTrials":4,"maxSeconds":120,"seed":42}});
        let normalized=crate::continuous::normalize_config(serde_json::from_value(request).unwrap(),"price_volume_spike",&config.optimizer).unwrap();
        setups_db::continuous::configure(pool,"admin","price_volume_spike",&json!(normalized),&[0.0]).await.unwrap();
        setups_db::continuous::start(pool,"admin","price_volume_spike").await.unwrap();
        let lease=db::claim_owner(pool,"runtime-fault-fixture").await.unwrap().unwrap();let job=db::prepare_next(pool,&lease).await.unwrap().unwrap();
        let spec:StudyRequest=serde_json::from_value(job.spec.clone()).unwrap();let context=ContinuousContext{observation_cutoff:job.observation_cutoff};
        let bounds=core::required_continuous_input_bounds(&spec,context).unwrap();
        let bars=((bounds.detector_from_unix+59).div_euclid(60)*60..bounds.detector_to_unix).step_by(60).map(|open_time_unix|setups_domain::DetectorBar{open_time_unix,close:"100.0000000000000001".into(),quote_turnover:"1000.0000000000000000".into()}).collect();
        let candles=((bounds.candles_from_unix+59).div_euclid(60)*60..bounds.candles_to_unix).step_by(60).map(|open_time_unix|setups_analytics::OhlcCandle{open_time_unix,open:100.0,high:100.0,low:98.0,close:100.0}).collect();
        let input=Arc::new(core::StudyInput{instruments:vec![core::InstrumentInput{instrument:spec.instruments[0].clone(),detector_bars:bars,candles}],snapshot_hash:format!("{:064x}",42),captured_at:"2026-11-14T00:00:00Z".into()});
        sqlx::query("update continuous_processes set retry_count=2,error='{\"code\":\"persistence_unconfirmed\"}'").execute(pool).await.unwrap();
        super::super::frozen::persist(pool,&job,input.clone(),config.optimizer.max_snapshot_bytes,config.max_runtime_bytes,config.max_database_bytes,&config.replay_admission,&CancellationToken::new()).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(pool).await.unwrap(),0,"a new committed manifest is durable advancement");
        sqlx::query("update continuous_processes set retry_count=2,error='{\"code\":\"persistence_unconfirmed\"}'").execute(pool).await.unwrap();
        super::super::frozen::persist(pool,&job,input.clone(),config.optimizer.max_snapshot_bytes,config.max_runtime_bytes,config.max_database_bytes,&config.replay_admission,&CancellationToken::new()).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(pool).await.unwrap(),2,"an exact seal retry must not reset failed recoveries");
        sqlx::query("update continuous_processes set retry_count=0,error=null").execute(pool).await.unwrap();
        setups_db::continuous::maintenance::set_paused(pool,"admin",true).await.unwrap();
        (job,input,config)
    }

    async fn fault_after_actual_commit(operation:&'static str,failures:usize) {
        isolated(move|pool|async move {
            let (job,input,config)=committed_input(&pool).await;
            let remaining=Arc::new(AtomicUsize::new(failures));let calls=Arc::new(AtomicUsize::new(0));
            let left=remaining.clone();let seen=calls.clone();
            let persistence=Persistence{after_success:Some(Arc::new(move|stage|{
                if stage==operation {
                    seen.fetch_add(1,Ordering::SeqCst);
                    // This hook runs ONLY after the real transaction helper
                    // returned success (COMMIT accepted), before its ACK is
                    // admitted to the owner/core. Subsequent DB calls are real.
                    let count=left.load(Ordering::SeqCst);
                    if count>0 && left.compare_exchange(count,count-1,Ordering::SeqCst,Ordering::SeqCst).is_ok() {
                        return Err(sqlx::Error::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset,"injected acknowledgement loss after actual COMMIT")).into());
                    }
                }
                Ok(())
            }))};
            let stop=CancellationToken::new();
            run_owner(&pool,&job.lease,"http://127.0.0.1:1","unused",&config,&stop,LeaseInstant::now(),&persistence).await.unwrap();
            assert_eq!(remaining.load(Ordering::SeqCst),0);assert!(calls.load(Ordering::SeqCst)>0);
            if failures==3 {
                let before=sqlx::query("select g.status,g.study_id,g.research_deadline,a.checkpoint_hash,p.retry_count,p.error from continuous_generations g join continuous_processes p on p.active_generation_id=g.id join continuous_runtime_artifacts a on a.generation_id=g.id").fetch_one(&pool).await.unwrap();
                assert_eq!(before.get::<String,_>("status"),"running");assert_eq!(before.get::<i64,_>("study_id"),job.study_id);
                assert_eq!(before.get::<chrono::DateTime<chrono::Utc>,_>("research_deadline"),job.research_deadline.unwrap());
                assert_eq!(before.get::<i32,_>("retry_count"),0,"failure handler cannot charge an uncertain DB transaction");
                assert_eq!(before.get::<Value,_>("error")["code"],"persistence_unconfirmed");
                assert_eq!(before.get::<Option<String>,_>("checkpoint_hash").is_some(),operation=="checkpoint");
                db::release(&pool,&job.lease).await.unwrap();
                assert!(db::claim_owner(&pool,"backoff").await.unwrap().is_none());
                sqlx::query("update continuous_processes set next_attempt_at=now()-interval '1 second'").execute(&pool).await.unwrap();
                let lease=db::claim_owner(&pool,"after-lost-ack").await.unwrap().unwrap();
                run_owner(&pool,&lease,"http://127.0.0.1:1","unused",&config,&stop,LeaseInstant::now(),&Persistence::default()).await.unwrap();
            }else{
                assert!(calls.load(Ordering::SeqCst)>=2,"actual committed head must be read/adopted by the idempotent retry");
            }
            let completed=sqlx::query("select status,study_id,execution_started_at,research_deadline,observation_cutoff,spec,result,error from continuous_generations where id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap();
            assert_eq!(completed.get::<String,_>("status"),"completed");assert_eq!(completed.get::<i64,_>("study_id"),job.study_id);
            assert_eq!(completed.get::<chrono::DateTime<chrono::Utc>,_>("execution_started_at"),job.execution_started_at);
            assert_eq!(completed.get::<chrono::DateTime<chrono::Utc>,_>("research_deadline"),job.research_deadline.unwrap());
            assert_eq!(completed.get::<i64,_>("observation_cutoff"),job.observation_cutoff);assert_eq!(completed.get::<Value,_>("spec"),job.spec);
            let result=completed.get::<Value,_>("result");assert_eq!(result["best"]["provenance"]["snapshotHash"],input.snapshot_hash);
            assert_eq!(result["best"]["matrix"],result["analytics"]["evaluation"]["matrix"]);
            let study=setups_db::optimization::get(&pool,job.study_id).await.unwrap().unwrap();let outcome:core::StudyOutcome=serde_json::from_value(study.result.unwrap()).unwrap();
            assert_eq!(outcome.progress.evaluated_candidates,4);assert_eq!(outcome.detector_passes,4,"ACK retries must not reevaluate committed proposals");
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_generations").fetch_one(&pool).await.unwrap(),1);
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_batches").fetch_one(&pool).await.unwrap(),1);
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_runs").fetch_one(&pool).await.unwrap(),1);
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_runtime_artifacts").fetch_one(&pool).await.unwrap(),0);
            assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(&pool).await.unwrap(),0);
        }).await;
    }
    #[tokio::test]
    #[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
    async fn checkpoint_commit_ack_loss_is_adopted_by_real_owner_without_losing_frontier() {fault_after_actual_commit("checkpoint",1).await;}
    #[tokio::test]
    #[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
    async fn replay_commit_ack_loss_is_adopted_by_real_owner_without_duplicate_batch() {fault_after_actual_commit("replay",1).await;}
    #[tokio::test]
    #[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
    async fn exhausted_checkpoint_ack_retries_defer_then_resume_same_original_input_and_deadline() {fault_after_actual_commit("checkpoint",3).await;}
    #[tokio::test]
    #[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
    async fn ancillary_phase_ack_failure_preserves_head_for_real_owner_recovery() {fault_after_actual_commit("phase",3).await;}
}
