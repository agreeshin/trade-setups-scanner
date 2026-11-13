use serde::Deserialize;
use serde_json::Value;
use setups_detector::{PriceVolumeSpikeDetector, quality::ReplayQualityTracker, shared::{HistoryIdentity,HistoryLimits,HistoryPosition,SharedDetectorError,SharedHistory,SharedProfile}};
use setups_domain::{AlgorithmParams, DetectorBar, PushStatus, SkipReason, SpikeDropParams, SpikeParams};

const ID: HistoryIdentity=HistoryIdentity([7;32]);
const T0:i64=1_699_999_980;
fn bar(minute:usize,close:&str,volume:&str)->DetectorBar {DetectorBar{open_time_unix:T0+minute as i64*60,close:close.into(),quote_turnover:volume.into()}}
fn limits(minutes:usize)->HistoryLimits {
    let bytes=minutes*40;
    HistoryLimits{capacity_minutes:minutes,max_close_bytes:bytes,max_bytes:SharedHistory::required_bytes(minutes,bytes).unwrap()}
}
fn params()->SpikeParams {SpikeParams{baseline_long_days:1,baseline_short_days:1,window_minutes:5,price_growth_pct:"0".into(),volume_multiplier:"0.1".into(),required_recent_minutes:0,..SpikeParams::default()}}

#[derive(Deserialize)]
struct Fixture {scenarios:Vec<Scenario>}
#[derive(Deserialize)]
#[serde(rename_all="camelCase")]
struct Scenario {
    name:String,params:SpikeParams,last_setup_unix:Option<i64>,track_from_unix:i64,track_to_unix:i64,
    bars:Vec<(i64,String,String)>,pushes:Vec<Value>,quality:Value,intervals:Vec<Value>,
}

#[test]
fn shared_matches_every_reference_push_and_all_nine_golden_quality_reports() {
    let fixture:Fixture=serde_json::from_str(include_str!("../../../fixtures/parity.json")).unwrap();
    assert_eq!(fixture.scenarios.len(),9);
    for scenario in fixture.scenarios {
        let mut history=SharedHistory::new(ID,limits(scenario.params.baseline_long_days as usize*1440+1500)).unwrap();
        let mut shared=SharedProfile::new(AlgorithmParams::Spike(scenario.params.clone()),ID,HistoryPosition(0),scenario.last_setup_unix).unwrap();
        let mut reference=PriceVolumeSpikeDetector::new(scenario.params.clone(),scenario.last_setup_unix).unwrap();
        let mut quality=ReplayQualityTracker::new(scenario.track_from_unix,scenario.track_to_unix,u64::from(scenario.params.baseline_long_days)*1440);
        let mut interesting=Vec::new();
        for (time,close,volume) in scenario.bars {
            let bar=DetectorBar{open_time_unix:time,close,quote_turnover:volume};
            let position=history.push(&bar,time+60).unwrap();
            let result=shared.push(&history,position).unwrap();
            assert_eq!(result,reference.push(&bar).unwrap(),"{} at {time}",scenario.name);
            quality.observe(time,&result);
            if result.setup.is_some() || result.status==PushStatus::Evaluated || matches!(result.skip_reason,Some(SkipReason::RecentGap|SkipReason::PriceReferenceMissing|SkipReason::GapToleranceExceeded)) {
                interesting.push(serde_json::to_value(result).unwrap());
            }
        }
        quality.finish();
        assert_eq!(interesting,scenario.pushes,"{} pushes",scenario.name);
        assert_eq!(serde_json::to_value(quality.quality()).unwrap(),scenario.quality,"{} quality",scenario.name);
        let intervals:Vec<_>=quality.drain().iter().map(|v|serde_json::to_value(v).unwrap()).collect();
        assert_eq!(intervals,scenario.intervals,"{} intervals",scenario.name);
    }
}

