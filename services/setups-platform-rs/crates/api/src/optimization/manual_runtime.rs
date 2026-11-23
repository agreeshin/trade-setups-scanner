//! Resumable manual executor. The DB reservation owns the research lane; the
//! detached runtime lock only authorizes takeover. No browser owns this work.
use super::*;
use setups_db::optimization::manual::{self as storage,Job};
use setups_optimizer::{self as core,ManualSearchControl,ManualSearchExit,CheckpointAction};

fn database(error:setups_db::DbError)->JobError {
    let code=if setups_db::continuous::recovery::is_transient(&error){"persistence_unconfirmed"}
        else if error.to_string().contains("resource_limit"){"resource_limit"}else{"manual_storage_invalid"};
    JobError::new(code,error.to_string())
}
fn engine(error:core::StudyError)->JobError {JobError::new(if matches!(error,core::StudyError::ResourceLimit{..}){"resource_limit"}else{"evaluation_failed"},error.to_string())}
fn config(job:&Job,fallback:&OptimizationConfig)->Result<OptimizationConfig,JobError> {
    job.limits.clone().map(serde_json::from_value).transpose().map_err(|e|JobError::new("manual_limits_incompatible",e.to_string())).map(|c|c.unwrap_or_else(||fallback.clone()))
}
fn identity(job:&Job,config:&OptimizationConfig)->Value {
    json!({"version":1,"ownerId":job.study.owner_id,"studyId":job.study.id,
        "specHash":setups_db::continuous::frozen::checksum(&serde_json::to_vec(&job.study.spec).expect("JSON spec")),
        "optimizerVersion":core::OPTIMIZER_VERSION,"checkpointVersion":core::MANUAL_CHECKPOINT_VERSION,
        "wordBits":usize::BITS,"codec":"framed-cbor-1","limits":config})
}
fn archive(archive:Option<core::ManualOriginalArchive>)->Option<setups_db::study_display::ManualArchive> {
    archive.map(|archive|setups_db::study_display::ManualArchive{observation_cutoff:archive.observation_cutoff,
        tp_pct:archive.minutes.tp_pct,sl_pct:archive.minutes.sl_pct,kernel:archive.minutes.evaluator,
        minutes:archive.minutes.points.into_iter().map(|p|setups_db::study_display::OriginalMinute{
            signal_close_unix:p.signal_close_unix,sum_r:p.sum_r,closed_count:p.closed_count,tp_count:p.tp_count,sl_count:p.sl_count,
        }).collect()})
}
enum Exit {Completed(StudyOutcome,Option<core::ManualOriginalArchive>),Suspended,Deferred}

