//! Real max1 control-pool cancellation/readback regression.
use super::{Durable, PromotionStore, persist};
use crate::WorkerError;
use futures::FutureExt;
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

struct BlockedPromotion {
    pool: PgPool,
    key: i64,
    attempts: AtomicUsize,
    rereads: AtomicUsize,
    completed_rereads: AtomicUsize,
}

impl PromotionStore for BlockedPromotion {
    async fn attempt(&self) -> Result<bool, WorkerError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let mut tx = self.pool.begin().await?;
        sqlx::query("select pg_advisory_xact_lock($1)")
            .bind(self.key)
            .execute(&mut *tx)
            .await?;
        // The blocker stays held across both attempts. The control session's
        // server lock timeout must release this transaction before the outer
        // attempt/readback guards, without waiting for the blocker to release.
        Err(super::invalid(
            "test advisory blocker was unexpectedly released",
        ))
    }

    async fn reread(&self) -> Result<Durable, WorkerError> {
        self.rereads.fetch_add(1, Ordering::SeqCst);
        let value: i32 = sqlx::query_scalar("select 1").fetch_one(&self.pool).await?;
        assert_eq!(value, 1);
        self.completed_rereads.fetch_add(1, Ordering::SeqCst);
        Ok(Durable::Predecessor)
    }
}

#[tokio::test]
async fn postgres_max_one_cancelled_promotion_releases_connection_for_predecessor_readback() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL absent; isolated promotion cancellation test skipped");
        return;
    };
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    assert!(
        options.get_database().unwrap_or("").ends_with("_test"),
        "promotion cancellation regression requires an isolated _test database"
    );
    let mut blocker = PgConnection::connect_with(&options).await.unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            options
                .application_name("setups-live-control")
                .options([("lock_timeout", "500ms"), ("statement_timeout", "750ms")]),
        )
        .await
        .unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        let configured: (String, String, String) = sqlx::query_as(
            "select current_setting('application_name'),current_setting('lock_timeout'),current_setting('statement_timeout')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            configured,
            ("setups-live-control".into(), "500ms".into(), "750ms".into())
        );
        let mut held = blocker.begin().await.unwrap();
        let key = rand::random::<i64>();
        sqlx::query("select pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(&mut *held)
            .await
            .unwrap();
        let store = BlockedPromotion {
            pool: pool.clone(),
            key,
            attempts: AtomicUsize::new(0),
            rereads: AtomicUsize::new(0),
            completed_rereads: AtomicUsize::new(0),
        };
        let started = Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(5), persist(&store)).await;
        let elapsed = started.elapsed();
        let counts = (
            store.attempts.load(Ordering::SeqCst),
            store.rereads.load(Ordering::SeqCst),
            store.completed_rereads.load(Ordering::SeqCst),
        );

        // Perform cleanup BEFORE the behavioral assertions: a regression must
        // not leave SQLx's pending rollback blocked on our advisory lock.
        held.rollback().await.unwrap();
        let mut returned = tokio::time::timeout(Duration::from_secs(2), pool.acquire())
            .await
            .expect("sole pool connection must be reacquirable after blocker release")
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i32>("select 1")
                .fetch_one(&mut *returned)
                .await
                .unwrap(),
            1
        );
        drop(returned);

        let outcome = outcome.expect("two bounded attempts/readbacks must finish within five seconds");
        assert_eq!(
            counts,
            (2, 2, 2),
            "expected two cancelled attempts and two real predecessor readbacks; elapsed={elapsed:?}, outcome={outcome:?}"
        );
        assert!(elapsed >= Duration::from_secs(1), "both attempts must reach their 500ms lock timeout");
        let error = outcome.expect_err("two predecessors preserve the last server lock timeout");
        match error {
            WorkerError::Db(setups_db::DbError::Sqlx(sqlx::Error::Database(error))) => {
                assert_eq!(error.code().as_deref(), Some("55P03"));
            }
            other => panic!("expected server lock_timeout before the 1s client guard, got {other}"),
        }
    })
    .catch_unwind()
    .await;
    // Closing the direct connection also releases its transaction lock if an
    // earlier setup/cleanup assertion unwound before the explicit rollback.
    blocker.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), pool.close())
        .await
        .expect("isolated promotion pool cleanup must finish");
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
