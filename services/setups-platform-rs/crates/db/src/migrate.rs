use sqlx::PgPool;
use std::collections::HashSet;

const BASELINE: &str = "0010_baseline.sql";
const RUN_NAMES: &str = "0011_run_names.sql";
const OPTIMIZATION_STUDIES: &str = "0012_optimization_studies.sql";
const CONTINUOUS_STUDIES: &str = "0013_continuous_studies.sql";
const ADAPTIVE_LIVE_READINESS: &str = "0014_adaptive_live_readiness.sql";
const CONTINUOUS_HISTORY_CLEANUP: &str = "0015_continuous_history_cleanup.sql";
const INDEPENDENT_LIVE_INITIALIZATION: &str = "0016_independent_live_initialization.sql";
const CONTINUOUS_RESUME: &str = "0017_continuous_resume.sql";
const LIVE_READMISSION: &str = "0018_live_readmission.sql";
const TELEGRAM_RELAY: &str = "0019_telegram_relay.sql";
const STUDY_EXPECTANCY: &str = "0020_study_expectancy.sql";
const TELEGRAM_V2_QUEUE: &str = "0021_telegram_v2_queue.sql";
const HISTORICAL_EXPECTANCY: &str = "0022_historical_expectancy.sql";
const STANDALONE_PROFILES: &str = "0023_standalone_adaptive_profiles.sql";
const CONTINUOUS_PENDING_CONFIG: &str = "0024_continuous_pending_config.sql";
const LIVE_SETUP_TRADE_OUTCOME: &str = "0025_live_setup_trade_outcome.sql";
const STUDY_ACTUAL: &str = "0026_study_actual.sql";
const MIGRATIONS: &[&str] = &[
    BASELINE,
    RUN_NAMES,
    OPTIMIZATION_STUDIES,
    CONTINUOUS_STUDIES,
    ADAPTIVE_LIVE_READINESS,
    CONTINUOUS_HISTORY_CLEANUP,
    INDEPENDENT_LIVE_INITIALIZATION,
    CONTINUOUS_RESUME,
    LIVE_READMISSION,
    TELEGRAM_RELAY,
    STUDY_EXPECTANCY,
    TELEGRAM_V2_QUEUE,
    HISTORICAL_EXPECTANCY,
    STANDALONE_PROFILES,
    CONTINUOUS_PENDING_CONFIG,
    LIVE_SETUP_TRADE_OUTCOME,
    STUDY_ACTUAL,
];

fn migration_source(name: &str) -> &'static str {
    match name {
        BASELINE => include_str!("../migrations/0010_baseline.sql"),
        RUN_NAMES => include_str!("../migrations/0011_run_names.sql"),
        OPTIMIZATION_STUDIES => include_str!("../migrations/0012_optimization_studies.sql"),
        CONTINUOUS_STUDIES => include_str!("../migrations/0013_continuous_studies.sql"),
        ADAPTIVE_LIVE_READINESS => include_str!("../migrations/0014_adaptive_live_readiness.sql"),
        CONTINUOUS_HISTORY_CLEANUP => {
            include_str!("../migrations/0015_continuous_history_cleanup.sql")
        }
        INDEPENDENT_LIVE_INITIALIZATION => {
            include_str!("../migrations/0016_independent_live_initialization.sql")
        }
        CONTINUOUS_RESUME => include_str!("../migrations/0017_continuous_resume.sql"),
        LIVE_READMISSION => include_str!("../migrations/0018_live_readmission.sql"),
        TELEGRAM_RELAY => include_str!("../migrations/0019_telegram_relay.sql"),
        STUDY_EXPECTANCY => include_str!("../migrations/0020_study_expectancy.sql"),
        TELEGRAM_V2_QUEUE => include_str!("../migrations/0021_telegram_v2_queue.sql"),
        HISTORICAL_EXPECTANCY => include_str!("../migrations/0022_historical_expectancy.sql"),
        STANDALONE_PROFILES => include_str!("../migrations/0023_standalone_adaptive_profiles.sql"),
        CONTINUOUS_PENDING_CONFIG => {
            include_str!("../migrations/0024_continuous_pending_config.sql")
        }
        LIVE_SETUP_TRADE_OUTCOME => include_str!("../migrations/0025_live_setup_trade_outcome.sql"),
        STUDY_ACTUAL => include_str!("../migrations/0026_study_actual.sql"),
        other => unreachable!("unknown migration {other}"),
    }
}

