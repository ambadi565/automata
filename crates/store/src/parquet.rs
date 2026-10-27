//! Parquet codecs for bars and instruments.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Date32Type, Int64Type, TimestampMillisecondType, UInt32Type, UInt64Type};
use arrow_array::{
    Array, ArrayRef, Date32Array, Int64Array, RecordBatch, StringArray, TimestampMillisecondArray, UInt32Array,
    UInt64Array,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use chrono::{DateTime, NaiveDate};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use parquet::format::KeyValue;
use saarthi_core::{Bar, Exchange, Instrument, Price, Symbol};

use crate::error::{StoreError, StoreResult};

const TZ: &str = "Asia/Kolkata";
const META_PRICE_UNIT: (&str, &str) = ("saarthi.price_unit", "paise");

fn props() -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_key_value_metadata(Some(vec![KeyValue::new(
            META_PRICE_UNIT.0.into(),
            META_PRICE_UNIT.1.to_string(),
        )]))
        .build()
}

fn write_batch(path: &Path, batch: &RecordBatch) -> StoreResult<()> {
    crate::atomic_write(path, |f| {
        let mut w = ArrowWriter::try_new(f, batch.schema(), Some(props()))?;
        w.write(batch)?;
        w.close()?;
        Ok(())
    })
}

fn read_batches(path: &Path) -> StoreResult<Vec<RecordBatch>> {
    let f = File::open(path).map_err(|e| StoreError::io(path, e))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(f)?.build()?;
    Ok(reader.collect::<Result<Vec<_>, _>>()?)
}

fn column<'a>(batch: &'a RecordBatch, name: &str, path: &Path) -> StoreResult<&'a ArrayRef> {
    batch.column_by_name(name).ok_or_else(|| StoreError::Schema {
        path: path.display().to_string(),
        detail: format!("missing column {name}"),
    })
}

fn schema_err(path: &Path, detail: impl Into<String>) -> StoreError {
    StoreError::Schema {
        path: path.display().to_string(),
        detail: detail.into(),
    }
}

// ---------------------------------------------------------------- bars

pub fn bar_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("ts", DataType::Timestamp(TimeUnit::Millisecond, Some(TZ.into())), false),
        Field::new("open", DataType::Int64, false),
        Field::new("high", DataType::Int64, false),
        Field::new("low", DataType::Int64, false),
        Field::new("close", DataType::Int64, false),
        Field::new("volume", DataType::UInt64, false),
    ]))
}

/// Writes bars (any order) sorted by time, atomically replacing `path`.
pub fn write_bars(path: &Path, bars: &[Bar]) -> StoreResult<()> {
    let mut bars = bars.to_vec();
    bars.sort_by_key(|b| b.ts);
    let px = |f: fn(&Bar) -> Price| Arc::new(Int64Array::from_iter_values(bars.iter().map(|b| f(b).0))) as ArrayRef;
    let batch = RecordBatch::try_new(
        bar_schema(),
        vec![
            Arc::new(
                TimestampMillisecondArray::from_iter_values(bars.iter().map(|b| b.ts.timestamp_millis()))
                    .with_timezone(TZ),
            ),
            px(|b| b.open),
            px(|b| b.high),
            px(|b| b.low),
            px(|b| b.close),
            Arc::new(UInt64Array::from_iter_values(bars.iter().map(|b| b.volume))),
        ],
    )?;
    write_batch(path, &batch)
}

/// Reads bars; a missing file is an empty series.
pub fn read_bars(path: &Path) -> StoreResult<Vec<Bar>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for batch in read_batches(path)? {
        let ts = column(&batch, "ts", path)?
            .as_primitive_opt::<TimestampMillisecondType>()
            .ok_or_else(|| schema_err(path, "ts is not timestamp[ms]"))?;
        let i64col = |n: &str| -> StoreResult<_> {
            column(&batch, n, path)?
                .as_primitive_opt::<Int64Type>()
                .ok_or_else(|| schema_err(path, format!("{n} is not int64")))
        };
        let (o, h, l, c) = (i64col("open")?, i64col("high")?, i64col("low")?, i64col("close")?);
        let v = column(&batch, "volume", path)?
            .as_primitive_opt::<UInt64Type>()
            .ok_or_else(|| schema_err(path, "volume is not uint64"))?;
        for i in 0..batch.num_rows() {
            out.push(Bar {
                ts: DateTime::from_timestamp_millis(ts.value(i)).ok_or_else(|| schema_err(path, "ts out of range"))?,
                open: Price(o.value(i)),
                high: Price(h.value(i)),
                low: Price(l.value(i)),
                close: Price(c.value(i)),
                volume: v.value(i),
            });
        }
    }
    Ok(out)
}

/// Union of two series keyed by timestamp; `newer` wins on conflicts.
pub fn merge_bars(existing: Vec<Bar>, newer: Vec<Bar>) -> Vec<Bar> {
    let mut map: BTreeMap<_, Bar> = existing.into_iter().map(|b| (b.ts, b)).collect();
    map.extend(newer.into_iter().map(|b| (b.ts, b)));
    map.into_values().collect()
}

// ---------------------------------------------------------------- instruments

pub fn instrument_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("token", DataType::Utf8, false),
        Field::new("symbol", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("exchange", DataType::Utf8, false),
        Field::new("instrument_type", DataType::Utf8, false),
        Field::new("lot_size", DataType::UInt32, false),
        Field::new("tick_size", DataType::Int64, false),
        Field::new("expiry", DataType::Date32, true),
        Field::new("strike", DataType::Int64, true),
    ]))
}

const EPOCH: NaiveDate = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid date");

