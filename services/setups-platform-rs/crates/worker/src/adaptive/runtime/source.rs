//! Repair-journal validation surrounds legacy history reads. Capture is NOT
//! proof, and no fetched candle reaches detector/effect preparation until the
//! causal interval is validated against the captured/last-observed token.
use super::*;
use setups_mdclient::{MdClientError,TailReadGuardQuery,TailReadToken};

pub(super) struct ValidatedPage {
    pub bars:Vec<DetectorBar>,
    pub complete:bool,
    pub token:TailReadToken,
}
#[derive(Debug)]
pub(super) struct ReadFailure {
    pub error:WorkerError,
    pub invalidate:bool,
    pub rebuild:bool,
}
impl ReadFailure {
    fn invalid(message:&str)->Self {Self {error:invalid(message),invalidate:true,rebuild:false}}
    fn proof(guard:Option<&setups_mdclient::TailReadGuard>)->Self {
        let mut failure=Self::invalid("source causal history changed or repair coverage is unknown");
        failure.rebuild=guard.is_some_and(|guard|guard.status==setups_mdclient::TailReadGuardStatus::Invalid);
        failure
    }
    fn transport(error:MdClientError)->Self {
        let invalidate=matches!(&error,MdClientError::Decode(_));
        Self {error:invalid(&error.to_string()),invalidate,rebuild:false}
    }
}
fn tail_query(instrument_id:i64,closed:i64)->TailBatchQuery {
    TailBatchQuery {instrument_ids:vec![instrument_id],from_unix:closed-60,to_unix:closed}
}
async fn observe(client:&MarketDataClient,guard:&TailReadGuardQuery,closed:i64)->Result<setups_mdclient::TailInstrument,ReadFailure> {
    let response=client.tail_batch_guarded(&tail_query(guard.instrument_id,closed),std::slice::from_ref(guard))
        .await.map_err(ReadFailure::transport)?;
    let item=response.instruments.into_iter().next().ok_or_else(||ReadFailure::invalid("source guard instrument missing"))?;
    if item.status!="known" {return Err(ReadFailure::invalid("source guard instrument not found"));}
    Ok(item)
}

pub(super) async fn read_history(client:&MarketDataClient,instrument_id:i64,from:i64,target:i64,mut guard:TailReadGuardQuery,warmup_boundary:i64)->Result<ValidatedPage,ReadFailure> {
    // Rebase only after proving the ALREADY consumed live interval unchanged.
    // The coherent canonical end bounds this read; later ordinary finals are
    // pending input, not a mutation of the page being materialized.
    let before=observe(client,&guard,target).await?;
    let previous=if guard.previous.is_some() {
        guard.validated_token(before.read_guard.as_ref())
            .ok_or_else(||ReadFailure::proof(before.read_guard.as_ref()))?
    } else {
        before.read_guard.and_then(|guard|guard.token)
            .ok_or_else(||ReadFailure::invalid("source capture has no repair token"))?
    };
    let read_to=before.latest_stored_open_time_unix.map_or(from,|open|open.saturating_add(60)).min(target).max(from);
    let (bars,complete)=if read_to==from {(Vec::new(),true)} else {
        let page=client.candles(&CandlesQuery {instrument_id,from_unix:from,to_unix:read_to,after_unix:None,limit:Some(HISTORY_PAGE_LIMIT)},Duration::from_secs(10))
            .await.map_err(ReadFailure::transport)?;
        let bars:Vec<_>=page.candles.into_iter().map(|bar|DetectorBar {open_time_unix:bar.open_time_unix,close:bar.close,quote_turnover:bar.quote_turnover}).collect();
        validate_history_page(&bars,from,read_to,page.next_cursor_unix).map_err(|error|ReadFailure {error,invalidate:true,rebuild:false})?;
        (bars,page.next_cursor_unix.is_none())
    };
    let consumed=bars.last().map_or(from,|bar|bar.open_time_unix+60);
    let observed=if complete {target}else{consumed};
    guard.previous=Some(previous);
    guard.to_unix=guard.to_unix.max(consumed).max(warmup_boundary.min(observed));
    let item=observe(client,&guard,target).await?;
    let token=guard.validated_token(item.read_guard.as_ref())
        .ok_or_else(||ReadFailure::proof(item.read_guard.as_ref()))?;
    Ok(ValidatedPage {bars,complete,token})
}
