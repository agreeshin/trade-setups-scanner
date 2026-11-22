//! Durable owner-wide research admission gate; never changes live or desired state.
use crate::DbError;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceStatus {
    pub admission_paused: bool,
    pub state: &'static str,
    pub active_chains: i64,
    pub resumable_chains: i64,
    pub running_automatic_actual: i64,
    pub active_manual_actual: i64,
    pub updated_at: Option<DateTime<Utc>>,
}

pub async fn status(pool: &PgPool, owner: &str) -> Result<MaintenanceStatus, DbError> {
    let row=sqlx::query("select coalesce((select admission_paused from continuous_maintenance where owner_id=$1),false) paused,(select updated_at from continuous_maintenance where owner_id=$1) updated_at,count(*)::bigint active,count(*) filter(where a.snapshot_manifest is not null)::bigint resumable from continuous_generations g left join continuous_runtime_artifacts a on a.generation_id=g.id where g.owner_id=$1 and g.status='running'")
        .bind(owner).fetch_one(pool).await?;
    let admission_paused = row.try_get("paused")?;
    let active_chains = row.try_get("active")?;
    let (running_automatic_actual,active_manual_actual):(i64,i64)=sqlx::query_as("select count(*) filter(where request_origin='automatic' and status='running'),count(*) filter(where request_origin='manual' and status in ('queued','running')) from study_actualizations where owner_id=$1").bind(owner).fetch_one(pool).await?;
    Ok(MaintenanceStatus {
        admission_paused,
        state: if admission_paused {
            if active_chains == 0 && running_automatic_actual == 0 {
                "drained"
            } else {
                "draining"
            }
        } else {
            "running"
        },
        active_chains,
        running_automatic_actual,
        active_manual_actual,
        resumable_chains: row.try_get("resumable")?,
        updated_at: row.try_get("updated_at")?,
    })
}

pub async fn set_paused(
    pool: &PgPool,
    owner: &str,
    paused: bool,
) -> Result<MaintenanceStatus, DbError> {
    let mut tx = pool.begin().await?;
    super::owner_lock(&mut tx, owner).await?;
    sqlx::query("insert into continuous_maintenance(owner_id,admission_paused) values($1,$2) on conflict(owner_id) do update set admission_paused=excluded.admission_paused,updated_at=case when continuous_maintenance.admission_paused<>excluded.admission_paused then now() else continuous_maintenance.updated_at end")
        .bind(owner).bind(paused).execute(&mut *tx).await?;
    tx.commit().await?;
    status(pool, owner).await
}
