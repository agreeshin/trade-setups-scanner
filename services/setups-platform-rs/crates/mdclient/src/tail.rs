//! Bounded saved-only multi-instrument live tail client.
use super::*;
use std::collections::BTreeSet;

pub const TAIL_MAX_INSTRUMENTS: usize = 200;
pub const TAIL_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all="camelCase")]
pub struct TailBatchQuery {pub instrument_ids:Vec<i64>,pub from_unix:i64,pub to_unix:i64}

/// Source-compatible optional extension: `tail_batch_guarded` serializes these
/// as readGuards alongside the existing TailBatchQuery fields.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="camelCase")]
pub struct TailReadToken {pub epoch:String,pub revision:String}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="camelCase")]
pub struct TailReadGuardQuery {
    pub instrument_id:i64,
    pub from_unix:i64,
    pub to_unix:i64,
    pub previous:Option<TailReadToken>,
}
#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="lowercase")]
pub enum TailReadGuardStatus {Valid,Invalid,Unknown}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="camelCase")]
pub struct TailReadGuard {pub status:TailReadGuardStatus,pub token:Option<TailReadToken>}
impl TailReadToken {
    pub fn validate(&self)->Result<(),MdClientError> {
        if self.epoch.is_empty() || self.epoch.len()>128 || self.epoch.chars().any(char::is_control)
            || self.revision.is_empty() || self.revision.len()>19 || !self.revision.bytes().all(|b|b.is_ascii_digit())
            || self.revision.parse::<i64>().is_err() {return Err(invalid("malformed repair token"));}
        Ok(())
    }
}
impl TailReadGuardQuery {
    pub fn validated_token(&self,guard:Option<&TailReadGuard>)->Option<TailReadToken> {
        let previous=self.previous.as_ref()?;
        let guard=guard?;
        guard.validates(previous).then(||guard.token.clone()).flatten()
    }
}
impl TailReadGuard {
    /// A previous:null request is capture only. Callers need an actual previous
    /// token to invoke this proof check; status=valid alone is never sufficient.
    pub fn validates(&self,previous:&TailReadToken)->bool {
        self.status==TailReadGuardStatus::Valid && previous.validate().is_ok()
            && self.token.as_ref().is_some_and(|token|token.validate().is_ok() && token.epoch==previous.epoch
                && token.revision.parse::<i64>().unwrap()>=previous.revision.parse::<i64>().unwrap())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct TailBatchResponse {pub checked_at:String,pub instruments:Vec<TailInstrument>}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct TailInstrument {
    pub instrument_id:i64,
    pub status:String,
    pub latest_stored_open_time_unix:Option<i64>,
    pub contiguous_through_open_time_unix:Option<i64>,
    pub candles:Vec<TailCandle>,
    pub missing_ranges:Vec<TailMissingRange>,
    #[serde(default)]
    pub read_guard:Option<TailReadGuard>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct TailCandle {
    pub instrument_id:i64,
    #[serde(flatten)] pub candle:CandleDto,
}
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct TailMissingRange {pub from_unix:i64,pub to_unix:i64}

fn invalid(message:&str)->MdClientError {MdClientError::Decode(format!("invalid tail batch: {message}"))}
impl TailBatchQuery {
    pub fn validate(&self)->Result<(),MdClientError> {
        if self.instrument_ids.is_empty() || self.instrument_ids.len()>TAIL_MAX_INSTRUMENTS
            || self.instrument_ids.iter().any(|id|!(1..=i64::from(i32::MAX)).contains(id))
            || self.instrument_ids.iter().collect::<BTreeSet<_>>().len()!=self.instrument_ids.len()
            || self.from_unix<0 || self.from_unix%60!=0 || self.to_unix%60!=0
            || self.to_unix<=self.from_unix || self.to_unix-self.from_unix>1800 {
            return Err(invalid("request ids/range exceed contract"));
        }
        Ok(())
    }
}
impl TailBatchResponse {
    fn validate(&self,query:&TailBatchQuery,guards:Option<&[TailReadGuardQuery]>)->Result<(),MdClientError> {
        if self.checked_at.len()>64 || self.checked_at.is_empty() || self.instruments.len()!=query.instrument_ids.len() {
            return Err(invalid("missing instrument result or timestamp"));
        }
        let mut total=0;
        for (item,expected) in self.instruments.iter().zip(&query.instrument_ids) {
            if item.instrument_id!=*expected || !matches!(item.status.as_str(),"known"|"not_found")
                || item.candles.len()>30 || item.missing_ranges.len()>30 {
                return Err(invalid("instrument identity/status/item budget"));
            }
            let mut cursor=query.from_unix;
            let mut missing=Vec::new();
            let mut contiguous=None;
            for row in &item.candles {
                let bar=&row.candle;
                if row.instrument_id!=*expected || bar.open_time_unix<cursor || bar.open_time_unix>=query.to_unix
                    || bar.open_time_unix%60!=0
                    || [&bar.open,&bar.high,&bar.low,&bar.close,&bar.volume,&bar.quote_turnover].iter().any(|value|value.is_empty() || value.len()>128) {
                    return Err(invalid("unordered, open, oversized or foreign candle"));
                }
                if cursor<bar.open_time_unix {missing.push(TailMissingRange {from_unix:cursor,to_unix:bar.open_time_unix});}
                if missing.is_empty() {contiguous=Some(bar.open_time_unix);}
                cursor=bar.open_time_unix+60;
            }
            if cursor<query.to_unix {missing.push(TailMissingRange {from_unix:cursor,to_unix:query.to_unix});}
            if missing!=item.missing_ranges || contiguous!=item.contiguous_through_open_time_unix
                || (item.status=="not_found" && (!item.candles.is_empty() || item.latest_stored_open_time_unix.is_some())) {
                return Err(invalid("coverage differs from actual returned candles"));
            }
            if let Some(guard)=&item.read_guard {
                if let Some(token)=&guard.token {token.validate()?;}
                if guard.status==TailReadGuardStatus::Valid && guard.token.is_none() {
                    return Err(invalid("valid read guard has no token"));
                }
            }
            if let Some(requested)=guards.and_then(|guards|guards.iter().find(|g|g.instrument_id==item.instrument_id)) {
                let guard=item.read_guard.as_ref().ok_or_else(||invalid("requested read guard missing from response"))?;
                if let Some(previous)=&requested.previous
                    && guard.status==TailReadGuardStatus::Valid && !guard.validates(previous) {
                    return Err(invalid("valid read guard regressed or changed epoch"));
                }
            }
            total+=item.candles.len();
        }
        if total>6000 {return Err(invalid("total candle budget"));}
        Ok(())
    }
}
impl MarketDataClient {
    pub async fn tail_batch(&self,query:&TailBatchQuery)->Result<TailBatchResponse,MdClientError> {
        self.tail_batch_request(query,None).await
    }
    pub async fn tail_batch_guarded(&self,query:&TailBatchQuery,guards:&[TailReadGuardQuery])->Result<TailBatchResponse,MdClientError> {
        if guards.len()>TAIL_MAX_INSTRUMENTS || guards.iter().map(|g|g.instrument_id).collect::<BTreeSet<_>>().len()!=guards.len() {
            return Err(invalid("read guard count/identity budget"));
        }
        for guard in guards {
            if !query.instrument_ids.contains(&guard.instrument_id) || guard.from_unix<0
                || guard.from_unix%60!=0 || guard.to_unix%60!=0 || guard.from_unix>guard.to_unix {
                return Err(invalid("read guard identity/range"));
            }
            if let Some(token)=&guard.previous {token.validate()?;}
        }
        self.tail_batch_request(query,Some(guards)).await
    }
    async fn tail_batch_request(&self,query:&TailBatchQuery,guards:Option<&[TailReadGuardQuery]>)->Result<TailBatchResponse,MdClientError> {
        #[derive(Serialize)]
        #[serde(rename_all="camelCase")]
        struct Request<'a> {
            #[serde(flatten)] query:&'a TailBatchQuery,
            #[serde(skip_serializing_if="Option::is_none")] read_guards:Option<&'a [TailReadGuardQuery]>,
        }
        query.validate()?;
        let response=self.http.post(format!("{}/v1/candles/tail-batch",self.base_url))
            .header("x-api-key",&self.api_key).timeout(Duration::from_secs(5)).json(&Request {query,read_guards:guards})
            .send().await.map_err(|e|MdClientError::Network(e.to_string()))?;
        let body:TailBatchResponse=Self::decode_limited(response,TAIL_MAX_BYTES).await?;
        body.validate(query,guards)?;
        Ok(body)
    }
    pub(crate) async fn decode_limited<T:serde::de::DeserializeOwned>(mut response:reqwest::Response,max_bytes:usize)->Result<T,MdClientError> {
        let status=response.status();
        let limit=if status.is_success(){max_bytes}else{4096};
        if response.content_length().is_some_and(|bytes|bytes>limit as u64) {return Err(invalid("HTTP response exceeds byte budget"));}
        let mut bytes=Vec::new();
        while let Some(chunk)=response.chunk().await.map_err(|e|MdClientError::Network(e.to_string()))? {
            if chunk.len()>limit.saturating_sub(bytes.len()) {return Err(invalid("HTTP response exceeds byte budget"));}
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(MdClientError::Http {status:status.as_u16(),body:String::from_utf8_lossy(&bytes).chars().take(200).collect()});
        }
        serde_json::from_slice(&bytes).map_err(|e|MdClientError::Decode(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn query()->TailBatchQuery {TailBatchQuery {instrument_ids:vec![1],from_unix:120,to_unix:180}}
    fn guard()->TailReadGuardQuery {TailReadGuardQuery {instrument_id:1,from_unix:0,to_unix:120,
        previous:Some(TailReadToken {epoch:"fixture-epoch".into(),revision:"9007199254740993".into()})}}
    fn response(metadata:Option<Value>)->TailBatchResponse {
        let mut item=json!({"instrumentId":1,"status":"known","latestStoredOpenTimeUnix":null,
            "contiguousThroughOpenTimeUnix":null,"candles":[],"missingRanges":[{"fromUnix":120,"toUnix":180}]});
        if let Some(metadata)=metadata {item["readGuard"]=metadata;}
        serde_json::from_value(json!({"checkedAt":"2026-11-13T00:00:00Z","instruments":[item]})).unwrap()
    }
    #[test]
    fn repair_tokens_preserve_bigints_and_reject_numbers_or_malformed_strings() {
        let token=guard().previous.unwrap();token.validate().unwrap();
        assert_eq!(serde_json::to_value(&token).unwrap()["revision"],"9007199254740993");
        assert!(serde_json::from_value::<TailReadToken>(json!({"epoch":"a","revision":9007199254740993u64})).is_err());
        for revision in ["-1","1.5","NaN","9223372036854775808",""] {
            assert!(TailReadToken {epoch:"a".into(),revision:revision.into()}.validate().is_err());
        }
    }
    #[test]
    fn missing_requested_guard_fails_closed_but_unguarded_legacy_call_still_works() {
        let body=response(None);body.validate(&query(),None).unwrap();
        assert!(body.validate(&query(),Some(&[guard()])).is_err());
        assert!(guard().validated_token(body.instruments[0].read_guard.as_ref()).is_none());
    }
    #[test]
    fn capture_is_not_proof_and_invalid_unknown_or_changed_epochs_cannot_validate() {
        let body=response(Some(json!({"status":"valid","token":{"epoch":"fixture-epoch","revision":"9007199254740994"}})));
        body.validate(&query(),Some(&[guard()])).unwrap();
        let metadata=body.instruments[0].read_guard.as_ref();
        assert_eq!(guard().validated_token(metadata).unwrap().revision,"9007199254740994");
        let mut capture=guard();capture.previous=None;
        assert!(capture.validated_token(metadata).is_none());
        for status in ["invalid","unknown"] {
            let body=response(Some(json!({"status":status,"token":{"epoch":"fixture-epoch","revision":"9007199254740994"}})));
            body.validate(&query(),Some(&[guard()])).unwrap();
            assert!(guard().validated_token(body.instruments[0].read_guard.as_ref()).is_none());
        }
        for metadata in [json!({"status":"valid","token":null}),
            json!({"status":"valid","token":{"epoch":"other","revision":"9007199254740994"}}),
            json!({"status":"valid","token":{"epoch":"fixture-epoch","revision":"9007199254740992"}})] {
            assert!(response(Some(metadata)).validate(&query(),Some(&[guard()])).is_err());
        }
    }
}
