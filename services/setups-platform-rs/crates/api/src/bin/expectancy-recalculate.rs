//! Authorized offline operator command. Default compute has ZERO database
//! writes; stage/migrate/publish are separate explicit phases, never automatic.
use setups_api::historical_expectancy::{self, HistoricalCandidate, MAX_CANDIDATE_BYTES};
use setups_db::expectancy;
use sqlx::postgres::PgPoolOptions;
use std::{error::Error, fs::OpenOptions, io::{Read,Write}, path::Path};

type Failure=Box<dyn Error+Send+Sync>;
fn invalid(message:&str)->Failure {std::io::Error::other(message).into()}
fn env(name:&str)->Result<String,Failure> {std::env::var(name).map_err(|_|invalid(&format!("missing environment variable {name}")))}

fn write_new(path:&Path,candidate:&HistoricalCandidate)->Result<(),Failure> {
    let bytes=serde_json::to_vec(candidate)?;
    if bytes.len()>MAX_CANDIDATE_BYTES {return Err(invalid("candidate exceeds byte budget"));}
    let mut options=OpenOptions::new();options.write(true).create_new(true);
    #[cfg(unix)] {use std::os::unix::fs::OpenOptionsExt;options.mode(0o600);}
    let mut file=match options.open(path) {
        Ok(file)=>file,
        Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>{
            let previous=read_candidate(path)?;
            if previous.owner!=candidate.owner || previous.generation_id!=candidate.generation_id
                || previous.original_artifact_id!=candidate.original_artifact_id || previous.config_revision!=candidate.config_revision
                || previous.recalculation.input_hash!=candidate.recalculation.input_hash
                || previous.numerical!=candidate.numerical || previous.comparison!=candidate.comparison {
                return Err(invalid("immutable candidate file conflicts with this calculation"));
            }
            return Ok(());
        },Err(error)=>return Err(error.into()),
    };
    file.write_all(&bytes)?;file.sync_all()?;Ok(())
}
fn read_candidate(path:&Path)->Result<HistoricalCandidate,Failure> {
    let file=std::fs::File::open(path)?;
    if file.metadata()?.len()>MAX_CANDIDATE_BYTES as u64 {return Err(invalid("candidate exceeds byte budget"));}
    let mut bytes=Vec::new();file.take(MAX_CANDIDATE_BYTES as u64+1).read_to_end(&mut bytes)?;
    if bytes.len()>MAX_CANDIDATE_BYTES {return Err(invalid("candidate exceeds byte budget"));}
    Ok(serde_json::from_slice(&bytes)?)
}

#[tokio::main(flavor="current_thread")]
async fn main()->Result<(),Failure> {
    let args:Vec<_>=std::env::args().skip(1).collect();
    if args.is_empty() || args[0]=="--help" {
        println!("expectancy-recalculate compute OWNER GENERATION OUTPUT_DIRECTORY\nexpectancy-recalculate stage CANDIDATE_FILE\nexpectancy-recalculate migrate\nexpectancy-recalculate publish OWNER GENERATION REVISION_KEY\nEnv: DATABASE_URL; compute additionally MARKET_DATA_API_BASE_URL, MARKET_DATA_API_KEY; optional HISTORICAL_MAX_SECONDS (default 900, max 1800).\nCompute is read-only and writes a content-addressed 0600 candidate into an existing output directory. Stage never promotes. Migrate/stage/publish only after the compatible reader/GC deployment. No historical delivery or scientific mutation.");return Ok(());
    }
    let valid=matches!((args[0].as_str(),args.len()),("compute",4)|("stage",2)|("migrate",1)|("publish",4));
    if !valid {return Err(invalid("invalid arguments; use --help"));}
    let pool=PgPoolOptions::new().max_connections(2).acquire_timeout(std::time::Duration::from_secs(10)).connect(&env("DATABASE_URL")?).await?;
    match args[0].as_str() {
        "compute"=>{
            if !Path::new(&args[3]).is_dir() {return Err(invalid("output directory must already exist"));}
            let generation=args[2].parse()?;
            let max_seconds=std::env::var("HISTORICAL_MAX_SECONDS").unwrap_or_else(|_|"900".into()).parse()?;
            let client=setups_mdclient::MarketDataClient::new(env("MARKET_DATA_API_BASE_URL")?,env("MARKET_DATA_API_KEY")?);
            let candidate=historical_expectancy::compute(&pool,&client,&args[1],generation,max_seconds).await?;
            let output=Path::new(&args[3]).join(format!("generation-{generation}-{}.json",candidate.recalculation.input_hash));
            write_new(&output,&candidate)?;
            println!("{}",serde_json::json!({"status":if candidate.numerical.is_some(){"computed_matched"}else{"computed_mismatch"},"generationId":generation,
                "revisionKey":candidate.recalculation.input_hash,"candleCount":candidate.recalculation.candle_count,"missingMinutes":candidate.recalculation.missing_minutes,"output":output}));
        },
        "stage"=>{
            let candidate=read_candidate(Path::new(&args[1]))?;
            let stage=candidate.stage(&pool).await?;
            println!("{}",serde_json::json!({"status":if stage.is_some(){"staged"}else{"binding_changed"},"result":stage}));
        },
        "migrate"=>println!("{}",setups_db::migrate::ensure_migrations(&pool).await?),
        "publish"=>{
            let generation=args[2].parse()?;
            let published=expectancy::publish_historical_recalculation(&pool,&args[1],generation,&args[3]).await?;
            println!("{}",serde_json::json!({"status":if published.is_some(){"published"}else{"binding_changed"},"artifactId":published.as_ref().map(|row|row.artifact.id),"availableFromUnix":published.as_ref().map(|row|row.available_from_unix)}));
        },_=>unreachable!(),
    }
    pool.close().await;Ok(())
}