pub(super) async fn execute(pool:&PgPool,job:Job,url:&str,key:&str,fallback:&OptimizationConfig,stop:&CancellationToken,available:&AtomicBool) {
    let spec:Result<StudyRequest,_>=serde_json::from_value(job.study.spec.clone());
    let mut latest=Latest{progress:spec.as_ref().map(initial_progress).unwrap_or(StudyProgress{phase:StudyPhase::Acquiring,evaluated_candidates:0,scorable_candidates:0,max_trials:0,elapsed_seconds:0.0,best_score:None}),events:VecDeque::new()};
    latest.events=serde_json::from_value(job.study.events.clone()).unwrap_or_default();
    while latest.events.len()>MAX_EVENTS {latest.events.pop_front();}
    let outcome=match spec {
        Ok(spec)=>loop {
            let result=inner(pool,&job,spec.clone(),url,key,fallback,stop,&mut latest).await;
            if stop.is_cancelled() {break Ok(Exit::Deferred);}
            if result.as_ref().is_err_and(|error|error.code=="persistence_unconfirmed") {
                available.store(false,Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(500)).await;continue;
            }
            break result;
        },
        Err(error)=>Err(JobError::new("invalid_persisted_spec",error.to_string())),
    };
    latest.progress.elapsed_seconds=job.elapsed_ms() as f64/1000.0;
    match outcome {
        Ok(Exit::Deferred)=>{},
        Ok(Exit::Suspended)=>{
            latest.event("paused","Исследование приостановлено; рабочая память освобождена");
            loop {match storage::pause_ack(pool,&job,&json!(latest.progress),&json!(latest.events)).await {
                Ok(true)=>{available.store(true,Ordering::SeqCst);break;},
                Ok(false)=>{ // Stop committed after the checkpoint but before pause ACK.
                    settle_stop(pool,job,fallback,stop,available).await;
                    break;
                },
                Err(error)=>{
                    available.store(false,Ordering::SeqCst);
                    if stop.is_cancelled(){break;}
                    if !setups_db::continuous::recovery::is_transient(&error) && !matches!(error,setups_db::DbError::Sqlx(_)) {
                        let payload=TerminalPayload{id:job.study.id,status:"failed",progress:json!(latest.progress),result:None,archive:None,error:Some(json!({"code":"manual_pause_invalid","message":bounded_text(&error.to_string(),400)})),events:json!(latest.events)};
                        terminal(pool,&job,payload,stop,available).await;break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                },
            }}
        },
        result=>{
            let(status,result,error,original)=match result {
                Ok(Exit::Completed(mut outcome,original))=>{
                    outcome.progress=latest.progress.clone();let status=if outcome.reason==TerminationReason::Cancelled{"cancelled"}else{"completed"};
                    latest.event(status,if status=="cancelled"{"Исследование остановлено; сохранён завершённый частичный результат"}else{"Исследование завершено"});
                    (status,Some(json!(outcome)),None,archive(original))
                },
                Err(error)=>{
                    latest.event("failed",&error.message);
                    ("failed",None,Some(json!({"code":error.code,"message":error.message})),None)
                },
                _=>unreachable!(),
            };
            terminal(pool,&job,TerminalPayload{id:job.study.id,status,progress:json!(latest.progress),result,error,archive:original,events:json!(latest.events)},stop,available).await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn inner(pool:&PgPool,job:&Job,spec:StudyRequest,url:&str,key:&str,fallback:&OptimizationConfig,stop:&CancellationToken,latest:&mut Latest)->Result<Exit,JobError> {
    let config=config(job,fallback)?;let expected_identity=identity(job,&config);
    let mut existing=storage::artifact(pool,job,&config.manual).await.map_err(database)?;
    if existing.as_ref().is_some_and(|a|a.identity!=expected_identity) {return Err(JobError::new("manual_identity_incompatible","Private execution identity changed"));}
    let checkpoint=existing.as_mut().and_then(|a|a.checkpoint.take());
    let input=if let Some(snapshot)=existing.as_ref().and_then(|a|a.manifest.as_ref()) {
        match super::manual_frozen::load(pool,job,snapshot,config.max_snapshot_bytes,stop).await {
            Ok(input)=>input,
            Err(error) if error.code=="manual_snapshot_cancelled"=>return Ok(Exit::Suspended),
            Err(error)=>return Err(error),
        }
    }else {
        // Reserve checkpoint replacement headroom BEFORE any search can begin.
        storage::reserve(pool,job,&expected_identity,config.max_snapshot_bytes.saturating_mul(3),&config.manual).await.map_err(database)?;
        let budget_ms=(spec.max_seconds.saturating_mul(1000) as i64).saturating_sub(job.elapsed_ms()).max(0) as u64;
        if budget_ms==0 {return Ok(Exit::Completed(empty_outcome(latest,TerminationReason::TimeBudget),None));}
        let deadline=tokio::time::Instant::now()+Duration::from_millis(budget_ms);
        let acquiring=snapshot::acquire(&spec,url,key,&config);tokio::pin!(acquiring);
        let input=loop {
            tokio::select!{biased;
                _=stop.cancelled()=>return Ok(Exit::Deferred),
                _=tokio::time::sleep_until(deadline)=>return Ok(Exit::Completed(empty_outcome(latest,TerminationReason::TimeBudget),None)),
                result=&mut acquiring=>break result?,
                _=tokio::time::sleep(POLL)=>{
                    let status=storage::status(pool,job).await.map_err(database)?;
                    if status=="cancel_requested" {return Ok(Exit::Completed(empty_outcome(latest,TerminationReason::Cancelled),None));}
                    // Pause waits for complete acquisition; it never silently
                    // replaces a committed snapshot with a later market read.
                    latest.progress.elapsed_seconds=job.elapsed_ms() as f64/1000.0;
                }
            }
        };
        let input=Arc::new(input);let cutoff=core::manual_forecast_observation_cutoff(&spec,&input).map_err(engine)?;
        if let Err(error)=super::manual_frozen::persist(pool,job,input.clone(),&expected_identity,cutoff,config.max_snapshot_bytes,&config.manual,stop).await {
            if error.code=="manual_snapshot_cancelled"{return Ok(Exit::Completed(empty_outcome(latest,TerminationReason::Cancelled),None));}
            return Err(error);
        }
        Arc::try_unwrap(input).map_err(|_|JobError::new("manual_input_ownership","Frozen codec retained input after join"))?
    };
    latest.event("snapshot_ready","Неизменный снимок подтверждён; продолжение поиска");
    let cancel=Arc::new(CancelToken::new());let pause=Arc::new(CancelToken::new());
    let state=storage::status(pool,job).await.map_err(database)?;
    if state=="cancel_requested" {cancel.cancel();}else if state=="pausing" || stop.is_cancelled(){pause.cancel();}
    let budget_ms=(spec.max_seconds.saturating_mul(1000) as i64).saturating_sub(job.elapsed_ms()).max(0) as u64;
    let remaining=Duration::from_millis(budget_ms);
    let runtime=tokio::runtime::Handle::current();let db_pool=pool.clone();let db_job=job.clone();
    let callback_cancel=cancel.clone();let callback_pause=pause.clone();let cpu_cancel=cancel.clone();let cpu_pause=pause.clone();
    let limits=config.study_limits();let storage_limits=config.manual.clone();let max_checkpoint_bytes=config.manual.max_checkpoint_bytes as usize;
    let mut progress_state=latest.clone();let progress_job=job.clone();let(sender,receiver)=watch::channel(progress_state.clone());
    let shutdown=stop.clone();
    let mut cpu=tokio::task::spawn_blocking(move|| {
        core::search_manual_resumable(&spec,&input,&limits,ManualSearchControl{cancel:&cpu_cancel,pause:&cpu_pause,remaining_budget:remaining,resume_checkpoint:checkpoint.as_deref(),max_checkpoint_bytes},
            move|update|{progress_state.merge(update,0.0);progress_state.progress.elapsed_seconds=progress_job.elapsed_ms() as f64/1000.0;sender.send_replace(progress_state.clone());},
            |info,bytes|{
                let saved=loop {
                    match runtime.block_on(storage::checkpoint(&db_pool,&db_job,info.sequence,bytes,&storage_limits)) {
                        Ok(status)=>break status,
                        Err(error) if setups_db::continuous::recovery::is_transient(&error)=>{
                            if shutdown.is_cancelled(){return Err(core::StudyError::Execution("manual_checkpoint_shutdown_deferred".into()));}
                            std::thread::sleep(Duration::from_millis(500));
                        },
                        Err(error)=>return Err(core::StudyError::Execution(format!("manual_checkpoint_persistence: {error}"))),
                    }
                };
                match saved.as_str(){"cancel_requested"=>callback_cancel.cancel(),"pausing"=>callback_pause.cancel(),_=>{}}
                Ok(if callback_pause.is_cancelled(){CheckpointAction::Suspend}else{CheckpointAction::Continue})
            })
    });
    let mut polling=tokio::time::interval(POLL);polling.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut persistence_error=None;
    let result=loop {
        tokio::select!{
            result=&mut cpu=>break result,
            _=polling.tick()=>{
                if stop.is_cancelled(){pause.cancel();}
                match storage::status(pool,job).await {
                    Ok(status) if status=="cancel_requested"=>cancel.cancel(),
                    Ok(status) if status=="pausing"=>pause.cancel(),
                    Ok(_)=>{},Err(error)=>{if !setups_db::continuous::recovery::is_transient(&error){persistence_error=Some(database(error));cancel.cancel();}},
                }
                *latest=receiver.borrow().clone();
                if persistence_error.is_none(){if let Err(error)=storage::save_progress(pool,job,&json!(latest.progress),&json!(latest.events)).await {if !setups_db::continuous::recovery::is_transient(&error){persistence_error=Some(database(error));cancel.cancel();}}}
            }
        }
    };
    *latest=receiver.borrow().clone();
    if let Some(error)=persistence_error {return Err(error);}
    match result.map_err(|e|JobError::new("manual_worker_join",e.to_string()))?.map_err(engine)? {
        ManualSearchExit::Completed{outcome,original_archive}=>Ok(Exit::Completed(outcome,original_archive)),
        ManualSearchExit::Suspended(_)=>Ok(if stop.is_cancelled(){Exit::Deferred}else{Exit::Suspended}),
    }
}

pub(super) async fn settle_one_stop(pool:&PgPool,config:&OptimizationConfig,stop:&CancellationToken,available:&AtomicBool) {
    match storage::pending_stop(pool).await {
        Ok(Some(job))=>settle_stop(pool,job,config,stop,available).await,
        Ok(None)=>{},Err(error)=>eprintln!("manual stopped-head lookup failed: {error}"),
    }
}
pub(super) async fn settle_stops(pool:&PgPool,config:&OptimizationConfig,stop:&CancellationToken,available:&AtomicBool) {
    while !stop.is_cancelled(){
        settle_one_stop(pool,config,stop,available).await;
        tokio::select!{_=stop.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(250))=>{}}
    }
}
async fn settle_stop(pool:&PgPool,job:Job,fallback:&OptimizationConfig,stop:&CancellationToken,available:&AtomicBool) {
    // The joined executor remains responsible when Stop beats pause ACK. In
    // that race its reservation is still true, so handing a transient read to
    // the non-reserved settlement scanner would permanently strand the lane.
    loop {
    let result=async {
        let config=config(&job,fallback)?;let spec:StudyRequest=serde_json::from_value(job.study.spec.clone()).map_err(|e|JobError::new("invalid_persisted_spec",e.to_string()))?;
        let state=storage::artifact(pool,&job,&config.manual).await.map_err(database)?;
        if let Some(state)=state {
            if state.identity!=identity(&job,&config){return Err(JobError::new("manual_identity_incompatible","Private identity changed"));}
            if let (Some(bytes),Some(manifest),Some(cutoff))=(state.checkpoint,state.manifest,state.observation_cutoff) {
                let limits=config.study_limits();let max=config.manual.max_checkpoint_bytes as usize;
                return tokio::task::spawn_blocking(move||core::export_manual_checkpoint_partial(&spec,core::ManualCheckpointIdentity{snapshot_hash:&manifest.snapshot_hash,captured_at:&manifest.captured_at,observation_cutoff:cutoff},&limits,&bytes,max))
                    .await.map_err(|e|JobError::new("manual_partial_join",e.to_string()))?.map_err(engine);
            }
        }
        let mut progress=initial_progress(&spec);progress.elapsed_seconds=job.elapsed_ms() as f64/1000.0;
        Ok((StudyOutcome{progress,best:None,reason:TerminationReason::Cancelled,baseline:None,invalid_proposals:0,detector_passes:0},None))
    }.await;
    let(status,result,error,archive)=match result {
        Ok((mut outcome,original))=>{outcome.progress.elapsed_seconds=job.elapsed_ms() as f64/1000.0;("cancelled",Some(json!(outcome)),None,archive(original))},
        Err(error) if error.code=="persistence_unconfirmed"=>{
            if stop.is_cancelled(){return;}
            tokio::time::sleep(Duration::from_millis(500)).await;continue;
        },
        Err(error)=>("failed",None,Some(json!({"code":error.code,"message":error.message})),None),
    };
    let progress=result.as_ref().map(|v|v["progress"].clone()).unwrap_or_else(||job.study.progress.clone());
    terminal(pool,&job,TerminalPayload{id:job.study.id,status,progress,result,error,archive,events:job.study.events.clone()},stop,available).await;
    return;
    }
}
async fn terminal(pool:&PgPool,job:&Job,mut payload:TerminalPayload,stop:&CancellationToken,available:&AtomicBool) {
    if payload.result.as_ref().is_some_and(|v|serde_json::to_vec(v).map_or(true,|b|b.len()>8*1024*1024)){
        payload.status="failed";payload.result=None;payload.archive=None;payload.error=Some(json!({"code":"resource_limit","message":"Manual result exceeds byte cap"}));
    }
    let mut attempt=0u64;let mut shutdown_deadline=None;
    loop {
        if stop.is_cancelled(){shutdown_deadline.get_or_insert_with(||tokio::time::Instant::now()+Duration::from_secs(2));}
        if shutdown_deadline.is_some_and(|deadline|tokio::time::Instant::now()>=deadline){return;}
        let result=db::finish_fenced(pool,payload.id,Some(job.epoch),payload.status,&payload.progress,payload.result.as_ref(),payload.error.as_ref(),&payload.events,payload.archive.as_ref()).await;
        match result {
            Ok(())=>{available.store(true,Ordering::SeqCst);return;},
            Err(error)=>{
                if error.to_string().contains("execution epoch lost"){return;}
                if settle_archive_failure(&mut payload,&error){continue;}
                available.store(false,Ordering::SeqCst);
                if attempt.is_multiple_of(60){eprintln!("manual finalization retry: id={} attempt={} {error}",payload.id,attempt+1);}
            }
        }
        attempt=attempt.saturating_add(1);tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
