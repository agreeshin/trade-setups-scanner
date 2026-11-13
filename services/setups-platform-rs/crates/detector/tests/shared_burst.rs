//! Opt-in synthetic DETECTOR-ONLY release benchmark. Actual independent history
//! is allocated/warmed for all 820 instruments. Within each instrument, profiles
//! are homogeneous cheap scalar clones of ONE warmed profile (not extra history).
//!
//! cargo test --release -p setups-detector --test shared_burst -- --ignored --nocapture --test-threads=1
//! Optional: SHARED_BENCH_MAX_SECONDS=90 (default), SHARED_BENCH_BURSTS=30 (1..=120).
//! This is not an end-to-end live/research, market-data, DB, notification or UI SLO.
use setups_detector::shared::{
    HistoryIdentity, HistoryLimits, HistoryPosition, SharedHistory, SharedProfile,
};
use setups_domain::{AlgorithmParams, DetectorBar, SpikeParams};
use std::{
    hint::black_box,
    mem::size_of,
    time::{Duration, Instant},
};

const SYMBOLS: usize = 820;
const DAYS: usize = 30;
const MINUTES: usize = DAYS * 1440;
const T0: i64 = 1_699_999_980;
const MAX_OWNED_BYTES: usize = 4 * 1024 * 1024 * 1024;

struct Instrument {
    history: SharedHistory,
    profiles: Vec<SharedProfile>,
    normal_close: String,
    signal_close: String,
}

fn memory(label: &str) {
    match std::fs::read_to_string("/proc/self/status") {
        Ok(status) => {
            let rss = status
                .lines()
                .find(|l| l.starts_with("VmRSS:"))
                .unwrap_or("VmRSS: unavailable");
            let peak = status
                .lines()
                .find(|l| l.starts_with("VmHWM:"))
                .unwrap_or("VmHWM: unavailable");
            println!("memory phase={label} {rss} {peak}");
        }
        Err(error) => println!("memory phase={label} unavailable={error}"),
    }
}

fn allocated_bytes(instruments: &Vec<Instrument>) -> usize {
    // Actual collection capacities, not 820 times a single-symbol measurement.
    // SharedProfile's conservative accounting is counted ONCE per symbol's
    // immutable Arc config; every mutable profile allocation is counted directly.
    instruments.capacity() * size_of::<Instrument>()
        + instruments
            .iter()
            .map(|instrument| {
                instrument.history.retained_bytes() - size_of::<SharedHistory>()
                    + instrument.profiles.capacity() * size_of::<SharedProfile>()
                    + instrument.profiles[0].retained_bytes()
                    - size_of::<SharedProfile>()
                    + instrument.normal_close.capacity()
                    + instrument.signal_close.capacity()
            })
            .sum::<usize>()
}

fn check_deadline(start: Instant, max_seconds: u64, phase: &str, completed: usize) {
    assert!(
        start.elapsed() < Duration::from_secs(max_seconds),
        "benchmark timebox exhausted: phase={phase} completed={completed} max_seconds={max_seconds}"
    );
}

