//! Explicit management operations. Candle and tail readers never call these.
use super::*;

const MANAGEMENT_BYTES: usize = 512 * 1024;
const MANAGEMENT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryDemand {
    pub instrument_id: i64,
    pub from_unix: i64,
    pub to_unix: i64,
}
impl HistoryDemand {
    pub fn validate(self) -> Result<(), MdClientError> {
        if !(1..=i64::from(i32::MAX)).contains(&self.instrument_id)
            || self.from_unix < 0 || self.from_unix % 60 != 0 || self.to_unix % 60 != 0
            || self.to_unix <= self.from_unix || self.to_unix - self.from_unix > 1830 * 86400 {
            return Err(MdClientError::Decode("invalid selected-live preparation range".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionAdmission { pub instrument_id: i64, pub enabled: bool }
#[derive(Deserialize)]
struct CollectionWrite { updated: usize, enabled: bool }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetPage {
    targets: Vec<CollectionAdmission>,
    #[serde(default)]
    only_missing_supported: bool,
    #[serde(deserialize_with = "required_cursor")]
    next_after_instrument_id: Option<i64>,
}
#[derive(Default)]
struct TargetScan { after: i64, pages: usize, found: Option<CollectionAdmission> }
impl TargetScan {
    fn accept(&mut self, page: TargetPage, id: i64) -> Result<bool, MdClientError> {
        if !page.only_missing_supported {return Err(MdClientError::Decode("conditional_collection_unsupported".into()));}
        self.pages += 1;
        if self.pages > 16 || page.targets.len() > 1000 {
            return Err(MdClientError::Decode("collection target scan exceeds budget".into()));
        }
        let mut last = self.after;
        for row in page.targets {
            if row.instrument_id <= last || row.instrument_id > i64::from(i32::MAX) {
                return Err(MdClientError::Decode("unordered collection target page".into()));
            }
            last = row.instrument_id;
            if row.instrument_id == id { self.found = Some(row); }
        }
        if let Some(next) = page.next_after_instrument_id {
            if next != last || next <= self.after { return Err(MdClientError::Decode("collection target cursor did not advance".into())); }
            self.after = next;
            Ok(false)
        } else { Ok(true) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackfillStatus { Queued, Running, Done, CancelRequested, Cancelled, Failed }
impl BackfillStatus {
    pub fn active(self) -> bool { matches!(self, Self::Queued | Self::Running | Self::CancelRequested) }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillJob {
    pub id: i64,
    pub instrument_id: i64,
    pub from_unix: i64,
    pub to_unix: i64,
    pub status: BackfillStatus,
}
impl BackfillJob {
    pub fn covers(&self, demand: HistoryDemand) -> bool {
        self.instrument_id == demand.instrument_id && self.from_unix <= demand.from_unix && self.to_unix >= demand.to_unix
    }
    fn validate(&self, id: i64) -> Result<(), MdClientError> {
        HistoryDemand { instrument_id: self.instrument_id, from_unix: self.from_unix, to_unix: self.to_unix }.validate()?;
        if self.id <= 0 || self.instrument_id != id { return Err(MdClientError::Decode("foreign preparation job".into())); }
        Ok(())
    }
}
#[derive(Debug, Clone, Deserialize)]
pub struct BackfillAdmission { pub job: BackfillJob }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillHistory {
    pub jobs: Vec<BackfillJob>,
    #[serde(deserialize_with = "required_cursor")]
    pub next_before_id: Option<i64>,
}
fn required_cursor<'de,D:serde::Deserializer<'de>>(deserializer:D)->Result<Option<i64>,D::Error> {
    Option::<i64>::deserialize(deserializer)
}

impl MarketDataClient {
    fn management(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, format!("{}/v1{path}", self.base_url.trim_end_matches('/')))
            .header("x-api-key", &self.api_key).timeout(MANAGEMENT_TIMEOUT)
    }
    async fn management_read<T: serde::de::DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T, MdClientError> {
        let response = request.send().await.map_err(|_| MdClientError::Network("selected-live preparation transport failed".into()))?;
        Self::decode_limited(response, MANAGEMENT_BYTES).await
    }
    fn conditional_collection_request(&self, instrument_id: i64) -> reqwest::RequestBuilder {
        self.management(reqwest::Method::PUT, "/collection-targets")
            .json(&serde_json::json!({"instrumentIds": [instrument_id], "enabled": true, "onlyMissing": true}))
    }
    /// Requires producer's atomic onlyMissing extension. NEVER fall back to an
    /// unconditional upsert: a read/PUT sequence can overwrite admin disable.
    pub async fn ensure_collection_if_missing(&self, instrument_id: i64) -> Result<CollectionAdmission, MdClientError> {
        // Legacy Fastify may STRIP unknown onlyMissing and execute an ordinary
        // upsert. Capability must be proven by a read BEFORE any write. Existing
        // rows need no insertion at all; in particular never PUT over false.
        if let Some(existing)=self.collection_state(instrument_id).await? {return Ok(existing);}
        let response: CollectionWrite = self.management_read(self.conditional_collection_request(instrument_id)).await?;
        if response.updated > 1 || !response.enabled { return Err(MdClientError::Decode("invalid conditional collection response".into())); }
        self.collection_state(instrument_id).await?.ok_or_else(||MdClientError::Decode("conditional collection target absent after complete scan".into()))
    }
    pub async fn collection_state(&self, instrument_id:i64)->Result<Option<CollectionAdmission>,MdClientError> {
        // The PUT echoes the requested enabled=true, NOT the actual row. Read
        // all bounded pages; false may be an existing or concurrent admin choice.
        let mut scan = TargetScan::default();
        loop {
            if scan.pages>=16 {return Err(MdClientError::Decode("collection target scan exceeds page budget".into()));}
            let mut request = self.management(reqwest::Method::GET, "/collection-targets").query(&[("limit",1000)]);
            if scan.after > 0 { request = request.query(&[("afterInstrumentId",scan.after)]); }
            let page: TargetPage = self.management_read(request).await?;
            if scan.accept(page,instrument_id)? {break;}
        }
        Ok(scan.found)
    }
    pub async fn preparation_coverage(&self, demand: HistoryDemand) -> Result<CoverageRangeDto, MdClientError> {
        demand.validate()?;
        let response: CoverageRangeDto = self.management_read(self.management(reqwest::Method::GET,
            &format!("/instruments/{}/coverage-range", demand.instrument_id)).query(&[
                ("fromUnix", demand.from_unix), ("toUnix", demand.to_unix), ("splitUnix", demand.from_unix), ("offset", 0), ("limit", 1),
            ])).await?;
        if response.instrument_id != demand.instrument_id || response.from_unix != demand.from_unix || response.to_unix != demand.to_unix
            || response.missing_minutes < 0 || response.missing_minutes > (demand.to_unix - demand.from_unix) / 60 {
            return Err(MdClientError::Decode("invalid preparation coverage identity/count".into()));
        }
        Ok(response)
    }
    pub async fn preparation_jobs(&self, instrument_id: i64) -> Result<BackfillHistory, MdClientError> {
        let response: BackfillHistory = self.management_read(self.management(reqwest::Method::GET, "/backfill-jobs")
            .query(&[("instrumentId", instrument_id), ("limit", 200)])).await?;
        if response.jobs.len() > 200 { return Err(MdClientError::Decode("preparation job page exceeds budget".into())); }
        for job in &response.jobs { job.validate(instrument_id)?; }
        Ok(response)
    }
    fn backfill_request(&self, demand: HistoryDemand) -> reqwest::RequestBuilder {
        // Public HTTP range is [fromUnix,toUnix). Only the producer converts
        // this exclusive end to its internal inclusive toTs = toUnix - 60.
        self.management(reqwest::Method::POST, "/backfill-jobs").json(&demand)
    }
    pub async fn admit_preparation(&self, demand: HistoryDemand) -> Result<BackfillAdmission, MdClientError> {
        demand.validate()?;
        let response: BackfillAdmission = self.management_read(self.backfill_request(demand)).await?;
        response.job.validate(demand.instrument_id)?;
        if !response.job.covers(demand) { return Err(MdClientError::Decode("admitted job does not cover preparation".into())); }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preparation_wire_is_explicit_conditional_and_keeps_exclusive_end() {
        let client = MarketDataClient::new("http://fixture.invalid", "fixture-key");
        let demand = HistoryDemand { instrument_id: 820, from_unix: 120, to_unix: 240 };
        let request = client.backfill_request(demand).build().unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(request.url().path(), "/v1/backfill-jobs");
        assert_eq!(serde_json::from_slice::<Value>(request.body().unwrap().as_bytes().unwrap()).unwrap(),
            serde_json::json!({"instrumentId":820,"fromUnix":120,"toUnix":240}));
        let request = client.conditional_collection_request(820).build().unwrap();
        assert_eq!(request.method(), reqwest::Method::PUT);
        assert_eq!(request.url().path(), "/v1/collection-targets");
        assert_eq!(serde_json::from_slice::<Value>(request.body().unwrap().as_bytes().unwrap()).unwrap(),
            serde_json::json!({"instrumentIds":[820],"enabled":true,"onlyMissing":true}));
    }
    fn page(value:Value)->TargetPage {serde_json::from_value(value).unwrap()}
    #[test]
    fn actual_disabled_target_on_later_page_is_not_put_echo_or_missing_first_page() {
        let mut scan=TargetScan::default();
        assert!(!scan.accept(page(serde_json::json!({"onlyMissingSupported":true,"targets":[{"instrumentId":500,"enabled":true}],"nextAfterInstrumentId":500})),820).unwrap());
        assert!(scan.found.is_none());
        assert!(scan.accept(page(serde_json::json!({"onlyMissingSupported":true,"targets":[{"instrumentId":820,"enabled":false}],"nextAfterInstrumentId":null})),820).unwrap());
        assert!(!scan.found.unwrap().enabled);
    }
    #[test]
    fn paginated_target_and_job_contracts_fail_closed_on_bad_cursors_and_budgets() {
        assert!(serde_json::from_value::<TargetPage>(serde_json::json!({"targets":[]})).is_err());
        assert!(serde_json::from_value::<BackfillHistory>(serde_json::json!({"jobs":[]})).is_err());
        let mut scan=TargetScan::default();
        assert!(scan.accept(page(serde_json::json!({"onlyMissingSupported":true,"targets":[],"nextAfterInstrumentId":1})),820).is_err());
        let mut scan=TargetScan::default();
        for id in 1..=16 {assert!(!scan.accept(TargetPage {only_missing_supported:true,targets:vec![CollectionAdmission {instrument_id:id,enabled:true}],next_after_instrument_id:Some(id)},820).unwrap());}
        assert!(scan.accept(TargetPage {only_missing_supported:true,targets:vec![],next_after_instrument_id:None},820).is_err());
        let history:BackfillHistory=serde_json::from_value(serde_json::json!({"jobs":[],"nextBeforeId":null})).unwrap();
        assert!(history.next_before_id.is_none());
    }
    #[test]
    fn legacy_collection_response_without_capability_fails_preflight() {
        let legacy=page(serde_json::json!({"targets":[],"nextAfterInstrumentId":null}));
        assert!(!legacy.only_missing_supported);
        let error=TargetScan::default().accept(legacy,820).unwrap_err();
        assert!(matches!(error,MdClientError::Decode(code) if code=="conditional_collection_unsupported"));
    }
}
