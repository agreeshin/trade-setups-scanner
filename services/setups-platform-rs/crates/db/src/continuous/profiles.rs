//! Independent live control. Research contributes only committed candidate handoffs.
//! All collections are request/tick-local and bounded; scientific snapshots stay in DB.
use super::{live, owner_lock};
use crate::DbError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct ProfileLimits { pub total_profiles:i32, pub running_profiles:i32 }
impl ProfileLimits {
    pub fn validate(self)->Result<Self,DbError> {
        if !(1..=10000).contains(&self.total_profiles) || !(1..=self.total_profiles).contains(&self.running_profiles) {
            return Err(DbError::Json("invalid adaptive profile limits".into()));
        }
        Ok(self)
    }
}
async fn limits_in(tx:&mut Transaction<'_,Postgres>,owner:&str)->Result<ProfileLimits,DbError> {
    let limits:Value=sqlx::query_scalar("select coalesce((select jsonb_build_object('totalProfiles',total_profiles,'runningProfiles',running_profiles) from adaptive_profile_limits where owner_id=$1),(select value from settings where key='adaptiveProfileLimits'))")
        .bind(owner).fetch_one(&mut **tx).await?;
    serde_json::from_value::<ProfileLimits>(limits).map_err(crate::json_error)?.validate()
}
pub async fn limits(pool:&PgPool,owner:&str)->Result<ProfileLimits,DbError> {
    let mut tx=pool.begin().await?; let value=limits_in(&mut tx,owner).await?;tx.commit().await?;Ok(value)
}

/// Deployment/admin configuration. Deliberately not exposed in profile commands.
pub async fn configure_default_limits(pool:&PgPool,limits:ProfileLimits)->Result<(),DbError> {
    limits.validate()?;
    crate::settings_set(pool,"adaptiveProfileLimits",&json!(limits)).await
}

pub async fn permission(pool:&PgPool,owner:&str,algorithm:&str)->Result<bool,DbError> {
    Ok(sqlx::query_scalar::<_,bool>("select enabled from adaptive_live_permissions where owner_id=$1 and algorithm_id=$2")
        .bind(owner).bind(algorithm).fetch_optional(pool).await?.unwrap_or(false))
}
pub async fn set_permission(pool:&PgPool,owner:&str,algorithm:&str,enabled:bool)->Result<(),DbError> {
    let mut tx=pool.begin().await?;owner_lock(&mut tx,owner).await?;
    sqlx::query("insert into adaptive_live_permissions(owner_id,algorithm_id,enabled) values($1,$2,$3) on conflict(owner_id,algorithm_id) do update set enabled=excluded.enabled")
        .bind(owner).bind(algorithm).bind(enabled).execute(&mut *tx).await?;
    // Transitional read projection only; research orchestration does not own this flag.
    sqlx::query("update continuous_processes set config=jsonb_set(config,'{adaptiveEnabled}',$3) where owner_id=$1 and algorithm_id=$2")
        .bind(owner).bind(algorithm).bind(json!(enabled)).execute(&mut *tx).await?;
    tx.commit().await?;Ok(())
}

/// Live owner calls this AFTER joining the previous tick, discarding fenced
/// warmup and reconciling RAM. A GET never acknowledges a drain.
pub async fn acknowledge_stops(pool:&PgPool,owner:&str)->Result<(),DbError> {
    let mut tx=pool.begin().await?;
    let ids:Vec<i64>=sqlx::query_scalar("update adaptive_profiles p set stop_pending=false,updated_at=now() where p.id in(select id from adaptive_profiles where owner_id=$1 and not desired_running and stop_pending order by id limit 100 for update skip locked) returning id")
        .bind(owner).fetch_all(&mut *tx).await?;
    if !ids.is_empty() {
        sqlx::query("update continuous_slots set status='stopped',updated_at=now() where id=any($1)").bind(&ids).execute(&mut *tx).await?;
        sqlx::query("update adaptive_live_warmup_lane set slot_id=null,generation_id=null,lease_owner=null,lease_until=null,lease_token=lease_token+1 where slot_id=any($1)")
            .bind(&ids).execute(&mut *tx).await?;
    }
    tx.commit().await?;Ok(())
}

