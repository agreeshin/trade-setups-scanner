//! Isolated PostgreSQL evidence for the physical data/control reserve.
use futures::FutureExt;
use serde_json::json;
use setups_worker::adaptive::{AdaptiveLiveRuntime, AdaptiveRuntimeLimits};
use sqlx::{
    Connection, PgConnection, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{sync::atomic::AtomicBool, time::Duration};

async fn control_probe(data: &PgPool, control: &PgPool) {
    tokio::time::timeout(Duration::from_millis(500), async {
        assert_eq!(
            setups_db::continuous::profiles::accept_handoffs(control, "admin")
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            setups_db::settings_get(control, "live").await.unwrap(),
            Some(json!({"enabled":false}))
        );
        // Disabled tick must perform its handoff/settings/stop-control work
        // without touching saturated data or the deliberately unavailable HTTP.
        let mut runtime = AdaptiveLiveRuntime::new(AdaptiveRuntimeLimits::default());
        let client = setups_mdclient::MarketDataClient::new("http://127.0.0.1:1", "unused");
        runtime
            .tick_at_on_pools(data, control, &client, &AtomicBool::new(false), 180)
            .await
            .unwrap();
        let mut tx = control.begin().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i32>("select 1")
                .fetch_one(&mut *tx)
                .await
                .unwrap(),
            1
        );
        tx.commit().await.unwrap();
    })
    .await
    .expect("data saturation must not queue control checkout/transaction");
}

async fn case(total: u32) {
    let options: PgConnectOptions = std::env::var("TEST_DATABASE_URL")
        .expect("isolated TEST_DATABASE_URL required")
        .parse()
        .unwrap();
    assert!(options.get_database().unwrap_or("").ends_with("_test"));
    let name = format!(
        "live_reserve_{total}_{}_{}_test",
        std::process::id(),
        rand::random::<u32>()
    );
    let mut admin = PgConnection::connect_with(&options).await.unwrap();
    sqlx::query(&format!("create database {name}"))
        .execute(&mut admin)
        .await
        .unwrap();
    let options = options.database(&name);
    let data_role = format!("reserve-data-{total}");
    let data = PgPoolOptions::new()
        .max_connections(total - 1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options.clone().application_name(&data_role))
        .await
        .unwrap();
    let control = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options.clone().application_name("reserve-control"))
        .await
        .unwrap();
    let result=std::panic::AssertUnwindSafe(async {
        let baseline=include_str!("../../db/migrations/0010_baseline.sql").lines()
            .filter(|line|!line.starts_with('\\') && !line.starts_with("SET transaction_timeout"))
            .collect::<Vec<_>>().join("\n");
        let mut connection=data.acquire().await.unwrap();
        sqlx::raw_sql(&baseline).execute(&mut *connection).await.unwrap();
        sqlx::raw_sql("set search_path=public; create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null); insert into kysely_migration values('0010_baseline.sql','fixture');")
            .execute(&mut *connection).await.unwrap();
        drop(connection);
        setups_db::migrate::ensure_migrations(&data).await.unwrap();
        setups_db::settings_set(&control,"live",&json!({"enabled":false})).await.unwrap();
        assert_eq!(data.options().get_max_connections()+control.options().get_max_connections(),total);

        let mut held=Vec::new();
        for _ in 1..total {held.push(data.acquire().await.unwrap());}
        assert!(tokio::time::timeout(Duration::from_millis(50),data.acquire()).await.is_err());
        control_probe(&data,&control).await;
        drop(held);

        // Real delivery transactions contend on the singleton budget row.
        // Repeat with caller cancellation to include SQLx's deferred rollback
        // and return-to-pool phase, not merely successful query completion.
        for cancel in [false,true] {
            let mut blocker=PgConnection::connect_with(&options).await.unwrap();
            let mut tx=blocker.begin().await.unwrap();
            sqlx::query("select id from adaptive_notification_budget where id=1 for update").execute(&mut *tx).await.unwrap();
            let mut tasks=tokio::task::JoinSet::new();
            for worker in 0..total {
                let data=data.clone();
                tasks.spawn(async move {setups_db::continuous::outbox::claim_notifications(&data,&format!("reserve-{worker}"),1).await});
            }
            tokio::time::timeout(Duration::from_secs(3),async {
                loop {
                    sqlx::query("select pg_stat_clear_snapshot()").execute(&mut *tx).await.unwrap();
                    let waiting:i64=sqlx::query_scalar("select count(*) from pg_stat_activity where datname=current_database() and application_name=$1 and wait_event_type='Lock'")
                        .bind(&data_role).fetch_one(&mut *tx).await.unwrap();
                    if waiting==i64::from(total-1) {break;}
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await.expect("all data connections must reach the controlled row blocker");
            assert_eq!(data.size(),total-1);
            assert_eq!(data.num_idle(),0);
            if cancel {tasks.abort_all();while tasks.join_next().await.is_some() {}}
            control_probe(&data,&control).await;
            tx.rollback().await.unwrap();
            tokio::time::timeout(Duration::from_secs(5),async {
                while let Some(result)=tasks.join_next().await {assert!(result.unwrap().unwrap().is_empty());}
            }).await.expect("blocked delivery work must drain after lock release");
            let mut returned=Vec::new();
            for _ in 1..total {
                returned.push(tokio::time::timeout(Duration::from_secs(2),data.acquire()).await.unwrap().unwrap());
            }
            drop(returned);
            blocker.close().await.unwrap();
        }
    }).catch_unwind().await;
    data.close().await;
    control.close().await;
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
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn live_total_four_reserves_control_during_data_saturation_and_rollback() {
    case(4).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn live_total_two_reserves_control_during_data_saturation_and_rollback() {
    case(2).await;
}
