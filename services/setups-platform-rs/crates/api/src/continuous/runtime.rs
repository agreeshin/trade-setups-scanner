//! One retained frozen input per leased owner, through search -> durable replay -> analytics.
use super::ContinuousRuntimeConfig;
use crate::optimization::{JobError,snapshot};
use serde_json::{Value,json};
use setups_db::continuous::scheduler::{self as db,ChainJob,OwnerLease,ServiceReplayRun};
use setups_optimizer::{self as core,CancelToken,StudyRequest,StudyCandidate,ContinuousContext,ContinuousReplay,TerminationReason};
use sqlx::PgPool;
use std::{sync::{Arc,atomic::{AtomicBool,Ordering}},time::{Duration,Instant}};
use tokio::sync::{Notify,watch};
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
    let stop=CancellationToken::new();
    let notify=Arc::new(Notify::new());
    let available=Arc::new(AtomicBool::new(true));
    let mut tasks=Vec::with_capacity(config.workers);
    for index in 0..config.workers {
        let pool=pool.clone();let url=market_url.clone();let key=market_key.clone();let config=config.clone();
        let stop=stop.clone();let notify=notify.clone();let available=available.clone();
        let worker=format!("continuous-{}-{:016x}-{index}",std::process::id(),rand::random::<u64>());
        tasks.push(tokio::spawn(async move {
            while !stop.is_cancelled() {
                match db::claim_owner(&pool,&worker).await {
                    Ok(Some(lease))=>{
                        available.store(true,Ordering::SeqCst);
                        if let Err(error)=run_owner(&pool,&lease,&url,&key,&config,&stop).await {
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

async fn run_owner(pool:&PgPool,lease:&OwnerLease,url:&str,key:&str,config:&ContinuousRuntimeConfig,stop:&CancellationToken)->Result<(),JobError> {
    db::recover_owner(pool,lease).await.map_err(database)?;
    if stop.is_cancelled() {return Ok(());}
    let Some(job)=db::prepare_next(pool,lease).await.map_err(database)? else {return Ok(());};
    let started=Instant::now();
    println!("continuous chain start: owner={} generation={} study={}",lease.owner_id,job.generation_id,job.study_id);
    let cancel=CancellationToken::new();
    // A dedicated monitor cannot stop polling the pending materialization/CPU join.
    let monitor_stop=CancellationToken::new();
    let monitor=tokio::spawn(monitor_job(pool.clone(),job.clone(),stop.clone(),cancel.clone(),monitor_stop.clone()));
    let outcome=run_chain(pool,&job,url,key,config,&cancel).await;
    monitor_stop.cancel();
    if let Err(error)=monitor.await {return Err(JobError::new("monitor_failed",error.to_string()));}
    // All blocking stages have JOINED, and run_chain's final Arc<StudyInput> is gone.
    if let Err(error)=outcome {
        eprintln!("continuous chain failed: generation={} code={}",job.generation_id,error.code);
        db::fail(pool,&job,&json!({"code":if stop.is_cancelled(){"interrupted_by_restart"}else{error.code},"message":error.message})).await.map_err(database)?;
    }
    db::drain_stops(pool,lease).await.map_err(database)?;
    println!("continuous chain resources released: generation={} elapsedSeconds={:.3}",job.generation_id,started.elapsed().as_secs_f64());
    Ok(())
}

async fn monitor_job(pool:PgPool,job:ChainJob,shutdown:CancellationToken,cancel:CancellationToken,stop:CancellationToken) {
    let mut interval=tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_heartbeat=Instant::now();
    loop {
        tokio::select!{_=stop.cancelled()=>break,_=interval.tick()=>{}}
        if shutdown.is_cancelled() {cancel.cancel();}
        if last_heartbeat.elapsed()>=Duration::from_secs(1) {
            if !matches!(tokio::time::timeout(Duration::from_secs(2),db::heartbeat(&pool,&job.lease)).await,Ok(Ok(true))) {cancel.cancel();}
            last_heartbeat=Instant::now();
        }
        if !matches!(tokio::time::timeout(Duration::from_secs(2),db::current(&pool,&job)).await,Ok(Ok(true))) {cancel.cancel();}
        if let Err(error)=db::drain_idle_stops(&pool,&job.lease,&job.algorithm_id).await {eprintln!("continuous idle Stop drain failed: {error}");cancel.cancel();}
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
    if db::phase(pool,job,phase,None).await.map_err(database)? {Ok(())}else{Err(interrupted())}
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
    let (replay,_)=blocking(cancel,None,move|token|core::materialize_continuous(&cpu_spec,&cpu_input,&cpu_candidate,context,&limits,&token)).await?;
    let replay=replay.ok_or_else(interrupted)?;
    let bootstrap=core::continuous_bootstrap(&spec,&replay).map_err(compute_error)?;
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
    drop(input); // Release the only frozen snapshot before terminal activation/persistence retries.
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
