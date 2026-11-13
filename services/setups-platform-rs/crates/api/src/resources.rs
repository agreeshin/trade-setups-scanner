//! Enforced connection and resident-background-chain budgets, not OS CPU/RAM quotas.
use crate::continuous::ContinuousRuntimeConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeResources {
    pub total_db_connections: u32,
    pub api_db_connections: u32,
    pub live_db_connections: u32,
    pub manual_db_connections: u32,
    pub continuous_db_connections: u32,
    pub background_cpu_threads: usize,
    pub background_memory_bytes: u64,
    pub notification_max_jobs: i64,
    pub notification_max_bytes: i64,
}

impl RuntimeResources {
    pub fn from_lookup(get: &mut dyn FnMut(&str)->Option<String>) -> Result<Self,String> {
        fn bounded(get: &mut dyn FnMut(&str)->Option<String>, key: &str, fallback: u64, min: u64, max: u64) -> Result<u64,String> {
            let value = match get(key) {Some(raw)=>raw.parse::<u64>().map_err(|_|format!("{key} must be an integer"))?,None=>fallback};
            if !(min..=max).contains(&value) {return Err(format!("{key} must be in {min}..={max}"));}
            Ok(value)
        }
        let detected = std::thread::available_parallelism().map_or(2,usize::from);
        let result = Self {
            total_db_connections:bounded(get,"RUNTIME_MAX_DB_CONNECTIONS",20,8,256)? as u32,
            api_db_connections:bounded(get,"API_DB_CONNECTIONS",6,2,128)? as u32,
            live_db_connections:bounded(get,"LIVE_DB_CONNECTIONS",4,2,64)? as u32,
            manual_db_connections:bounded(get,"MANUAL_DB_CONNECTIONS",4,2,64)? as u32,
            continuous_db_connections:bounded(get,"CONTINUOUS_DB_CONNECTIONS",4,2,64)? as u32,
            background_cpu_threads:bounded(get,"CONTINUOUS_TOTAL_CPU_THREADS",detected.saturating_sub(2).clamp(1,4) as u64,1,64)? as usize,
            background_memory_bytes:bounded(get,"CONTINUOUS_TOTAL_MEMORY_BYTES",16*1024*1024*1024,1024*1024,64*1024*1024*1024)?,
            notification_max_jobs:bounded(get,"LIVE_NOTIFICATION_MAX_PENDING",10000,1,100000)? as i64,
            notification_max_bytes:bounded(get,"LIVE_NOTIFICATION_MAX_PENDING_BYTES",64*1024*1024,32768,1024*1024*1024)? as i64,
        };
        // The optimizer's detached advisory-lock socket is outside SQLx's pool count.
        let connections = result.api_db_connections+result.live_db_connections+result.manual_db_connections+result.continuous_db_connections+1;
        if connections>result.total_db_connections {return Err(format!("database lane budgets plus one optimizer-owner connection ({connections}) exceed RUNTIME_MAX_DB_CONNECTIONS ({})",result.total_db_connections));}
        Ok(result)
    }

    pub fn validate_continuous(&self, config: &ContinuousRuntimeConfig) -> Result<(),String> {
        if config.max_concurrent_chains==0 || config.max_concurrent_chains>config.workers {
            return Err("CONTINUOUS_MAX_CONCURRENT_CHAINS must be in 1..=CONTINUOUS_WORKERS".into());
        }
        let connections=config.max_concurrent_chains.checked_mul(2).and_then(|v|v.checked_add(1)).ok_or("continuous connection budget overflow")?;
        if connections>self.continuous_db_connections as usize {
            return Err("CONTINUOUS_DB_CONNECTIONS must cover resident chains, monitors and one coordinator".into());
        }
        let cpu = config.max_concurrent_chains.checked_mul(config.optimizer.cpu_threads).ok_or("continuous CPU budget overflow")?;
        let bytes = (config.optimizer.max_snapshot_bytes as u64).checked_add(config.optimizer.max_working_bytes as u64)
            .and_then(|bytes|bytes.checked_mul(config.max_concurrent_chains as u64)).ok_or("continuous resident memory budget overflow")?;
        if cpu>self.background_cpu_threads {return Err(format!("continuous kernels request {cpu} CPU threads, exceeding CONTINUOUS_TOTAL_CPU_THREADS ({})",self.background_cpu_threads));}
        if bytes>self.background_memory_bytes {return Err(format!("continuous resident chains request {bytes} snapshot/working bytes, exceeding CONTINUOUS_TOTAL_MEMORY_BYTES ({})",self.background_memory_bytes));}
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn includes_detached_owner_and_refuses_pool_overcommit() {
        let default = RuntimeResources::from_lookup(&mut |_|None).unwrap();
        assert_eq!(default.total_db_connections,20);
        assert!(RuntimeResources::from_lookup(&mut |key| (key=="RUNTIME_MAX_DB_CONNECTIONS").then(||"18".into())).is_err());
    }
    #[test]
    fn validates_total_resident_background_cpu_and_memory_without_clipping_requests() {
        let mut budgets = RuntimeResources::from_lookup(&mut |_|None).unwrap();
        let mut config = ContinuousRuntimeConfig::default();
        budgets.background_cpu_threads=1;
        assert!(budgets.validate_continuous(&config).is_ok());
        config.workers=2; config.max_concurrent_chains=2;
        budgets.continuous_db_connections=8;
        assert!(budgets.validate_continuous(&config).unwrap_err().contains("CPU"));
        config.max_concurrent_chains=1;
        budgets.background_memory_bytes=1;
        assert!(budgets.validate_continuous(&config).unwrap_err().contains("resident"));
    }
}