#[test]
fn independent_profiles_match_reference_across_directions_windows_gaps_and_resets() {
    let mut history=SharedHistory::new(ID,limits(2*1440+1500)).unwrap();
    let mut profiles=Vec::new();
    let mut references=Vec::new();
    for i in 0..64 {
        let p=SpikeParams{
            baseline_long_days:if i%2==0 {1} else {2}, baseline_short_days:1,
            window_minutes:[1,5,23,60][i%4],price_growth_pct:["0","0.125","1.50","12"][i%4].into(),
            volume_multiplier:["0.000001","1.000","3","8.5"][i%4].into(),cooldown_minutes:[0,1,5,17][(i/2)%4],
            max_gap_percent:["0","0.1","1","100"][(i/4)%4].into(),
            max_consecutive_gap_minutes:[0,2,5,1440][(i/8)%4], required_recent_minutes:[0,1,10,60][(i/16)%4],
        };
        let params=if i%3==0 {AlgorithmParams::SpikeDrop(SpikeDropParams{
            price_drop_pct:p.price_growth_pct.clone(),window_minutes:p.window_minutes,baseline_long_days:p.baseline_long_days,
            baseline_short_days:p.baseline_short_days,volume_multiplier:p.volume_multiplier.clone(),cooldown_minutes:p.cooldown_minutes,
            max_gap_percent:p.max_gap_percent.clone(),max_consecutive_gap_minutes:p.max_consecutive_gap_minutes,required_recent_minutes:p.required_recent_minutes,
        })} else {AlgorithmParams::Spike(p)};
        let seed=(i%5==0).then_some(T0+1439*60);
        profiles.push(SharedProfile::new(params.clone(),ID,HistoryPosition(0),seed).unwrap());
        references.push(PriceVolumeSpikeDetector::with_params(params,seed).unwrap());
    }
    for minute in 0..7000 {
        if matches!(minute,1500|1501|2010|2012|2900..=2905|5000..=6440) {continue;}
        let close=match minute%11 {0=>"112.0000",1=>"88.00",2=>"100.00000001",3=>"0",_=>"100"};
        let turnover=match minute%13 {0=>"0",1=>"0.00000001",2=>"99999999.9",3=>"3.0000",_=>"10"};
        let bar=bar(minute,close,turnover);
        let position=history.push(&bar,bar.open_time_unix+60).unwrap();
        for (i,(profile,reference)) in profiles.iter_mut().zip(&mut references).enumerate() {
            assert_eq!(profile.push(&history,position).unwrap(),reference.push(&bar).unwrap(),"profile {i} minute {minute}");
        }
    }
}

#[test]
fn maximum_baseline_and_window_exact_boundary_preserve_original_close_and_clone_state() {
    let mut p=params();p.baseline_long_days=90;p.baseline_short_days=7;p.window_minutes=1440;p.cooldown_minutes=0;
    let mut history=SharedHistory::new(ID,limits(90*1440+2)).unwrap();
    let mut shared=SharedProfile::new(AlgorithmParams::Spike(p.clone()),ID,HistoryPosition(0),None).unwrap();
    let mut reference=PriceVolumeSpikeDetector::new(p,None).unwrap();
    for minute in 0..90*1440+3 {
        let bar=bar(minute,if minute==90*1440 {" 112.0000 "} else {"100"},if minute%101==0 {"3000.00001"} else {"10"});
        let position=history.push_retaining(&bar,bar.open_time_unix+60,shared.retention_anchor()).unwrap();
        let mut speculative=shared.clone();
        let result=speculative.push(&history,position).unwrap();
        assert_eq!(result,reference.push(&bar).unwrap(),"minute {minute}");
        assert_eq!(shared.last_position(),(minute>0).then(||HistoryPosition(minute as u64-1)));
        if minute==90*1440 {assert_eq!(result.setup.unwrap().close_price," 112.0000 ");}
        shared=speculative;
    }
}

