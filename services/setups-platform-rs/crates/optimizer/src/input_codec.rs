//! Framed CBOR stream: one bounded record buffer, no second full snapshot.
use crate::{checkpoint::BoundedBytes, *};
use serde::{Deserialize, Serialize};
use setups_analytics::OhlcCandle;
use setups_domain::DetectorBar;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::mem::size_of;

#[derive(Debug, Clone)]
pub struct FrozenInputCodecLimits {
    pub max_encoded_bytes: u64,
    pub max_decoded_bytes: usize,
    pub max_record_bytes: usize,
    pub max_records: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenInputDigest {
    pub encoded_bytes: u64,
    pub sha256: [u8; 32],
}

#[derive(Serialize)]
enum RecordRef<'a> {
    Header {
        version: u32,
        instruments: usize,
        snapshot_hash: &'a str,
        captured_at: &'a str,
    },
    Instrument {
        instrument: &'a StudyInstrument,
        detector_count: usize,
        candle_count: usize,
    },
    Detector(&'a DetectorBar),
    Candle(&'a OhlcCandle),
    EndInstrument,
    End,
}
#[derive(Deserialize)]
enum Record {
    Header {
        version: u32,
        instruments: usize,
        snapshot_hash: String,
        captured_at: String,
    },
    Instrument {
        instrument: StudyInstrument,
        detector_count: usize,
        candle_count: usize,
    },
    Detector(DetectorBar),
    Candle(OhlcCandle),
    EndInstrument,
    End,
}
fn error(message: impl std::fmt::Display) -> StudyError {
    StudyError::InvalidInput(format!("frozen input codec: {message}"))
}
fn valid_limits(l: &FrozenInputCodecLimits) -> Result<(), StudyError> {
    if l.max_encoded_bytes == 0
        || l.max_decoded_bytes == 0
        || l.max_record_bytes == 0
        || l.max_records == 0
    {
        return Err(error("zero codec limit"));
    }
    Ok(())
}
struct Stream {
    bytes: u64,
    records: u64,
    hash: Sha256,
}
impl Stream {
    fn new() -> Self {
        Self {
            bytes: 0,
            records: 0,
            hash: Sha256::new(),
        }
    }
    fn account(
        &mut self,
        header: &[u8; 8],
        body: &[u8],
        limits: &FrozenInputCodecLimits,
    ) -> Result<(), StudyError> {
        self.bytes = self
            .bytes
            .checked_add(8)
            .and_then(|b| b.checked_add(body.len() as u64))
            .ok_or_else(|| error("byte overflow"))?;
        self.records = self
            .records
            .checked_add(1)
            .ok_or_else(|| error("record overflow"))?;
        if self.bytes > limits.max_encoded_bytes || self.records > limits.max_records {
            return Err(error("stream limit exceeded"));
        }
        self.hash.update(header);
        self.hash.update(body);
        Ok(())
    }
    fn finish(self) -> FrozenInputDigest {
        FrozenInputDigest {
            encoded_bytes: self.bytes,
            sha256: self.hash.finalize().into(),
        }
    }
}

pub fn write_frozen_input<W: Write>(
    input: &StudyInput,
    writer: &mut W,
    limits: &FrozenInputCodecLimits,
) -> Result<FrozenInputDigest, StudyError> {
    valid_limits(limits)?;
    crate::evaluation::limit(
        "snapshot_bytes",
        snapshot_bytes(input),
        limits.max_decoded_bytes,
    )?;
    if input.instruments.len() > 1000 {
        return Err(error("too many instruments"));
    }
    let mut stream = Stream::new();
    let mut write = |record: RecordRef<'_>| -> Result<(), StudyError> {
        let mut buffer = BoundedBytes {
            bytes: Vec::new(),
            limit: limits.max_record_bytes,
        };
        ciborium::ser::into_writer(&record, &mut buffer).map_err(error)?;
        let header = (buffer.bytes.len() as u64).to_le_bytes();
        stream.account(&header, &buffer.bytes, limits)?;
        writer
            .write_all(&header)
            .and_then(|_| writer.write_all(&buffer.bytes))
            .map_err(error)
    };
    write(RecordRef::Header {
        version: 1,
        instruments: input.instruments.len(),
        snapshot_hash: &input.snapshot_hash,
        captured_at: &input.captured_at,
    })?;
    for data in &input.instruments {
        write(RecordRef::Instrument {
            instrument: &data.instrument,
            detector_count: data.detector_bars.len(),
            candle_count: data.candles.len(),
        })?;
        for bar in &data.detector_bars {
            write(RecordRef::Detector(bar))?;
        }
        for candle in &data.candles {
            write(RecordRef::Candle(candle))?;
        }
        write(RecordRef::EndInstrument)?;
    }
    write(RecordRef::End)?;
    Ok(stream.finish())
}

fn exact_vec<T>(count: usize, bytes: &mut usize, limit: usize) -> Result<Vec<T>, StudyError> {
    let additional = count
        .checked_mul(size_of::<T>())
        .ok_or_else(|| error("allocation overflow"))?;
    crate::evaluation::limit(
        "decoded_snapshot_bytes",
        bytes.saturating_add(additional),
        limit,
    )?;
    let mut values = Vec::new();
    values.try_reserve_exact(count).map_err(error)?;
    *bytes = bytes.saturating_add(values.capacity().saturating_mul(size_of::<T>()));
    crate::evaluation::limit("decoded_snapshot_bytes", *bytes, limit)?;
    Ok(values)
}

pub fn read_frozen_input<R: Read>(
    reader: &mut R,
    limits: &FrozenInputCodecLimits,
) -> Result<(StudyInput, FrozenInputDigest), StudyError> {
    valid_limits(limits)?;
    let mut stream = Stream::new();
    let mut next = |reader: &mut R| -> Result<Record, StudyError> {
        let mut header = [0; 8];
        reader.read_exact(&mut header).map_err(error)?;
        let size = usize::try_from(u64::from_le_bytes(header)).map_err(error)?;
        if size == 0
            || size > limits.max_record_bytes
            || stream.bytes.saturating_add(8).saturating_add(size as u64) > limits.max_encoded_bytes
            || stream.records >= limits.max_records
        {
            return Err(error("record/stream limit exceeded"));
        }
        let mut bytes = vec![0; size];
        reader.read_exact(&mut bytes).map_err(error)?;
        stream.account(&header, &bytes, limits)?;
        let mut cursor = std::io::Cursor::new(bytes.as_slice());
        let record = ciborium::de::from_reader(&mut cursor).map_err(error)?;
        if cursor.position() != size as u64 {
            return Err(error("trailing record bytes"));
        }
        Ok(record)
    };
    let Record::Header {
        version: 1,
        instruments,
        mut snapshot_hash,
        mut captured_at,
    } = next(reader)?
    else {
        return Err(error("missing/unsupported header"));
    };
    if instruments > 1000
        || snapshot_hash.is_empty()
        || snapshot_hash.len() > 256
        || captured_at.is_empty()
        || captured_at.len() > 64
    {
        return Err(error("invalid header"));
    }
    snapshot_hash.shrink_to_fit();
    captured_at.shrink_to_fit();
    let mut bytes = size_of::<StudyInput>() + snapshot_hash.capacity() + captured_at.capacity();
    let mut input = StudyInput {
        instruments: exact_vec(instruments, &mut bytes, limits.max_decoded_bytes)?,
        snapshot_hash,
        captured_at,
    };
    let mut identities = std::collections::BTreeSet::new();
    for _ in 0..instruments {
        let Record::Instrument {
            mut instrument,
            detector_count,
            candle_count,
        } = next(reader)?
        else {
            return Err(error("expected instrument"));
        };
        instrument.exchange.shrink_to_fit();
        instrument.category.shrink_to_fit();
        instrument.symbol.shrink_to_fit();
        if instrument.exchange.len() > 256
            || instrument.category.len() > 256
            || instrument.symbol.len() > 256
            || !identities.insert(instrument.clone())
        {
            return Err(error("invalid/duplicate instrument"));
        }
        if (detector_count as u64)
            .checked_add(candle_count as u64)
            .is_none_or(|count| count > limits.max_records)
        {
            return Err(error("declared row count exceeds stream limit"));
        }
        bytes = bytes
            .saturating_add(instrument.exchange.capacity())
            .saturating_add(instrument.category.capacity())
            .saturating_add(instrument.symbol.capacity());
        // Exact counts avoid power-of-two growth making a previously admitted
        // ~6.9 GiB input exceed its unchanged 8 GiB snapshot budget on restore.
        let mut data = InstrumentInput {
            instrument,
            detector_bars: exact_vec(detector_count, &mut bytes, limits.max_decoded_bytes)?,
            candles: exact_vec(candle_count, &mut bytes, limits.max_decoded_bytes)?,
        };
        let mut candles_started = false;
        loop {
            match next(reader)? {
                Record::Detector(mut bar) if !candles_started => {
                    if data.detector_bars.len() >= detector_count
                        || data
                            .detector_bars
                            .last()
                            .is_some_and(|old| old.open_time_unix >= bar.open_time_unix)
                    {
                        return Err(error("detector count/order"));
                    }
                    // CBOR's streaming String visitor grows geometrically (including
                    // an eight-byte minimum). Do not retain that slack per row.
                    bar.close.shrink_to_fit();
                    bar.quote_turnover.shrink_to_fit();
                    bytes = bytes
                        .saturating_add(bar.close.capacity())
                        .saturating_add(bar.quote_turnover.capacity());
                    data.detector_bars.push(bar);
                }
                Record::Candle(candle) => {
                    candles_started = true;
                    if data.candles.len() >= candle_count
                        || data.detector_bars.len() != detector_count
                        || ![candle.open, candle.high, candle.low, candle.close]
                            .iter()
                            .all(|v| v.is_finite())
                        || data
                            .candles
                            .last()
                            .is_some_and(|old| old.open_time_unix >= candle.open_time_unix)
                    {
                        return Err(error("invalid candle/count/order"));
                    }
                    data.candles.push(candle);
                }
                Record::EndInstrument => {
                    if data.detector_bars.len() != detector_count
                        || data.candles.len() != candle_count
                    {
                        return Err(error("incomplete instrument counts"));
                    }
                    break;
                }
                _ => return Err(error("unexpected instrument record")),
            }
            crate::evaluation::limit("decoded_snapshot_bytes", bytes, limits.max_decoded_bytes)?;
        }
        input.instruments.push(data);
    }
    if !matches!(next(reader)?, Record::End) {
        return Err(error("missing stream end"));
    }
    let mut trailing = [0];
    if reader.read(&mut trailing).map_err(error)? != 0 {
        return Err(error("trailing stream bytes"));
    }
    crate::evaluation::limit(
        "snapshot_bytes",
        snapshot_bytes(&input),
        limits.max_decoded_bytes,
    )?;
    // The checksum binds this REDUCED exact representation and original hash;
    // it deliberately does not reconstruct the original acquisition wire hash.
    Ok((input, stream.finish()))
}