pub fn write_instruments(path: &Path, v: &[Instrument]) -> StoreResult<()> {
    let s = |f: fn(&Instrument) -> &str| Arc::new(StringArray::from_iter_values(v.iter().map(f))) as ArrayRef;
    let batch = RecordBatch::try_new(
        instrument_schema(),
        vec![
            s(|i| &i.token),
            s(|i| i.symbol.as_str()),
            s(|i| &i.name),
            s(|i| i.exchange.as_str()),
            s(|i| &i.instrument_type),
            Arc::new(UInt32Array::from_iter_values(v.iter().map(|i| i.lot_size))),
            Arc::new(Int64Array::from_iter_values(v.iter().map(|i| i.tick_size.0))),
            Arc::new(Date32Array::from_iter(
                v.iter().map(|i| i.expiry.map(|d| (d - EPOCH).num_days() as i32)),
            )),
            Arc::new(Int64Array::from_iter(v.iter().map(|i| i.strike.map(|p| p.0)))),
        ],
    )?;
    write_batch(path, &batch)
}

pub fn read_instruments(path: &Path) -> StoreResult<Vec<Instrument>> {
    let mut out = Vec::new();
    for batch in read_batches(path)? {
        let s = |n: &str| -> StoreResult<_> {
            column(&batch, n, path)?
                .as_string_opt::<i32>()
                .ok_or_else(|| schema_err(path, format!("{n} is not utf8")))
        };
        let (token, symbol, name, exch, itype) = (
            s("token")?,
            s("symbol")?,
            s("name")?,
            s("exchange")?,
            s("instrument_type")?,
        );
        let lot = column(&batch, "lot_size", path)?
            .as_primitive_opt::<UInt32Type>()
            .ok_or_else(|| schema_err(path, "lot_size is not uint32"))?;
        let i64col = |n: &str| -> StoreResult<_> {
            column(&batch, n, path)?
                .as_primitive_opt::<Int64Type>()
                .ok_or_else(|| schema_err(path, format!("{n} is not int64")))
        };
        let (tick, strike) = (i64col("tick_size")?, i64col("strike")?);
        let expiry = column(&batch, "expiry", path)?
            .as_primitive_opt::<Date32Type>()
            .ok_or_else(|| schema_err(path, "expiry is not date32"))?;
        for i in 0..batch.num_rows() {
            let exchange: Exchange = exch.value(i).parse().map_err(|e| schema_err(path, format!("{e}")))?;
            out.push(Instrument {
                token: token.value(i).to_string(),
                symbol: Symbol::new(symbol.value(i)),
                name: name.value(i).to_string(),
                exchange,
                instrument_type: itype.value(i).to_string(),
                lot_size: lot.value(i),
                tick_size: Price(tick.value(i)),
                expiry: (!expiry.is_null(i)).then(|| EPOCH + chrono::Duration::days(i64::from(expiry.value(i)))),
                strike: (!strike.is_null(i)).then(|| Price(strike.value(i))),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use saarthi_core::MarketClock;

    fn bar(hm: &str, close: i64) -> Bar {
        Bar {
            ts: MarketClock::at(
                "2026-10-27".parse().unwrap(),
                saarthi_core::clock::parse_hhmm(hm).unwrap(),
            ),
            open: Price(100),
            high: Price(close.max(100) + 5),
            low: Price(90),
            close: Price(close),
            volume: 42,
        }
    }

    #[test]
    fn bars_round_trip_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x/RELIANCE-EQ.parquet");
        let bars = vec![bar("09:20", 101), bar("09:15", 99)];
        write_bars(&p, &bars).unwrap();
        let back = read_bars(&p).unwrap();
        assert_eq!(back, vec![bars[1], bars[0]]);
        // No temp files left behind.
        let names: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn missing_file_is_empty() {
        assert!(read_bars(Path::new("/nonexistent/x.parquet")).unwrap().is_empty());
    }

    #[test]
    fn merge_prefers_newer() {
        let merged = merge_bars(
            vec![bar("09:15", 99), bar("09:20", 100)],
            vec![bar("09:20", 105), bar("09:25", 106)],
        );
        assert_eq!(merged.iter().map(|b| b.close.0).collect::<Vec<_>>(), vec![99, 105, 106]);
    }

    #[test]
    fn instruments_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("instruments.parquet");
        let v = vec![
            Instrument {
                token: "2885".into(),
                symbol: Symbol::new("RELIANCE-EQ"),
                name: "RELIANCE".into(),
                exchange: Exchange::Nse,
                instrument_type: String::new(),
                lot_size: 1,
                tick_size: Price(10),
                expiry: None,
                strike: None,
            },
            Instrument {
                token: "35003".into(),
                symbol: Symbol::new("NIFTY26DEC2425000CE"),
                name: "NIFTY".into(),
                exchange: Exchange::Nfo,
                instrument_type: "OPTIDX".into(),
                lot_size: 75,
                tick_size: Price(5),
                expiry: Some("2024-12-26".parse().unwrap()),
                strike: Some(Price(2_500_000)),
            },
        ];
        write_instruments(&p, &v).unwrap();
        assert_eq!(read_instruments(&p).unwrap(), v);
    }

    #[test]
    fn file_declares_price_unit() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.parquet");
        write_bars(&p, &[bar("09:15", 99)]).unwrap();
        let f = File::open(&p).unwrap();
        let meta = ParquetRecordBatchReaderBuilder::try_new(f).unwrap().metadata().clone();
        let kv = meta.file_metadata().key_value_metadata().unwrap();
        assert!(
            kv.iter()
                .any(|k| k.key == "saarthi.price_unit" && k.value.as_deref() == Some("paise"))
        );
    }
}