#[test]
fn attach_to_ahead_history_iterates_retained_prefix_without_lookahead_or_history_clone() {
    let mut history=SharedHistory::new(ID,limits(4500)).unwrap();
    let mut active=SharedProfile::new(AlgorithmParams::Spike(params()),ID,HistoryPosition(0),None).unwrap();
    let mut reference=PriceVolumeSpikeDetector::new(params(),None).unwrap();
    for minute in 0..4000 {
        if matches!(minute,2510|2511|3600) {continue;}
        let bar=bar(minute,if minute%7==0 {"112.00"}else{"100"},"100.000001");
        let position=history.push(&bar,bar.open_time_unix+60).unwrap();
        assert_eq!(active.push(&history,position).unwrap(),reference.push(&bar).unwrap());
    }
    let start=HistoryPosition(2000);
    let mut attached=SharedProfile::new(AlgorithmParams::Spike(params()),ID,start,Some(T0+2010*60)).unwrap();
    let mut fresh=PriceVolumeSpikeDetector::new(params(),Some(T0+2010*60)).unwrap();
    for frame in history.frames_from(start).unwrap() {
        let raw=DetectorBar{open_time_unix:frame.open_time_unix(),close:frame.original_close(),quote_turnover:"100.000001".into()};
        assert_eq!(attached.push(&history,frame.position()).unwrap(),fresh.push(&raw).unwrap());
    }
    assert_eq!(history.latest_position(),Some(HistoryPosition(3999)));
    assert_eq!(history.end_position(),HistoryPosition(4000));
    assert_eq!(history.position_for_open(T0+3000*60).unwrap(),HistoryPosition(3000));
    assert!(history.frame(HistoryPosition(2510)).unwrap().is_none());
    assert_eq!(history.frame(HistoryPosition(3999)).unwrap().unwrap().quote_turnover(),setups_domain::decimal::Decimal::parse("100.000001").unwrap());
    let mut wrong=SharedProfile::new(AlgorithmParams::Spike(params()),HistoryIdentity([8;32]),start,None).unwrap();
    assert_eq!(wrong.push(&history,HistoryPosition(2000)).unwrap_err(),SharedDetectorError::WrongHistory);
}

#[test]
fn eviction_byte_caps_and_anchor_headroom_fail_explicitly_without_mutating_state() {
    let mut history=SharedHistory::new(ID,limits(1442)).unwrap();
    let mut profile=SharedProfile::new(AlgorithmParams::Spike(params()),ID,HistoryPosition(0),None).unwrap();
    for minute in 0..1440 {
        let bar=bar(minute,"100","10");
        let pos=history.push(&bar,bar.open_time_unix+60).unwrap();
        profile.push(&history,pos).unwrap();
    }
    let held=profile.clone();
    for minute in 1440..1442 {
        let bar=bar(minute,"100","10");
        let pos=history.push_retaining(&bar,bar.open_time_unix+60,held.retention_anchor()).unwrap();
        profile.push(&history,pos).unwrap();
    }
    let next=bar(1442,"100","10");
    assert!(matches!(history.push_retaining(&next,next.open_time_unix+60,held.retention_anchor()),Err(SharedDetectorError::Evicted{..})));
    assert_eq!(history.end_position(),HistoryPosition(1442));
    let pos=history.push_retaining(&next,next.open_time_unix+60,profile.retention_anchor()).unwrap();
    profile.push(&history,pos).unwrap();
    assert!(matches!(history.frame(HistoryPosition(0)),Err(SharedDetectorError::Evicted{..})));
    let mut exhausted=held.clone();
    assert!(matches!(exhausted.push(&history,HistoryPosition(1440)),Err(SharedDetectorError::Evicted{..})));
    assert_eq!(exhausted.last_position(),held.last_position());
    let bytes=SharedHistory::required_bytes(2,6).unwrap();
    assert!(matches!(SharedHistory::new(ID,HistoryLimits{capacity_minutes:2,max_close_bytes:6,max_bytes:bytes-1}),Err(SharedDetectorError::Capacity{..})));
    let mut tiny=SharedHistory::new(ID,HistoryLimits{capacity_minutes:2,max_close_bytes:6,max_bytes:bytes}).unwrap();
    tiny.push(&bar(0,"100","10"),T0+60).unwrap();
    let before=tiny.retained_bytes();
    assert!(matches!(tiny.push(&bar(1,"1000","10"),T0+120),Err(SharedDetectorError::Capacity{..})));
    assert_eq!(tiny.end_position(),HistoryPosition(1));
    assert_eq!(tiny.frame(HistoryPosition(0)).unwrap().unwrap().original_close(),"100");
    tiny.push(&bar(1,"100","10"),T0+120).unwrap();
    assert_eq!(tiny.retained_bytes(),before);
}