#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct ProfileCommand {
    pub expected_revision:i64,
    pub expected_generation_id:i64,
    pub request_id:String,
}
impl ProfileCommand {
    pub fn validate(&self)->Result<(),DbError> {
        if self.expected_revision<=0 || self.expected_generation_id<=0 || !(16..=128).contains(&self.request_id.len())
            || !self.request_id.bytes().all(|b|b.is_ascii_alphanumeric() || matches!(b,b'-'|b'_')) {
            return Err(DbError::Json("invalid profile command identity".into()));
        }
        Ok(())
    }
}
pub enum CommandOutcome {Applied(Value),NotFound,Conflict(&'static str),Limited{resource:&'static str,limit:i32}}

/// At most 32 durable events per tick. Blocked outcomes are visible and do not
/// hold the research queue or accumulate retry payloads in memory.
pub async fn accept_handoffs(pool:&PgPool,owner:&str)->Result<usize,DbError> {
    let due:bool=sqlx::query_scalar("select exists(select 1 from adaptive_profile_handoffs where owner_id=$1 and status='pending')").bind(owner).fetch_one(pool).await?;
    if !due {return Ok(0);}
    let mut tx=pool.begin().await?;owner_lock(&mut tx,owner).await?;
    let limits=limits_in(&mut tx,owner).await?;
    let rows=sqlx::query("select h.generation_id,h.slot_id,g.algorithm_id,s.min_setups_per_day::text frequency,g.study_id from adaptive_profile_handoffs h join continuous_generations g on g.id=h.generation_id join continuous_slots s on s.id=h.slot_id where h.owner_id=$1 and h.status='pending' and g.status='completed' and g.accepted order by h.generation_id limit 32 for update of h")
        .bind(owner).fetch_all(&mut *tx).await?;
    let count=rows.len();
    for row in rows {
        let id:i64=row.try_get("slot_id")?;let generation:i64=row.try_get("generation_id")?;
        let current=sqlx::query("select accepted_generation_id,desired_running from adaptive_profiles where id=$1 for update")
            .bind(id).fetch_optional(&mut *tx).await?;
        if current.as_ref().is_some_and(|r|r.get::<i64,_>("accepted_generation_id")>=generation) {
            sqlx::query("update adaptive_profile_handoffs set status='superseded' where generation_id=$1").bind(generation).execute(&mut *tx).await?;continue;
        }
        let mut running=current.as_ref().is_none_or(|r|r.get::<bool,_>("desired_running"));
        let mut error=None;
        if current.is_none() {
            let total:i64=sqlx::query_scalar("select count(*) from adaptive_profiles where owner_id=$1").bind(owner).fetch_one(&mut *tx).await?;
            if total>=i64::from(limits.total_profiles) {
                sqlx::query("update adaptive_profile_handoffs set status='blocked',error=$2 where generation_id=$1")
                    .bind(generation).bind(json!({"code":"profile_total_limit","limit":limits.total_profiles})).execute(&mut *tx).await?;continue;
            }
            let active:i64=sqlx::query_scalar("select count(*) from adaptive_profiles where owner_id=$1 and desired_running").bind(owner).fetch_one(&mut *tx).await?;
            if active>=i64::from(limits.running_profiles) {running=false;error=Some(json!({"code":"profile_running_limit","limit":limits.running_profiles}));}
            sqlx::query("insert into adaptive_profiles(id,owner_id,algorithm_id,min_setups_per_day,desired_running,accepted_generation_id,error) values($1,$2,$3,$4::text::numeric,$5,$6,$7)")
                .bind(id).bind(owner).bind(row.try_get::<String,_>("algorithm_id")?).bind(row.try_get::<String,_>("frequency")?)
                .bind(running).bind(generation).bind(&error).execute(&mut *tx).await?;
        } else {
            live::supersede_pending(&mut tx,id).await?;
            sqlx::query("update adaptive_profiles set accepted_generation_id=$2,error=null,updated_at=now() where id=$1")
                .bind(id).bind(generation).execute(&mut *tx).await?;
        }
        // Do not change the control revision on replacement: pre-watermark
        // deliveries remain valid. Generation pointer fences superseded work.
        sqlx::query("update continuous_slots set pending_generation_id=case when $3 then $2 else null end,pending_status=case when $3 then 'warming' else null end,pending_error=$4,readiness_frontier=null,status=case when exists(select 1 from adaptive_profiles p where p.id=$1 and p.stop_pending) then 'stopping' when not $3 then 'stopped' when active_generation_id is not null then 'active' else 'warming' end,study_id=$5,updated_at=now() where id=$1")
            .bind(id).bind(generation).bind(running).bind(&error).bind(row.try_get::<Option<i64>,_>("study_id")?).execute(&mut *tx).await?;
        sqlx::query("update adaptive_profile_handoffs set status='accepted',error=$2 where generation_id=$1")
            .bind(generation).bind(&error).execute(&mut *tx).await?;
    }
    tx.commit().await?;Ok(count)
}

