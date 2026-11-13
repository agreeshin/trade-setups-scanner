//! Opt-in cross-language contract check against a local market-data fixture.
//! Fixture: one instrument with bars at FROM and FROM+120 (hole at FROM+60),
//! first close exactly 100.123456789012345678; no exchange downloads are requested.
//! Optional TAIL_SMOKE_REPAIR_SCRIPT is a trusted absolute .mts fixture script:
//! `node --import tsx <script> repair` must fill FROM+60 through insertCandles.
use setups_mdclient::{MarketDataClient, TailBatchQuery, TailReadGuardQuery, TailReadGuardStatus};
use std::{path::Path, time::Duration};

#[tokio::test]
#[ignore = "requires an explicitly prepared local market-data API fixture"]
async fn tail_contract_matches_running_market_service() {
    let url = std::env::var("TAIL_SMOKE_URL").expect("TAIL_SMOKE_URL");
    let parsed = reqwest::Url::parse(&url).unwrap();
    assert!(matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")));
    let key = std::env::var("TAIL_SMOKE_KEY").expect("TAIL_SMOKE_KEY");
    let instrument_id = std::env::var("TAIL_SMOKE_INSTRUMENT")
        .expect("TAIL_SMOKE_INSTRUMENT")
        .parse::<i64>()
        .unwrap();
    let from_unix = std::env::var("TAIL_SMOKE_FROM")
        .expect("TAIL_SMOKE_FROM")
        .parse::<i64>()
        .unwrap();
    let query = TailBatchQuery {
        instrument_ids: vec![instrument_id, i64::from(i32::MAX)],
        from_unix,
        to_unix: from_unix + 180,
    };
    let client = MarketDataClient::new(&url, &key);
    let result = client.tail_batch(&query).await.unwrap();
    let known = &result.instruments[0];
    assert_eq!(known.status, "known");
    assert_eq!(known.candles.len(), 2);
    assert_eq!(known.candles[0].candle.close, "100.123456789012345678");
    assert_eq!(known.latest_stored_open_time_unix, Some(from_unix + 120));
    assert_eq!(known.contiguous_through_open_time_unix, Some(from_unix));
    assert_eq!(known.missing_ranges.len(), 1);
    assert_eq!(known.missing_ranges[0].from_unix, from_unix + 60);
    assert_eq!(known.missing_ranges[0].to_unix, from_unix + 120);
    assert_eq!(result.instruments[1].status, "not_found");
    assert!(result.instruments[1].candles.is_empty());
    assert!(MarketDataClient::new(&url, "invalid-fixture-key").tail_batch(&query).await.is_err());

    let capture = TailReadGuardQuery {
        instrument_id,
        from_unix,
        to_unix: from_unix + 180,
        previous: None,
    };
    let unknown_capture = TailReadGuardQuery { instrument_id: i64::from(i32::MAX), ..capture.clone() };
    let captured = client.tail_batch_guarded(&query, &[capture.clone(), unknown_capture]).await.unwrap();
    let captured_guard = captured.instruments[0].read_guard.as_ref().expect("capture guard");
    assert_eq!(captured_guard.status, TailReadGuardStatus::Valid);
    assert!(capture.validated_token(Some(captured_guard)).is_none(), "capture is not historical proof");
    let token = captured_guard.token.clone().expect("captured source token");
    token.validate().unwrap();
    let unknown = captured.instruments[1].read_guard.as_ref().expect("unknown instrument guard");
    assert_eq!(unknown.status, TailReadGuardStatus::Unknown);
    assert!(unknown.token.is_none());

    let validate_original = TailReadGuardQuery { previous: Some(token.clone()), ..capture.clone() };
    let unchanged = client.tail_batch_guarded(&query, std::slice::from_ref(&validate_original)).await.unwrap();
    let unchanged_guard = unchanged.instruments[0].read_guard.as_ref().expect("unchanged guard");
    assert_eq!(unchanged_guard.status, TailReadGuardStatus::Valid);
    assert_eq!(validate_original.validated_token(Some(unchanged_guard)), Some(token.clone()));
    println!("actual TS -> Rust guard capture/unchanged validated at revision {}", token.revision);

    if let Ok(script) = std::env::var("TAIL_SMOKE_REPAIR_SCRIPT") {
        assert!(Path::new(&script).is_absolute() && Path::new(&script).is_file(), "trusted absolute repair script required");
        let mut repair = tokio::process::Command::new("node");
        repair.args(["--import", "tsx", &script, "repair"]).kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(10), repair.output())
            .await.expect("canonical fixture repair timed out").expect("launch canonical repair fixture");
        assert!(output.status.success(), "canonical fixture repair failed: {}", String::from_utf8_lossy(&output.stderr));

        let repaired = client.tail_batch_guarded(&query, std::slice::from_ref(&validate_original)).await.unwrap();
        let known = &repaired.instruments[0];
        let changed = known.read_guard.as_ref().expect("repaired source guard");
        assert_eq!(changed.status, TailReadGuardStatus::Invalid);
        assert!(validate_original.validated_token(Some(changed)).is_none());
        let next = changed.token.as_ref().expect("current repair token");
        assert_eq!(next.epoch, token.epoch);
        assert!(next.revision.parse::<i64>().unwrap() > token.revision.parse::<i64>().unwrap());
        assert_eq!(known.latest_stored_open_time_unix, Some(from_unix + 120));
        assert_eq!(known.candles.len(), 3);
        assert!(known.missing_ranges.is_empty());
        assert_eq!(known.contiguous_through_open_time_unix, Some(from_unix + 120));
        assert_eq!(known.candles[1].candle.open_time_unix, from_unix + 60);
        assert_eq!(known.candles[1].candle.close, "101.000000000000000001");
        println!("actual canonical insert repaired consumed hole; Rust rejected old guard {} -> {} with unchanged latest", token.revision, next.revision);
    }
}