#[test]
#[ignore = "opt-in 820-instrument 30-day release benchmark; allocates about 3 GiB, bounded to 4 GiB"]
fn actual_820_instrument_histories_homogeneous_10_and_60_profile_bursts() {
    assert!(!cfg!(debug_assertions), "run this benchmark with --release");
    let max_seconds = std::env::var("SHARED_BENCH_MAX_SECONDS")
        .map(|s| s.parse::<u64>().unwrap())
        .unwrap_or(90);
    let bursts = std::env::var("SHARED_BENCH_BURSTS")
        .map(|s| s.parse::<usize>().unwrap())
        .unwrap_or(30);
    assert!((1..=120).contains(&bursts));
    assert!((10..=300).contains(&max_seconds));
    let capacity = MINUTES + 2; // full departure anchor plus one incoming minute
    let close_bytes = capacity * 16;
    let per_history = SharedHistory::required_bytes(capacity, close_bytes).unwrap();
    assert!(per_history.checked_mul(SYMBOLS).unwrap() < MAX_OWNED_BYTES);
    // Refuse obvious memory pressure BEFORE allocating the actual universe.
    if let Ok(info) = std::fs::read_to_string("/proc/meminfo") {
        let available = info
            .lines()
            .find(|line| line.starts_with("MemAvailable:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024;
        assert!(
            available > MAX_OWNED_BYTES as u64 + 1024 * 1024 * 1024,
            "need 4 GiB benchmark cap plus 1 GiB available-memory reserve; available={available}"
        );
    }
    println!(
        "config symbols={SYMBOLS} baseline_days={DAYS} homogeneous_profiles=[10,60] bursts={bursts} state_size={} per_history_requested={per_history} max_owned_bytes={MAX_OWNED_BYTES} max_seconds={max_seconds}",
        size_of::<SharedProfile>()
    );
    memory("before");
    let start = Instant::now();
    let mut instruments = Vec::with_capacity(SYMBOLS);
    let mut first_symbol_seconds = 0.0;
    for symbol in 0..SYMBOLS {
        check_deadline(start, max_seconds, "warmup", symbol);
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&(symbol as u64).to_le_bytes());
        let identity = HistoryIdentity(key);
        let mut history = SharedHistory::new(
            identity,
            HistoryLimits {
                capacity_minutes: capacity,
                max_close_bytes: close_bytes,
                max_bytes: per_history,
            },
        )
        .unwrap();
        let params = SpikeParams {
            baseline_long_days: 30,
            baseline_short_days: 7,
            window_minutes: 5,
            price_growth_pct: "12".into(),
            volume_multiplier: "3".into(),
            cooldown_minutes: 0,
            max_gap_percent: "1".into(),
            max_consecutive_gap_minutes: 5,
            required_recent_minutes: 10,
        };
        let mut profile = SharedProfile::new(
            AlgorithmParams::Spike(params),
            identity,
            HistoryPosition(0),
            None,
        )
        .unwrap();
        let normal_close = format!("{}.00000000", 10000 + symbol);
        let signal_close = format!("{}.00000000", (10000 + symbol) * 2);
        let mut bar = DetectorBar {
            open_time_unix: T0,
            close: normal_close.clone(),
            quote_turnover: "100.00000000".into(),
        };
        for minute in 0..MINUTES {
            bar.open_time_unix = T0 + minute as i64 * 60;
            let position = history.push(&bar, bar.open_time_unix + 60).unwrap();
            black_box(profile.push(&history, position).unwrap());
        }
        instruments.push(Instrument {
            history,
            profiles: vec![profile],
            normal_close,
            signal_close,
        });
        if symbol == 0 {
            first_symbol_seconds = start.elapsed().as_secs_f64();
            println!("warmup first_symbol_seconds={first_symbol_seconds:.6}");
        }
        if (symbol + 1) % 100 == 0 {
            println!(
                "warmup completed_symbols={} elapsed_seconds={:.3}",
                symbol + 1,
                start.elapsed().as_secs_f64()
            );
        }
    }
    println!(
        "warmup actual_symbols={SYMBOLS} profiles_per_symbol=1 real_bars={} elapsed_seconds={:.6} first_symbol_seconds={first_symbol_seconds:.6}",
        SYMBOLS * MINUTES,
        start.elapsed().as_secs_f64()
    );
    memory("warmed_820");
    let mut next_minute = MINUTES;
    for count in [10usize, 60] {
        for instrument in &mut instruments {
            let seed = instrument.profiles[0].clone();
            instrument.profiles.resize(count, seed);
        }
        let before_bytes = allocated_bytes(&instruments);
        assert!(before_bytes < MAX_OWNED_BYTES);
        println!(
            "allocated phase=profiles_{count} actual_histories={} actual_profile_count={} owned_capacity_bytes={before_bytes} history_capacity_bytes={} profile_state_size={} profile_bytes_conservative={}",
            instruments.len(),
            SYMBOLS * count,
            instruments
                .iter()
                .map(|i| i.history.retained_bytes())
                .sum::<usize>(),
            size_of::<SharedProfile>(),
            instruments[0].profiles[0].retained_bytes()
        );
        memory(&format!("profiles_{count}"));
        let mut timings = Vec::with_capacity(bursts);
        let mut setups = 0usize;
        for burst in 0..bursts {
            check_deadline(start, max_seconds, "burst", burst);
            let tick = Instant::now();
            for instrument in &mut instruments {
                let input = DetectorBar {
                    open_time_unix: T0 + next_minute as i64 * 60,
                    close: if burst % 2 == 0 {
                        instrument.signal_close.clone()
                    } else {
                        instrument.normal_close.clone()
                    },
                    quote_turnover: if burst % 2 == 0 {
                        "1000000.00000000".into()
                    } else {
                        "100.00000000".into()
                    },
                };
                let anchor = instrument
                    .profiles
                    .iter()
                    .map(SharedProfile::retention_anchor)
                    .min()
                    .unwrap();
                let position = instrument
                    .history
                    .push_retaining(&input, input.open_time_unix + 60, anchor)
                    .unwrap();
                for profile in &mut instrument.profiles {
                    // Simulate worker's scalar clone -> compute -> successful CAS
                    // adoption. No DB wait and no retained result/event collection.
                    let mut speculative = profile.clone();
                    let result = speculative.push(&instrument.history, position).unwrap();
                    setups += usize::from(result.setup.is_some());
                    black_box(result);
                    *profile = speculative;
                }
            }
            next_minute += 1;
            timings.push(tick.elapsed().as_secs_f64() * 1000.0);
        }
        timings.sort_by(f64::total_cmp);
        let percentile = |p: usize| timings[(timings.len() * p).div_ceil(100).saturating_sub(1)];
        assert_eq!(
            allocated_bytes(&instruments),
            before_bytes,
            "hot path retained collections must plateau"
        );
        println!(
            "result symbols={SYMBOLS} profiles={count} bursts={bursts} evaluations={} emitted_setups={setups} burst_min_ms={:.6} burst_p50_ms={:.6} burst_p95_ms={:.6} burst_p99_ms={:.6} burst_max_ms={:.6} owned_capacity_bytes={before_bytes} elapsed_seconds={:.6}",
            SYMBOLS * count * bursts,
            timings[0],
            percentile(50),
            percentile(95),
            percentile(99),
            timings[timings.len() - 1],
            start.elapsed().as_secs_f64()
        );
        memory(&format!("after_bursts_{count}"));
    }
    drop(instruments);
    memory("after_drop");
    println!(
        "cleanup actual_histories=0 actual_profiles=0 total_seconds={:.6}",
        start.elapsed().as_secs_f64()
    );
}