/// A coherent lightweight profile projection; retained even if research stops
/// or the pairing is removed. Heavy immutable spec/result is detail-only.
pub async fn list(pool:&PgPool,owner:&str,algorithm:Option<&str>,offset:i64,limit:i64)->Result<Value,DbError> {
    if offset<0 || !(1..=100).contains(&limit) {return Err(DbError::Json("invalid profile pagination".into()));}
    let mut tx=pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *tx).await?;
    let mut rows:Vec<Value>=sqlx::query_scalar(r#"select jsonb_build_object(
      'id',p.id,'algorithmId',p.algorithm_id,'minSetupsPerDay',p.min_setups_per_day,
      'desiredRunning',p.desired_running,'controlRevision',p.control_revision,
      'acceptedGenerationId',p.accepted_generation_id,'activeGenerationId',s.active_generation_id,
      'activeStatus',case when p.desired_running and a.enabled and s.active_generation_id is not null and exists(select 1 from settings where key='live' and value->'enabled'='true'::jsonb) then 'active' else 'inactive' end,
      'status',case when p.stop_pending then 'stopping' when not p.desired_running then 'stopped' else s.status end,
      'pendingGenerationId',s.pending_generation_id,'pendingStatus',s.pending_status,'pendingError',coalesce(s.pending_error,p.error),
      'error',p.error,'adaptiveEnabled',a.enabled,'liveWatermark',s.live_watermark,
      'liveInitialization',g.live_initialization,'readiness',coalesce(g.live_readiness,'[]'::jsonb),
      '_spec',jsonb_build_object('instruments',g.spec->'instruments'),'readinessFrontier',s.readiness_frontier,
      'direction',coalesce(g.spec->'direction',c.spec->'direction'),
      'blockingReason',case when not a.enabled then 'adaptive_permission_disabled' when not exists(select 1 from settings where key='live' and value->'enabled'='true'::jsonb) then 'global_live_disabled' else null end,
      'studyId',c.study_id,'activeStudyId',g.study_id,'pendingStudyId',n.study_id,
      'meanExpectancy',case when g.id is not null then coalesce((g.result->>'meanExpectancy')::double precision,s.mean_expectancy) else (c.result->>'meanExpectancy')::double precision end,
      'selectedCell',case when g.id is not null then g.result->'selectedCell' else c.result->'selectedCell' end,
      'trendMode',case when g.id is not null then coalesce(g.spec->'trendMode','"mean"'::jsonb) else coalesce(c.spec->'trendMode','"mean"'::jsonb) end,
      'trendBlendLambda',case when (case when g.id is not null then g.spec else c.spec end)->>'trendMode'='blend' then coalesce((case when g.id is not null then g.spec else c.spec end)->'trendBlendLambda','0.7'::jsonb) else null end,
      'pendingAttempts',coalesce(n.live_init_attempts,c.live_init_attempts),
      'attemptLimit',coalesce(n.live_init_attempt_limit,c.live_init_attempt_limit),
      'initializationStatus',coalesce(n.live_init_status,c.live_init_status),
      'inResearchQueue',s.enabled,'updatedAt',p.updated_at)
      from adaptive_profiles p join adaptive_live_permissions a using(owner_id,algorithm_id)
      join continuous_slots s on s.id=p.id join continuous_generations c on c.id=p.accepted_generation_id
      left join continuous_generations g on g.id=s.active_generation_id
      left join continuous_generations n on n.id=s.pending_generation_id
      where p.owner_id=$1 and ($2::text is null or p.algorithm_id=$2) order by p.id offset $3 limit $4"#)
        .bind(owner).bind(algorithm).bind(offset).bind(limit).fetch_all(&mut *tx).await?;
    for value in &mut rows {
        let spec=value.as_object_mut().and_then(|v|v.remove("_spec")).ok_or_else(||DbError::Json("missing profile readiness scope".into()))?;
        let readiness=value.as_object_mut().and_then(|v|v.remove("readiness")).ok_or_else(||DbError::Json("missing profile readiness".into()))?;
        value["liveReadinessSummary"]=match value["activeGenerationId"].as_i64() {
            Some(id)=>json!(live::readiness_summary(id,&spec,&serde_json::from_value::<Vec<live::LiveInstrumentReadiness>>(readiness).map_err(crate::json_error)?)?),
            None=>Value::Null,
        };
    }
    let total:i64=sqlx::query_scalar("select count(*) from adaptive_profiles where owner_id=$1 and ($2::text is null or algorithm_id=$2)")
        .bind(owner).bind(algorithm).fetch_one(&mut *tx).await?;
    let blocked:Vec<Value>=sqlx::query_scalar("select jsonb_build_object('generationId',h.generation_id,'slotId',h.slot_id,'error',h.error) from adaptive_profile_handoffs h join continuous_generations g on g.id=h.generation_id where h.owner_id=$1 and h.status='blocked' and ($2::text is null or g.algorithm_id=$2) order by h.generation_id desc limit 50")
        .bind(owner).bind(algorithm).fetch_all(&mut *tx).await?;
    let limits=limits_in(&mut tx,owner).await?;tx.commit().await?;
    Ok(json!({"profiles":rows,"total":total,"offset":offset,"limit":limit,"limits":limits,"blockedHandoffs":blocked}))
}
pub async fn detail(pool:&PgPool,owner:&str,id:i64)->Result<Option<Value>,DbError> {
    let mut result:Option<Value>=sqlx::query_scalar("select jsonb_build_object('id',p.id,'ownerId',p.owner_id,'algorithmId',p.algorithm_id,'minSetupsPerDay',p.min_setups_per_day,'desiredRunning',p.desired_running,'controlRevision',p.control_revision,'acceptedGenerationId',p.accepted_generation_id,'activeGenerationId',s.active_generation_id,'pendingGenerationId',s.pending_generation_id,'status',case when p.stop_pending then 'stopping' when p.desired_running then s.status else 'stopped' end,'error',p.error,'liveWatermark',s.live_watermark,'spec',g.spec,'result',g.result,'readiness',g.live_readiness,'initialization',g.live_initialization,'attempts',g.live_init_attempts,'attemptLimit',g.live_init_attempt_limit,'initializationStatus',g.live_init_status,'updatedAt',p.updated_at) from adaptive_profiles p join continuous_slots s on s.id=p.id join continuous_generations g on g.id=p.accepted_generation_id where p.owner_id=$1 and p.id=$2")
        .bind(owner).bind(id).fetch_optional(pool).await?;
    if let Some(value)=result.as_mut() {value["limits"]=json!(limits(pool,owner).await?);}
    Ok(result)
}