#[test]
fn input_and_cursor_errors_do_not_poison_shared_history_or_profile() {
    let mut history=SharedHistory::new(ID,limits(1442)).unwrap();
    let mut profile=SharedProfile::new(AlgorithmParams::Spike(params()),ID,HistoryPosition(0),None).unwrap();
    let mut invalid=bar(0,"not a decimal","10");
    assert!(history.push(&invalid,T0+60).is_err());assert!(history.is_empty());
    invalid=bar(0,"100","10");
    assert!(history.push(&invalid,T0+59).is_err());
    let first=history.push(&bar(0,"100","0"),T0+60).unwrap();
    invalid.open_time_unix=T0+61;assert!(history.push(&invalid,T0+121).is_err());
    assert_eq!(history.push(&bar(0,"100","0"),T0+60).unwrap(),first);
    let second=history.push(&bar(1,"100","10"),T0+120).unwrap();
    assert!(matches!(profile.push(&history,second),Err(SharedDetectorError::Sequence{..})));
    assert_eq!(profile.last_position(),None);
    profile.push(&history,first).unwrap();
    assert!(profile.push(&history,first).is_err());
    profile.push(&history,second).unwrap();
    assert!(matches!(profile.push(&history,HistoryPosition(100)),Err(SharedDetectorError::Future(_))));
}

#[test]
fn consumed_hole_repair_or_conflict_never_mutates_future_departure_arithmetic() {
    let mut history=SharedHistory::new(ID,limits(1442)).unwrap();
    let mut shared=SharedProfile::new(AlgorithmParams::Spike(params()),ID,HistoryPosition(0),None).unwrap();
    let mut reference=PriceVolumeSpikeDetector::new(params(),None).unwrap();
    for minute in 0..3000 {
        if minute==1450 {continue;}
        let input=bar(minute," 100.000 ",if minute%7==0 {"999.00001"}else{"0"});
        let pos=history.push_retaining(&input,input.open_time_unix+60,shared.retention_anchor()).unwrap();
        assert_eq!(shared.push(&history,pos).unwrap(),reference.push(&input).unwrap(),"minute {minute}");
        if minute==1460 {
            let end=history.end_position();
            assert!(matches!(history.push(&bar(1450,"100","99999"),input.open_time_unix+60),Err(SharedDetectorError::Invalid(_))));
            assert!(matches!(history.push(&bar(1460,"100","123"),input.open_time_unix+60),Err(SharedDetectorError::Invalid(_))));
            assert_eq!(history.push(&input,input.open_time_unix+60).unwrap(),pos);
            assert_eq!(history.end_position(),end);
            assert!(history.frame(HistoryPosition(1450)).unwrap().is_none());
        }
    }
    // By now the consumed missing minute has departed the baseline. If the
    // rejected repair had inserted turnover, sums/PushResult above would differ.
    assert!(matches!(history.push(&bar(0,"100","10"),T0+3001*60),Err(SharedDetectorError::Evicted{..})));
}

#[test]
fn repeated_eviction_releases_original_strings_and_profile_clones_do_not_own_history() {
    for _ in 0..3 {
        let mut history=SharedHistory::new(ID,limits(1442)).unwrap();
        let mut profile=SharedProfile::new(AlgorithmParams::Spike(params()),ID,HistoryPosition(0),None).unwrap();
        let bytes=history.retained_bytes();
        let profile_bytes=profile.retained_bytes();
        for minute in 0..6000 {
            let input=bar(minute,if minute%2==0 {"100.000000000000000001"} else {"100"},"10.00");
            let pos=history.push_retaining(&input,input.open_time_unix+60,profile.retention_anchor()).unwrap();
            profile.push(&history,pos).unwrap();
            assert!(history.len()<=1442);assert_eq!(history.retained_bytes(),bytes);assert_eq!(profile.retained_bytes(),profile_bytes);
        }
        assert_eq!(history.oldest_position(),HistoryPosition(4558));
        let clone=profile.clone();
        drop(history); // no borrow/Arc to history remains in either profile
        assert_eq!(profile.last_position(),clone.last_position());
        assert!(profile.retained_bytes()<2048);
        drop(profile);drop(clone);
    }
}
