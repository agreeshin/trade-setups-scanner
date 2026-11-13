//! Explicit confirmed history deletion, never Stop's unfinished-work cleanup.
//! SQL sets own the historical scope; only bounded counts/fingerprints enter RAM.
use super::owner_lock;
use crate::DbError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub studies: i64,
    pub generations: i64,
    pub profiles: i64,
    pub checkpoints: i64,
    pub notification_jobs: i64,
    pub processes_reset: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Retained {
    pub runs: i64,
    pub batches: i64,
    pub quality_intervals: i64,
    pub analytics_results: i64,
    pub live_setups: i64,
    pub configured_processes: i64,
    pub configured_slots: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Preview {
    pub eligible: bool,
    pub blockers: Vec<String>,
    pub fingerprint: String,
    pub counts: Counts,
    pub retained: Retained,
}
#[derive(Debug, Clone, Serialize)]
pub struct Completed {
    pub deleted: Counts,
    pub retained: Retained,
}
pub enum Outcome {
    Deleted(Completed),
    Blocked(Preview),
    Stale(Preview),
}

// A profile's frequency/enabled identity is configuration, not research history.
const DIRTY_PROFILE: &str = "(study_id is not null or active_generation_id is not null or pending_generation_id is not null or mean_expectancy is not null or selected_cell is not null or pending_status is not null or pending_error is not null or readiness_frontier is not null or live_watermark is not null or status not in ('idle','stopped'))";
const DIRTY_PROCESS: &str = "(active_study_id is not null or active_generation_id is not null or phase is not null or error is not null or retry_count<>0 or next_slot<>0 or cleanup_targets<>'{\"studies\":[],\"runs\":[],\"batches\":[]}'::jsonb)";

async fn snapshot(tx: &mut Transaction<'_, Postgres>, owner: &str) -> Result<Preview, DbError> {
    let state: Value = sqlx::query_scalar(&format!(r#"
      select jsonb_build_object(
        'counts',jsonb_build_object(
          'studies',(select count(*) from optimization_studies where owner_id=$1 and origin='continuous'),
          'generations',(select count(*) from continuous_generations where owner_id=$1),
          'profiles',(select count(*) from continuous_slots where owner_id=$1 and {DIRTY_PROFILE}),
          'checkpoints',(select count(*) from adaptive_live_checkpoints c join continuous_slots s on s.id=c.slot_id where s.owner_id=$1),
          'notificationJobs',(select count(*) from adaptive_notification_outbox where owner_id=$1 and slot_id is not null),
          'processesReset',(select count(*) from continuous_processes where owner_id=$1 and {DIRTY_PROCESS})),
        'retained',jsonb_build_object(
          'runs',(select count(*) from replay_runs where owner_id=$1 and origin='continuous'),
          'batches',(select count(*) from replay_batches where owner_id=$1 and origin='continuous'),
          'qualityIntervals',(select count(*) from replay_quality_intervals q join replay_runs r on r.id=q.run_id where r.owner_id=$1 and r.origin='continuous'),
          'analyticsResults',(select count(*) from replay_batches b left join continuous_generations g on g.id=b.continuous_generation_id where b.owner_id=$1 and b.origin='continuous' and (b.continuous_analytics_archive is not null or (b.id=g.replay_batch_id and g.result ? 'analytics'))),
          'liveSetups',(select count(*) from live_setups l join continuous_slots s on s.id=l.adaptive_slot_id where s.owner_id=$1),
          'configuredProcesses',(select count(*) from continuous_processes where owner_id=$1),
          'configuredSlots',(select count(*) from continuous_slots where owner_id=$1)),
        'busy',(exists(select 1 from continuous_processes where owner_id=$1 and (desired_running or status<>'stopped'))
          or exists(select 1 from continuous_generations where owner_id=$1 and status='running')
          or exists(select 1 from optimization_studies where owner_id=$1 and origin='continuous' and status in ('queued','running','cancel_requested'))
          or exists(select 1 from replay_runs where owner_id=$1 and origin='continuous' and status in ('queued','running','cancel_requested'))
          or exists(select 1 from continuous_slots where owner_id=$1 and (active_generation_id is not null or pending_generation_id is not null))
          or exists(select 1 from adaptive_notification_outbox where owner_id=$1 and slot_id is not null and lease_owner is not null and lease_until>clock_timestamp())),
        'ownershipConflict',(
          exists(select 1 from optimization_studies s join continuous_generations g on g.id=s.continuous_generation_id where (g.owner_id=$1 and (s.owner_id<>$1 or s.origin<>'continuous')) or (s.owner_id=$1 and s.origin='continuous' and g.owner_id<>$1))
          or exists(select 1 from replay_runs r join continuous_generations g on g.id=r.continuous_generation_id where (g.owner_id=$1 and (r.owner_id<>$1 or r.origin<>'continuous')) or (r.owner_id=$1 and r.origin='continuous' and g.owner_id<>$1))
          or exists(select 1 from replay_batches b join continuous_generations g on g.id=b.continuous_generation_id where (g.owner_id=$1 and (b.owner_id<>$1 or b.origin<>'continuous')) or (b.owner_id=$1 and b.origin='continuous' and g.owner_id<>$1))
          or exists(select 1 from live_setups l join continuous_generations g on g.id=l.adaptive_generation_id left join continuous_slots s on s.id=l.adaptive_slot_id where (g.owner_id=$1 and s.owner_id is distinct from $1) or (s.owner_id=$1 and g.owner_id<>$1))
          or exists(select 1 from continuous_generations g join optimization_studies s on s.id=g.study_id where s.owner_id=$1 and s.origin='continuous' and g.owner_id<>$1)
          or exists(select 1 from continuous_slots t join optimization_studies s on s.id=t.study_id where s.owner_id=$1 and s.origin='continuous' and t.owner_id<>$1)
          or exists(select 1 from continuous_processes p join optimization_studies s on s.id=p.active_study_id where s.owner_id=$1 and s.origin='continuous' and p.owner_id<>$1)
          or exists(select 1 from continuous_generations g where g.owner_id=$1 and g.result ? 'analytics' and not exists(select 1 from replay_batches b where b.id=g.replay_batch_id and b.continuous_generation_id=g.id and b.owner_id=$1 and b.origin='continuous'))),
        'revision',coalesce((select md5(string_agg(to_jsonb(p)::text,'' order by algorithm_id)) from continuous_processes p where owner_id=$1),''),
        'slots',coalesce((select md5(string_agg(to_jsonb(s)::text,'' order by id)) from continuous_slots s where owner_id=$1),''),
        'studies',coalesce((select md5(string_agg(concat_ws(':',id,status,continuous_generation_id,updated_at),',' order by id)) from optimization_studies where owner_id=$1 and origin='continuous'),''),
        'generations',coalesce((select md5(string_agg(concat_ws(':',id,status,updated_at),',' order by id)) from continuous_generations where owner_id=$1),''),
        'notifications',coalesce((select md5(string_agg(concat_ws(':',id,lease_token,attempts),',' order by id)) from adaptive_notification_outbox where owner_id=$1 and slot_id is not null),''))
    "#)).bind(owner).fetch_one(&mut **tx).await?;
    let fingerprint: String = sqlx::query_scalar("select md5($1::jsonb::text)")
        .bind(&state).fetch_one(&mut **tx).await?;
    let mut blockers = Vec::new();
    if state["busy"] == true { blockers.push("continuous_not_stopped".into()); }
    if state["ownershipConflict"] == true { blockers.push("ownership_conflict".into()); }
    Ok(Preview { eligible: blockers.is_empty(), blockers, fingerprint,
        counts: serde_json::from_value(state["counts"].clone()).map_err(crate::json_error)?,
        retained: serde_json::from_value(state["retained"].clone()).map_err(crate::json_error)? })
}

pub async fn preview(pool: &PgPool, owner: &str) -> Result<Preview, DbError> {
    let mut tx=pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *tx).await?;
    let result=snapshot(&mut tx,owner).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn purge(pool: &PgPool, owner: &str, fingerprint: &str) -> Result<Outcome, DbError> {
    let mut tx=pool.begin().await?;
    owner_lock(&mut tx,owner).await?;
    sqlx::query("select owner_id from continuous_processes where owner_id=$1 order by algorithm_id for update").bind(owner).execute(&mut *tx).await?;
    sqlx::query("select id from continuous_slots where owner_id=$1 order by id for update").bind(owner).execute(&mut *tx).await?;
    // Lock before job rows, matching claim/finish and trigger accounting. Fixed
    // delivery can continue before/after this short mutation transaction.
    sqlx::query("select id from adaptive_notification_budget where id=1 for update").execute(&mut *tx).await?;
    let before=snapshot(&mut tx,owner).await?;
    if !before.eligible { return Ok(Outcome::Blocked(before)); }
    if before.fingerprint!=fingerprint { return Ok(Outcome::Stale(before)); }
    if before.counts==Counts::default() {
        tx.commit().await?;
        return Ok(Outcome::Deleted(Completed {deleted:before.counts,retained:before.retained}));
    }
    // Monotone fences survive purge; do not delete/recreate identities or reset sequences.
    sqlx::query("update continuous_owner_lanes set lease_token=lease_token+1,lease_owner=null,lease_until=null where owner_id=$1").bind(owner).execute(&mut *tx).await?;
    sqlx::query("update continuous_processes set config_revision=config_revision+1,active_generation_id=null,active_study_id=null,phase=null,error=null,retry_count=0,next_slot=0,cleanup_targets='{\"studies\":[],\"runs\":[],\"batches\":[]}',updated_at=now() where owner_id=$1").bind(owner).execute(&mut *tx).await?;
    sqlx::query("update continuous_slots set active_generation_id=null,pending_generation_id=null,pending_status=null,pending_error=null,readiness_frontier=null,live_watermark=null,study_id=null,mean_expectancy=null,selected_cell=null,status='stopped',updated_at=now() where owner_id=$1").bind(owner).execute(&mut *tx).await?;
    sqlx::query("update adaptive_live_warmup_lane set slot_id=null,generation_id=null,lease_owner=null,lease_until=null,lease_token=lease_token+1 where slot_id in(select id from continuous_slots where owner_id=$1) or generation_id in(select id from continuous_generations where owner_id=$1)").bind(owner).execute(&mut *tx).await?;
    sqlx::query("delete from adaptive_live_checkpoints where slot_id in(select id from continuous_slots where owner_id=$1)").bind(owner).execute(&mut *tx).await?;
    sqlx::query("with removed as(delete from adaptive_notification_outbox where owner_id=$1 and slot_id is not null returning setup_id) update live_setups l set notification_delivery_status='skipped',notification_delivery_error='continuous_history_purged',notification_delivery_updated_at=clock_timestamp() from removed r where l.id=r.setup_id and l.notification_delivery_status='pending'").bind(owner).execute(&mut *tx).await?;
    // Snapshot only protected output metadata. No research spec/best/bootstrap copy.
    let metadata="jsonb_build_object('generationId',g.id,'studyId',g.study_id,'slotId',g.slot_id,'ownerId',g.owner_id,'algorithmId',g.algorithm_id,'minSetupsPerDay',s.min_setups_per_day,'configRevision',g.config_revision,'executionStartedAt',g.execution_started_at,'observationCutoff',g.observation_cutoff)";
    sqlx::query(&format!("update replay_batches b set continuous_archive={metadata},continuous_analytics_archive=case when b.id=g.replay_batch_id then g.result->'analytics' end,continuous_analytics_context_archive=case when b.id=g.replay_batch_id then jsonb_build_object('effectiveAnalytics',g.result#>'{{best,analytics}}','direction',g.spec->'direction') end,continuous_replay_archive=case when b.id=g.replay_batch_id then g.replay_manifest end,continuous_generation_id=null from continuous_generations g join continuous_slots s on s.id=g.slot_id where b.continuous_generation_id=g.id and g.owner_id=$1 and b.owner_id=$1 and b.origin='continuous'"))
        .bind(owner).execute(&mut *tx).await?;
    sqlx::query(&format!("update replay_runs r set continuous_archive={metadata},continuous_generation_id=null from continuous_generations g join continuous_slots s on s.id=g.slot_id where r.continuous_generation_id=g.id and g.owner_id=$1 and r.owner_id=$1 and r.origin='continuous'"))
        .bind(owner).execute(&mut *tx).await?;
    sqlx::query(&format!("update live_setups l set adaptive_generation_archive={metadata},adaptive_generation_id=null from continuous_generations g join continuous_slots s on s.id=g.slot_id where l.adaptive_generation_id=g.id and g.owner_id=$1 and l.adaptive_slot_id in(select id from continuous_slots where owner_id=$1)"))
        .bind(owner).execute(&mut *tx).await?;
    sqlx::query("delete from optimization_studies where owner_id=$1 and origin='continuous'").bind(owner).execute(&mut *tx).await?;
    sqlx::query("delete from continuous_generations where owner_id=$1").bind(owner).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Outcome::Deleted(Completed {deleted:before.counts,retained:before.retained}))
}