pub async fn command(pool:&PgPool,owner:&str,id:i64,play:bool,request:&ProfileCommand)->Result<CommandOutcome,DbError> {
    request.validate()?;let action=if play {"play"}else{"stop"};
    let mut tx=pool.begin().await?;
    sqlx::query("select set_config('statement_timeout','2000',true),set_config('lock_timeout','1000',true)").execute(&mut *tx).await?;
    owner_lock(&mut tx,owner).await?;
    if let Some(prior)=sqlx::query("select * from adaptive_profile_commands where owner_id=$1 and request_id=$2").bind(owner).bind(&request.request_id).fetch_optional(&mut *tx).await? {
        if prior.try_get::<i64,_>("profile_id")?!=id || prior.try_get::<String,_>("action")?!=action
            || prior.try_get::<i64,_>("expected_revision")?!=request.expected_revision || prior.try_get::<i64,_>("expected_generation_id")?!=request.expected_generation_id {
            return Ok(CommandOutcome::Conflict("profile_command_key_conflict"));
        }
        return Ok(CommandOutcome::Applied(prior.try_get("receipt")?));
    }
    let row=sqlx::query("select p.*,g.live_init_attempts,g.live_init_attempt_limit,g.live_init_status,jsonb_build_object('attempts',g.live_init_attempts,'attemptLimit',g.live_init_attempt_limit,'status',g.live_init_status,'error',s.pending_error,'deadline',g.live_init_deadline,'nextAttemptAt',g.live_init_next_attempt_at,'initialization',g.live_initialization) before_state from adaptive_profiles p join continuous_generations g on g.id=p.accepted_generation_id join continuous_slots s on s.id=p.id where p.id=$1 and p.owner_id=$2 for update of p")
        .bind(id).bind(owner).fetch_optional(&mut *tx).await?;
    let Some(row)=row else {return Ok(CommandOutcome::NotFound);};
    if row.try_get::<i64,_>("control_revision")?!=request.expected_revision || row.try_get::<i64,_>("accepted_generation_id")?!=request.expected_generation_id {return Ok(CommandOutcome::Conflict("profile_revision_changed"));}
    if play && row.try_get::<bool,_>("stop_pending")? {return Ok(CommandOutcome::Conflict("profile_stopping"));}
    let was_running:bool=row.try_get("desired_running")?;
    let attempts:i32=row.try_get("live_init_attempts")?;let mut attempt_limit:i32=row.try_get("live_init_attempt_limit")?;
    let exhausted=row.try_get::<Option<String>,_>("live_init_status")?.as_deref()==Some("exhausted") || (!was_running && attempts>=attempt_limit);
    if play && !was_running {
        let cap=limits_in(&mut tx,owner).await?;
        let count:i64=sqlx::query_scalar("select count(*) from adaptive_profiles where owner_id=$1 and desired_running").bind(owner).fetch_one(&mut *tx).await?;
        if count>=i64::from(cap.running_profiles) {return Ok(CommandOutcome::Limited{resource:"profile_running_limit",limit:cap.running_profiles});}
    }
    let change=was_running!=play || (play && exhausted);
    let revision=if change {request.expected_revision.checked_add(1).ok_or_else(||DbError::Json("profile revision overflow".into()))?}else{request.expected_revision};
    let grant=play && exhausted;
    if grant {attempt_limit=attempt_limit.checked_add(live::MAX_INITIALIZATION_ATTEMPTS).ok_or_else(||DbError::Json("profile attempt limit overflow".into()))?;}
    if change {
        sqlx::query("select id from continuous_slots where id=$1 for update").bind(id).execute(&mut *tx).await?;
        // Keep an occupied lane until expiry: fencing must not permit overlap
        // with a worker that has not yet observed Stop/supersession.
        sqlx::query("update adaptive_live_warmup_lane set lease_token=lease_token+1 where slot_id=$1").bind(id).execute(&mut *tx).await?;
        sqlx::query("update adaptive_profiles set desired_running=$3,stop_pending=not $3,control_revision=$4,error=null,updated_at=now() where id=$1 and owner_id=$2")
            .bind(id).bind(owner).bind(play).bind(revision).execute(&mut *tx).await?;
        if play {
          if !was_running {
            sqlx::query("delete from adaptive_profile_stopped_checkpoints where profile_id=$1").bind(id).execute(&mut *tx).await?;
            sqlx::query("insert into adaptive_profile_stopped_checkpoints(profile_id,instrument_key,generation_id,state) select slot_id,instrument_key,generation_id,state from adaptive_live_checkpoints where slot_id=$1")
                .bind(id).execute(&mut *tx).await?;
            sqlx::query("delete from adaptive_live_checkpoints where slot_id=$1").bind(id).execute(&mut *tx).await?;
          }
            sqlx::query("update continuous_generations set live_initialization=case when $3 then null else live_initialization end,live_init_status='retry',live_init_attempt_limit=$2,live_init_deadline=null,live_init_next_attempt_at=clock_timestamp() where id=$1")
                .bind(request.expected_generation_id).bind(attempt_limit).bind(!was_running).execute(&mut *tx).await?;
            sqlx::query("update continuous_slots set active_generation_id=case when $3 then null else active_generation_id end,pending_generation_id=$2,pending_status='warming',pending_error=null,readiness_frontier=null,status=case when not $3 and active_generation_id is not null then 'active' else 'warming' end,updated_at=now() where id=$1")
                .bind(id).bind(request.expected_generation_id).bind(!was_running).execute(&mut *tx).await?;
        } else {
            sqlx::query("update continuous_slots set pending_generation_id=null,pending_status=null,pending_error=null,readiness_frontier=null,status='stopping',updated_at=now() where id=$1")
                .bind(id).execute(&mut *tx).await?;
        }
    }
    let receipt=json!({"profileId":id,"action":action,"desiredRunning":play,"controlRevision":revision,
        "acceptedGenerationId":request.expected_generation_id,"requestId":request.request_id,"attemptsBefore":attempts,
        "limitBefore":row.try_get::<i32,_>("live_init_attempt_limit")?,"attemptLimit":attempt_limit,"grantedAttempts":if grant {3}else{0},
        "before":row.try_get::<Value,_>("before_state")?,"status":if play {"warming"}else if change || row.try_get::<bool,_>("stop_pending")? {"stopping"}else{"stopped"}});
    sqlx::query("insert into adaptive_profile_commands(owner_id,request_id,profile_id,action,expected_revision,expected_generation_id,receipt) values($1,$2,$3,$4,$5,$6,$7)")
        .bind(owner).bind(&request.request_id).bind(id).bind(action).bind(request.expected_revision).bind(request.expected_generation_id).bind(&receipt).execute(&mut *tx).await?;
    tx.commit().await?;Ok(CommandOutcome::Applied(receipt))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_limits_are_external_validated_configuration_not_a_browser_field() {
        assert!(ProfileLimits{total_profiles:50,running_profiles:30}.validate().is_ok());
        for limits in [ProfileLimits{total_profiles:0,running_profiles:1},ProfileLimits{total_profiles:10,running_profiles:11},ProfileLimits{total_profiles:10001,running_profiles:30}] {assert!(limits.validate().is_err());}
    }
    #[test]
    fn profile_command_requires_fence_and_bounded_durable_idempotency_key() {
        let command=ProfileCommand{expected_revision:7,expected_generation_id:22,request_id:"profile-play-000001".into()};
        assert!(command.validate().is_ok());
        for key in ["short".to_owned(),"x".repeat(129),"invalid key 000000".into()] {assert!(ProfileCommand{request_id:key,..command.clone()}.validate().is_err());}
        assert!(serde_json::from_value::<ProfileCommand>(json!({"expectedRevision":7,"expectedGenerationId":22,"requestId":command.request_id,"totalProfiles":999})).is_err());
    }
}
