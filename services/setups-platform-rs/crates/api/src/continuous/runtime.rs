//! One retained frozen input per leased owner, through search -> durable replay -> analytics.
use super::ContinuousRuntimeConfig;
use crate::optimization::{JobError,snapshot};
use serde_json::{Value,json};
use setups_db::continuous::scheduler::{self as db,ChainJob,OwnerLease,ServiceReplayRun};
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
                        if let Err(error)=run_owner(&pool,&lease,&url,&key,&config,&stop,claim_started).await {
                            eprintln!("continuous owner failed: owner={} code={}",lease.owner_id,error.code);
                        }
                        if let Err(error)=db::release(&pool,&lease).await {eprintln!("continuous lease release failed: {error}");}
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
fn interrupted()->JobError {JobError::new("chain_interrupted","Цепочка прервана; незавершённая стадия не является непрохождением")}
fn compute_error(error:core::StudyError)->JobError {
    let code=if matches!(error,core::StudyError::ResourceLimit{..}){"resource_limit"}else{"evaluation_failed"};
    JobError::new(code,error.to_string())
}

async fn run_owner(pool:&PgPool,lease:&OwnerLease,url:&str,key:&str,config:&ContinuousRuntimeConfig,stop:&CancellationToken,claim_started:LeaseInstant)->Result<(),JobError> {
    db::recover_owner(pool,lease).await.map_err(database)?;
    if stop.is_cancelled() {return Ok(());}
    let Some(job)=db::prepare_next(pool,lease).await.map_err(database)? else {return Ok(());};
    let started=Instant::now();
    println!("continuous chain start: owner={} generation={} study={} revision={} leaseToken={} phase=acquiring",lease.owner_id,job.generation_id,job.study_id,job.config_revision,lease.token);
    let cancel=CancellationToken::new();
    // Keep renewing while cooperative CPU/materialization work joins, including Stop.
    let monitor_stop=CancellationToken::new();
    let monitor=tokio::spawn(monitor_job(pool.clone(),job.clone(),stop.clone(),cancel.clone(),monitor_stop.clone(),claim_started));
    let outcome=run_chain(pool,&job,url,key,config,&cancel).await;
    monitor_stop.cancel();
    let monitor_reason=monitor.await.map_err(|error|JobError::new("monitor_failed",error.to_string()))?;
    // A final current=false can race a successful finalizer clearing its pointer.
    // Never replace success or another precise computation/database error.
    let outcome=chain_outcome(outcome,monitor_reason);
    let outcome_kind=if outcome.is_ok() {"completed"}else{"interrupted_or_failed"};
    if let Err(error)=outcome {
        eprintln!("continuous chain failed: generation={} code={}",job.generation_id,error.code);
        db::fail(pool,&job,&json!({"code":if stop.is_cancelled(){"interrupted_by_restart"}else{error.code},"message":error.message})).await.map_err(database)?;
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
async fn monitor_job(pool:PgPool,job:ChainJob,shutdown:CancellationToken,cancel:CancellationToken,stop:CancellationToken,claim_started:LeaseInstant)->Option<JobError> {
    let mut interval=tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut proof=MonitorProof::new(claim_started);
    let mut reason=None;
    let mut last_heartbeat=None;
    'monitor: loop {
        tokio::select!{biased;
            _=stop.cancelled()=>return reason,
            _=shutdown.cancelled(),if reason.is_none()=>{monitor_cancel(&mut reason,JobError::new("interrupted_by_restart","Цепочка прервана остановкой сервера"),&cancel,&pool,&job,&proof);},
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
                _=shutdown.cancelled(),if reason.is_none()=>{monitor_cancel(&mut reason,JobError::new("interrupted_by_restart","Цепочка прервана остановкой сервера"),&cancel,&pool,&job,&proof);continue 'monitor;},
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

async fn change_phase(pool:&PgPool,job:&ChainJob,phase:&str)->Result<(),JobError> {
    if db::phase(pool,job,phase,None).await.map_err(database)? {
        println!("continuous chain phase: generation={} study={} phase={phase}",job.generation_id,job.study_id);
        Ok(())
    }else{Err(interrupted())}
}

async fn run_chain(pool:&PgPool,job:&ChainJob,url:&str,key:&str,config:&ContinuousRuntimeConfig,cancel:&CancellationToken)->Result<(),JobError> {
    let spec:StudyRequest=serde_json::from_value(job.spec.clone()).map_err(|e|JobError::new("invalid_persisted_spec",e.to_string()))?;
    let context=ContinuousContext{observation_cutoff:job.observation_cutoff};
    core::validate_continuous_request(&spec,context).map_err(compute_error)?;
    let estimated=config.optimizer.max_setups_per_candidate.saturating_mul(4096)
        .saturating_add(spec.tp_levels.len().saturating_mul(spec.sl_levels.len()).saturating_mul(4096))
        .saturating_add(config.optimizer.max_request_bytes.saturating_mul(2)).saturating_add(8*1024*1024);
    let budget=db::reserve_storage(pool,job,i64::try_from(estimated).unwrap_or(i64::MAX),config.max_database_bytes,&config.replay_admission).await.map_err(database)?.ok_or_else(interrupted)?;
    if !budget.admitted {return Err(JobError::new("resource_limit",format!("Continuous database budget exceeded: limit={}, requested={}; completed history is preserved",budget.limit,budget.requested)));}
    let deadline=tokio::time::Instant::now()+Duration::from_secs(spec.max_seconds);
    let input=tokio::select!{
        result=snapshot::acquire_continuous(&spec,context,url,key,&config.optimizer)=>Arc::new(result?),
        _=cancel.cancelled()=>return Err(interrupted()),
        _=tokio::time::sleep_until(deadline)=>return Err(JobError::new("acquisition_timeout","Истёк бюджет acquisition; отсутствие снимка не считается непрохождением")),
    };
    change_phase(pool,job,"searching").await?;
    let cpu_spec=spec.clone();let cpu_input=input.clone();let limits=config.optimizer.study_limits();
    let (sender,mut receiver)=watch::channel(Value::Null);
    let search=blocking(cancel,Some(deadline),move|token|core::search_continuous(&cpu_spec,&cpu_input,context,&limits,&token,move|update|{sender.send_replace(json!(update.progress));}));
    tokio::pin!(search);
    let mut progress_tick=tokio::time::interval(Duration::from_millis(500));
    let mut progress_error=None;
    progress_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (mut outcome,timed_out)=loop {
        tokio::select!{
            result=&mut search=>break result?,
            _=progress_tick.tick()=>{
                let progress=receiver.borrow_and_update().clone();
                if !progress.is_null() && !cancel.is_cancelled() {
                    // Failure cancels through the outer monitor; still join search before returning.
                    match db::phase(pool,job,"searching",Some(&progress)).await {
                        Ok(true)=>{},Ok(false)=>cancel.cancel(),
                        Err(error)=>{progress_error=Some(error.to_string());cancel.cancel();}
                    }
                }
            }
        }
    };
    if let Some(error)=progress_error {return Err(database(error));}
    if timed_out && !cancel.is_cancelled() {
        outcome.reason=TerminationReason::TimeBudget;
        if let Some(best)=&mut outcome.best {best.partial=false;}
    }
    let completed=outcome.reason!=TerminationReason::Cancelled;
    db::finish_search(pool,job,&json!(outcome),completed).await.map_err(database)?;
    if !completed || cancel.is_cancelled() {return Err(interrupted());}
    let Some(best)=outcome.best else {
        db::finalize(pool,job,&json!({"best":null,"reason":"no_winner"}),false,None,None).await.map_err(database)?;
        return Ok(());
    };
    let candidate=StudyCandidate{params:best.params.clone(),analytics:core::AnalyticsParams{
        cluster_enabled:best.analytics.cluster_enabled,cluster_gap_minutes:best.analytics.cluster_gap_minutes.unwrap_or(spec.analytics_params.cluster_gap_minutes),
        time_shift_minutes:best.analytics.time_shift_minutes,price_change_pct:best.analytics.price_change_pct,
    }};
    change_phase(pool,job,"replay").await?;
    let cpu_spec=spec.clone();let cpu_input=input.clone();let cpu_candidate=candidate.clone();let limits=config.optimizer.study_limits();
    let (prepared,_)=blocking(cancel,None,move|token| {
        let Some(replay)=core::materialize_continuous(&cpu_spec,&cpu_input,&cpu_candidate,context,&limits,&token)? else {return Ok(None);};
        let bootstrap=core::continuous_bootstrap_with_frozen_prefix(&cpu_spec,&replay,&cpu_input)?;
        Ok(Some((replay,bootstrap)))
    }).await?;
    let (replay,bootstrap)=prepared.ok_or_else(interrupted)?;
    let manifest=replay_manifest(&replay);
    let runs=replay.instruments.iter().map(|raw|Ok(ServiceReplayRun{
        exchange:raw.instrument.exchange.clone(),category:raw.instrument.category.clone(),symbol:raw.instrument.symbol.clone(),
        setups:serde_json::to_value(&raw.setups).map_err(|e|JobError::new("serialization_failed",e.to_string()))?,
        warmup_minutes:raw.warmup_minutes as i64,evaluated_minutes:raw.evaluated_minutes as i64,
    })).collect::<Result<Vec<_>,JobError>>()?;
    let persisted=db::persist_replay(pool,job,&json!(candidate.params),manifest,&runs).await.map_err(database)?;
    drop(runs);drop(replay);
    if persisted.is_none() {return Err(interrupted());}
    change_phase(pool,job,"analytics").await?;
    let stored=db::load_replay(pool,job,config.optimizer.max_working_bytes).await.map_err(database)?;
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
    let result=json!({"best":best,"selectedCell":selected,"meanExpectancy":mean,"analytics":evaluation,"recommendation":recommendation,"bootstrap":bootstrap,"setupSetId":format!("batch-{}",persisted_batch_id(pool,job).await?)});
    drop(input); // Compact prefix fingerprints are captured; the raw snapshot is no longer retained.
    db::finalize(pool,job,&result,accepted,mean,selected.as_ref()).await.map_err(database)?;
    Ok(())
}

async fn persisted_batch_id(pool:&PgPool,job:&ChainJob)->Result<i64,JobError> {
    sqlx::query_scalar::<_,i32>("select replay_batch_id from continuous_generations where id=$1").bind(job.generation_id).fetch_one(pool).await.map(i64::from).map_err(database)
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
}