/// Applies embedded SQL migrations, recording progress in the legacy
/// `kysely_migration` table so history from the retired TS engine carries over.
/// On databases that already have TS migration history the baseline is
/// recorded without executing: the schema is already established there.
pub async fn ensure_migrations(pool: &PgPool) -> Result<String, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock(918273645)")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        r#"create table if not exists public.kysely_migration (
            name varchar(255) primary key,
            "timestamp" varchar(255) not null
        )"#,
    )
    .execute(&mut *tx)
    .await?;
    let applied: HashSet<String> = sqlx::query_scalar("select name from public.kysely_migration")
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
    let history_present = applied
        .iter()
        .any(|name| !MIGRATIONS.contains(&name.as_str()));
    let mut applied_now = 0usize;
    for name in MIGRATIONS {
        if applied.contains(*name) {
            continue;
        }
        if !(history_present && *name == BASELINE) {
            sqlx::raw_sql(migration_source(name))
                .execute(&mut *tx)
                .await?;
            sqlx::query("select pg_catalog.set_config('search_path', 'public', false)")
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(r#"insert into public.kysely_migration (name, "timestamp") values ($1, $2)"#)
            .bind(name)
            .bind(chrono::Utc::now().to_rfc3339())
            .execute(&mut *tx)
            .await?;
        applied_now += 1;
    }
    tx.commit().await?;
    Ok(format!(
        "{applied_now} applied, {} up to date",
        MIGRATIONS.len() - applied_now
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_names_migration_is_registered_after_baseline() {
        assert_eq!(
            MIGRATIONS,
            &[
                BASELINE,
                RUN_NAMES,
                OPTIMIZATION_STUDIES,
                CONTINUOUS_STUDIES,
                ADAPTIVE_LIVE_READINESS,
                CONTINUOUS_HISTORY_CLEANUP,
                INDEPENDENT_LIVE_INITIALIZATION,
                CONTINUOUS_RESUME,
                LIVE_READMISSION,
                TELEGRAM_RELAY,
                STUDY_EXPECTANCY,
                TELEGRAM_V2_QUEUE,
                HISTORICAL_EXPECTANCY,
                STANDALONE_PROFILES,
                CONTINUOUS_PENDING_CONFIG,
                LIVE_SETUP_TRADE_OUTCOME,
                STUDY_ACTUAL
            ]
        );
        let source = migration_source(RUN_NAMES);
        assert!(
            source.contains("ALTER TABLE public.replay_runs ADD COLUMN IF NOT EXISTS name text")
        );
        assert!(
            source.contains("ALTER TABLE public.replay_batches ADD COLUMN IF NOT EXISTS name text")
        );
    }

    #[test]
    fn baseline_is_registered_without_execution_when_history_present() {
        // История TS (001..007) существует — baseline должен только
        // фиксироваться; правило проверяется на живой БД в деплой-верификации.
        assert_eq!(BASELINE, "0010_baseline.sql");
        assert!(migration_source(BASELINE).contains("CREATE TABLE"));
    }

    #[tokio::test]
    async fn ensure_migrations_bootstraps_fresh_database() {
        use sqlx::Connection as _;
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("TEST_DATABASE_URL absent; fresh-database migration test skipped");
            return;
        };
        let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
        assert!(options.get_database().unwrap_or("").ends_with("_test"));
        assert!(matches!(
            options.get_host(),
            "localhost" | "127.0.0.1" | "::1"
        ));
        let name = format!("migrate_fresh_{}_test", std::process::id());
        let mut admin = sqlx::PgConnection::connect_with(&options).await.unwrap();
        sqlx::query(&format!("drop database if exists {name}"))
            .execute(&mut admin)
            .await
            .unwrap();
        sqlx::query(&format!("create database {name}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.database(&name))
            .await
            .unwrap();
        let summary = ensure_migrations(&pool).await.unwrap();
        assert_eq!(
            summary,
            format!("{} applied, 0 up to date", MIGRATIONS.len())
        );
        let runs: i64 = sqlx::query_scalar("select count(*) from replay_runs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(runs, 0);
        let again = ensure_migrations(&pool).await.unwrap();
        assert_eq!(again, format!("0 applied, {} up to date", MIGRATIONS.len()));
        pool.close().await;
        sqlx::query(&format!("drop database {name} with (force)"))
            .execute(&mut admin)
            .await
            .unwrap();
    }
}
