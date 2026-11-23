//! Focused HTTP and persistence-fault checks. Main owns cross-mode FIFO acceptance.
use super::*;

fn long_spec()->Value {
    let mut value=spec();value["maxTrials"]=json!(1024);value["maxSeconds"]=json!(60);
    value["algorithmSearch"]=json!({"windowMinutes":(1..=32).collect::<Vec<_>>(),"priceGrowthPct":(1..=32).map(|n|n.to_string()).collect::<Vec<_>>()});value
}
async fn create(router:&Router,value:Value)->i64 {
    let(status,body)=request(router,"POST","/v1/optimization-studies",value).await;
    assert_eq!(status,StatusCode::ACCEPTED,"{body}");body["id"].as_i64().unwrap()
}
async fn pause_with_winner(router:&Router,pool:&PgPool,id:i64)->db::StudyRecord {
    let running=wait_for(pool,id,|r|r.status=="running" && r.progress["evaluatedCandidates"].as_u64().unwrap_or(0)>0).await;
    assert!(running.progress["bestScore"].is_number(),"positive fixture incumbent must exist before pause");
    let revision=running.manual_control.unwrap().revision;
    assert_eq!(request(router,"POST",&format!("/v1/optimization-studies/{id}/pause"),json!({"expectedControlRevision":revision})).await.0,StatusCode::ACCEPTED);
    wait_for(pool,id,|r|r.status=="paused").await
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn manual_http_idempotency_control_guards_and_queue_limits() {
    let database=TestDb::new().await;let mut owner=database.pool.begin().await.unwrap();
    sqlx::query("select pg_advisory_xact_lock($1)").bind(db::RUNTIME_LOCK).execute(&mut *owner).await.unwrap();
    let mut config=OptimizationConfig::default();config.manual.max_pending=2;let router=app(database.pool.clone(),config);
    async fn keyed(router:&Router,key:&str,value:Value)->(StatusCode,Value){
        let response=router.clone().oneshot(Request::builder().method("POST").uri("/v1/optimization-studies").header("x-api-key","test-key").header("content-type","application/json").header("idempotency-key",key).body(Body::from(value.to_string())).unwrap()).await.unwrap();
        let status=response.status();let body=serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();(status,body)
    }
    let(first,a)=keyed(&router,"intentional-1",spec()).await;assert_eq!(first,StatusCode::ACCEPTED);
    let(second,b)=keyed(&router,"intentional-1",spec()).await;assert_eq!(second,StatusCode::ACCEPTED);assert_eq!(a["id"],b["id"]);
    let mut different=spec();different["seed"]=json!(43);assert_eq!(keyed(&router,"intentional-1",different).await.0,StatusCode::CONFLICT);
    assert_eq!(keyed(&router,"bad key",spec()).await.0,StatusCode::BAD_REQUEST);
    let id=a["id"].as_i64().unwrap();assert_eq!(a["manualControl"]["supportsPause"],true);
    assert_eq!(request(&router,"POST",&format!("/v1/optimization-studies/{id}/pause"),json!({"expectedControlRevision":1})).await.0,StatusCode::CONFLICT);
    create(&router,spec()).await;
    let(status,body)=keyed(&router,"third",spec()).await;assert_eq!(status,StatusCode::TOO_MANY_REQUESTS);assert_eq!(body["resource"],"manual_pending");
    let(_,list)=request(&router,"GET","/v1/optimization-studies",Value::Null).await;assert!(list["studies"][0]["manualControl"].is_object());
    assert_eq!(request(&router,"DELETE",&format!("/v1/optimization-studies/{id}"),Value::Null).await.0,StatusCode::OK);
    let(_,again)=keyed(&router,"intentional-1",spec()).await;assert_ne!(again["id"],a["id"],"explicit delete forgets the receipt");
    owner.rollback().await.unwrap();database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn manual_terminal_outage_retains_archive_and_dispatches_next_without_restart() {
    let database=TestDb::new().await;let(url,_,server)=market(Duration::from_millis(5),0).await;
    sqlx::raw_sql("create table manual_finish_gate(blocked boolean not null);insert into manual_finish_gate values(true);create function manual_finish_fault() returns trigger language plpgsql as $$ begin if NEW.status in ('completed','cancelled','failed') and (select blocked from manual_finish_gate) then raise exception 'temporary test outage' using errcode='08006';end if;return NEW;end $$;create trigger manual_finish_fault before update on optimization_studies for each row execute function manual_finish_fault()")
        .execute(&database.pool).await.unwrap();
    let runtime=start_optimization_runtime(database.pool.clone(),url,"key".into(),Default::default()).await.unwrap();
    let router=app(database.pool.clone(),Default::default());let a=create(&router,spec()).await;
    // Register the next study before finalization becomes unavailable.
    let b=match db::create(&database.pool,"price_volume_spike",&spec(),&json!({})).await.unwrap(){Admission::Created(row)=>row.id,_=>panic!("queued admission")};
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(db::get(&database.pool,a).await.unwrap().unwrap().status,"running");
    assert_eq!(db::get(&database.pool,b).await.unwrap().unwrap().status,"queued");
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from manual_runtime_artifacts").fetch_one(&database.pool).await.unwrap(),1,"valid checkpoint must survive terminal outage");
    sqlx::query("update manual_finish_gate set blocked=false").execute(&database.pool).await.unwrap();
    let first=wait_for(&database.pool,a,|r|!r.active()).await;assert_eq!(first.status,"completed","{:?}",first.error);assert!(first.result.unwrap()["best"].is_object());
    let next=wait_for(&database.pool,b,|r|!r.active()).await;assert_eq!(next.status,"completed","{:?}",next.error);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from manual_runtime_artifacts").fetch_one(&database.pool).await.unwrap(),0);
    assert!(runtime.available.load(Ordering::SeqCst),"dispatcher must recover without restart");
    runtime.stop().await;server.abort();database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn manual_paused_stop_retries_artifact_outage_while_other_research_is_reserved() {
    let database=TestDb::new().await;
    let fault_pool=PgPoolOptions::new().max_connections(6).acquire_timeout(Duration::from_millis(70)).connect_with((*database.pool.connect_options()).clone()).await.unwrap();
    let(url,_,server)=market(Duration::from_millis(80),0).await;
    let config=OptimizationConfig{cpu_threads:1,batch_size:1,..Default::default()};
    let runtime=start_optimization_runtime(fault_pool.clone(),url,"key".into(),config.clone()).await.unwrap();let router=app(database.pool.clone(),config);
    let a=create(&router,long_spec()).await;let paused=pause_with_winner(&router,&database.pool,a).await;
    let before:(Vec<u8>,String)=sqlx::query_as("select checkpoint,checkpoint_hash from manual_runtime_artifacts where study_id=$1").bind(a).fetch_one(&database.pool).await.unwrap();
    sqlx::raw_sql(&format!("create table paused_terminal_gate(blocked boolean not null);insert into paused_terminal_gate values(true);create function paused_terminal_fault() returns trigger language plpgsql as $$ begin if NEW.id={a} and NEW.status='cancelled' and (select blocked from paused_terminal_gate) then raise exception 'paused terminal outage' using errcode='08006';end if;return NEW;end $$;create trigger paused_terminal_fault before update on optimization_studies for each row execute function paused_terminal_fault()"))
        .execute(&database.pool).await.unwrap();
    let b=create(&router,spec()).await;wait_for(&database.pool,b,|r|r.status=="running").await;
    let mut held=Vec::new();for _ in 0..6 {match fault_pool.acquire().await{Ok(connection)=>held.push(connection),Err(sqlx::Error::PoolTimedOut)=>break,Err(error)=>panic!("{error}")}}
    assert!(!held.is_empty());
    assert_eq!(request(&router,"POST",&format!("/v1/optimization-studies/{a}/cancel"),Value::Null).await.0,StatusCode::ACCEPTED);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(db::get(&database.pool,a).await.unwrap().unwrap().status,"cancel_requested");
    let during:(Vec<u8>,String)=sqlx::query_as("select checkpoint,checkpoint_hash from manual_runtime_artifacts where study_id=$1").bind(a).fetch_one(&database.pool).await.unwrap();assert_eq!(before,during,"unconfirmed read cannot erase incumbent/ledger");
    assert_eq!(db::get(&database.pool,b).await.unwrap().unwrap().status,"running");drop(held);
    assert_eq!(wait_for(&database.pool,b,|r|r.status=="completed").await.status,"completed","settlement retry must not block the other executor future");
    assert_eq!(db::get(&database.pool,a).await.unwrap().unwrap().status,"cancel_requested");
    sqlx::query("update paused_terminal_gate set blocked=false").execute(&database.pool).await.unwrap();
    let stopped=wait_for(&database.pool,a,|r|r.status=="cancelled").await;
    assert_eq!(stopped.result.as_ref().unwrap()["best"]["score"],paused.progress["bestScore"]);
    assert!(stopped.result.as_ref().unwrap()["best"]["partial"].as_bool().unwrap());
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from manual_runtime_artifacts where study_id=$1").bind(a).fetch_one(&database.pool).await.unwrap(),0);
    runtime.stop().await;fault_pool.close().await;server.abort();database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn manual_pause_ack_outage_retries_and_paused_survives_restart_without_autoplay() {
    let database=TestDb::new().await;let(url,requests,server)=market(Duration::from_millis(5),0).await;
    let config=OptimizationConfig{cpu_threads:1,batch_size:1,..Default::default()};
    sqlx::raw_sql("create table manual_pause_gate(blocked boolean not null);insert into manual_pause_gate values(true);create function manual_pause_fault() returns trigger language plpgsql as $$ begin if NEW.status='paused' and (select blocked from manual_pause_gate) then raise exception 'temporary pause ACK outage' using errcode='08006';end if;return NEW;end $$;create trigger manual_pause_fault before update on optimization_studies for each row execute function manual_pause_fault()")
        .execute(&database.pool).await.unwrap();
    let runtime=start_optimization_runtime(database.pool.clone(),url.clone(),"key".into(),config.clone()).await.unwrap();let router=app(database.pool.clone(),config.clone());let id=create(&router,long_spec()).await;
    let running=wait_for(&database.pool,id,|r|r.status=="running" && r.progress["evaluatedCandidates"].as_u64().unwrap_or(0)>0).await;
    request(&router,"POST",&format!("/v1/optimization-studies/{id}/pause"),json!({"expectedControlRevision":running.manual_control.unwrap().revision})).await;
    tokio::time::sleep(Duration::from_secs(2)).await;assert_eq!(db::get(&database.pool,id).await.unwrap().unwrap().status,"pausing");
    assert_eq!(sqlx::query_scalar::<_,bool>("select manual_execution_reserved from optimization_studies where id=$1").bind(id).fetch_one(&database.pool).await.unwrap(),true);
    sqlx::query("update manual_pause_gate set blocked=false").execute(&database.pool).await.unwrap();wait_for(&database.pool,id,|r|r.status=="paused").await;
    runtime.stop().await;let calls=requests.load(Ordering::SeqCst);
    let runtime=start_optimization_runtime(database.pool.clone(),url,"key".into(),config).await.unwrap();tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(db::get(&database.pool,id).await.unwrap().unwrap().status,"paused");assert_eq!(requests.load(Ordering::SeqCst),calls);
    request(&router,"POST",&format!("/v1/optimization-studies/{id}/cancel"),Value::Null).await;wait_for(&database.pool,id,|r|r.status=="cancelled").await;
    runtime.stop().await;server.abort();database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn manual_checkpoint_write_outage_keeps_acknowledged_incumbent_until_pause_can_commit() {
    let database=TestDb::new().await;let(url,_,server)=market(Duration::from_millis(5),0).await;
    let config=OptimizationConfig{cpu_threads:1,batch_size:1,..Default::default()};
    sqlx::raw_sql("create table manual_checkpoint_gate(blocked boolean not null);insert into manual_checkpoint_gate values(false);create function manual_checkpoint_fault() returns trigger language plpgsql as $$ begin if NEW.checkpoint is not null and (select blocked from manual_checkpoint_gate) then raise exception 'temporary checkpoint write outage' using errcode='08006';end if;return NEW;end $$;create trigger manual_checkpoint_fault before update on manual_runtime_artifacts for each row execute function manual_checkpoint_fault()")
        .execute(&database.pool).await.unwrap();
    let runtime=start_optimization_runtime(database.pool.clone(),url,"key".into(),config.clone()).await.unwrap();let router=app(database.pool.clone(),config);let a=create(&router,long_spec()).await;
    let running=wait_for(&database.pool,a,|r|r.status=="running" && r.progress["evaluatedCandidates"].as_u64().unwrap_or(0)>0).await;
    sqlx::query("update manual_checkpoint_gate set blocked=true").execute(&database.pool).await.unwrap();
    let before:(Vec<u8>,String)=sqlx::query_as("select checkpoint,checkpoint_hash from manual_runtime_artifacts where study_id=$1").bind(a).fetch_one(&database.pool).await.unwrap();
    request(&router,"POST",&format!("/v1/optimization-studies/{a}/pause"),json!({"expectedControlRevision":running.manual_control.unwrap().revision})).await;
    let b=create(&router,spec()).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(db::get(&database.pool,a).await.unwrap().unwrap().status,"pausing");assert_eq!(db::get(&database.pool,b).await.unwrap().unwrap().status,"queued");
    let during:(Vec<u8>,String)=sqlx::query_as("select checkpoint,checkpoint_hash from manual_runtime_artifacts where study_id=$1").bind(a).fetch_one(&database.pool).await.unwrap();assert_eq!(before,during);
    sqlx::query("update manual_checkpoint_gate set blocked=false").execute(&database.pool).await.unwrap();
    let paused=wait_for(&database.pool,a,|r|r.status=="paused").await;assert!(paused.progress["bestScore"].is_number());
    assert_eq!(wait_for(&database.pool,b,|r|!r.active()).await.status,"completed");
    request(&router,"POST",&format!("/v1/optimization-studies/{a}/cancel"),Value::Null).await;
    let stopped=wait_for(&database.pool,a,|r|r.status=="cancelled").await;assert_eq!(stopped.result.unwrap()["best"]["score"],paused.progress["bestScore"]);
    runtime.stop().await;server.abort();database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn manual_stop_winning_pause_ack_retries_reserved_checkpoint_read_and_releases_next() {
    let database=TestDb::new().await;let(url,_,server)=market(Duration::from_millis(5),0).await;
    let config=OptimizationConfig{cpu_threads:1,batch_size:1,..Default::default()};
    sqlx::raw_sql("create function reserved_pause_fault() returns trigger language plpgsql as $$ begin if NEW.status='paused' then raise exception 'hold joined executor before pause ACK' using errcode='08006';end if;return NEW;end $$;create trigger reserved_pause_fault before update on optimization_studies for each row execute function reserved_pause_fault()")
        .execute(&database.pool).await.unwrap();
    let runtime=start_optimization_runtime(database.pool.clone(),url,"key".into(),config.clone()).await.unwrap();let router=app(database.pool.clone(),config);let a=create(&router,long_spec()).await;
    let running=wait_for(&database.pool,a,|r|r.status=="running" && r.progress["evaluatedCandidates"].as_u64().unwrap_or(0)>0).await;
    request(&router,"POST",&format!("/v1/optimization-studies/{a}/pause"),json!({"expectedControlRevision":running.manual_control.unwrap().revision})).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let checkpoint_hash:String=sqlx::query_scalar("select checkpoint_hash from manual_runtime_artifacts where study_id=$1").bind(a).fetch_one(&database.pool).await.unwrap();
    let b=create(&router,spec()).await;
    request(&router,"POST",&format!("/v1/optimization-studies/{a}/cancel"),Value::Null).await;
    let mut lock=database.pool.begin().await.unwrap();sqlx::query("lock table manual_runtime_artifacts in access exclusive mode").execute(&mut *lock).await.unwrap();
    let pid=tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let pid:Option<i32>=sqlx::query_scalar("select pid from pg_stat_activity where datname=current_database() and wait_event_type='Lock' and query like 'select * from manual_runtime_artifacts where study_id=%' limit 1")
                .fetch_optional(&database.pool).await.unwrap();
            if let Some(pid)=pid {break pid;}tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("joined reserved Stop must attempt checkpoint-only settlement");
    sqlx::query("select pg_terminate_backend($1)").bind(pid).execute(&database.pool).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;lock.rollback().await.unwrap();
    let current:Option<String>=sqlx::query_scalar("select checkpoint_hash from manual_runtime_artifacts where study_id=$1").bind(a).fetch_optional(&database.pool).await.unwrap();
    if let Some(current)=current {assert_eq!(current,checkpoint_hash);}
    let stopped=wait_for(&database.pool,a,|r|!r.active()).await;assert_eq!(stopped.status,"cancelled","{:?}",stopped.error);assert!(stopped.result.unwrap()["best"].is_object());
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from study_manual_original_outcomes where study_id=$1").bind(a).fetch_one(&database.pool).await.unwrap(),1);
    assert_eq!(wait_for(&database.pool,b,|r|!r.active()).await.status,"completed");
    assert!(!sqlx::query_scalar::<_,bool>("select manual_execution_reserved from optimization_studies where id=$1").bind(a).fetch_one(&database.pool).await.unwrap());
    runtime.stop().await;server.abort();database.cleanup().await;
}
