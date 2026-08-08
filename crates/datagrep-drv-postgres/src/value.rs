//! Wire-level `Value` mapping. `numeric` becomes a `Decimal` carried as a
//! string and NEVER goes through f64 — a silently wrong number is worse than
//! a crash.
//!
//! Decoding reads Postgres's binary wire format directly rather than going
//! through `tokio-postgres`'s convenience `FromSql` impls for chrono/uuid/etc
//! (which this crate does not depend on — see the `Cargo.toml` comment): the
//! [`DecodedCell`] wrapper below implements `FromSql` itself and dispatches
//! on [`Type`] to `decode_binary`. Encoding (query parameters) is the mirror
//! image via [`PgParam`]/`ToSql`.

use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use tokio_postgres::types::{FromSql, IsNull, Kind, ToSql, Type};

use datagrep_api::{shape::LogicalType, value::TzSpec, Value};

/// Days from the Postgres epoch (2000-01-01) to the Unix epoch (1970-01-01).
const PG_EPOCH_DAYS: i32 = 10_957;
/// Microseconds from the Postgres epoch to the Unix epoch.
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

/// The engine-neutral [`LogicalType`] a Postgres wire [`Type`] maps to.
/// Shared between value decoding (`FieldDef::logical`) and the catalog
/// (`describe`/`infer_shape`).
pub fn logical_type_of(ty: &Type) -> LogicalType {
    if let Kind::Array(_) = ty.kind() {
        return LogicalType::Array;
    }
    match *ty {
        Type::BOOL => LogicalType::Bool,
        Type::INT2 | Type::INT4 | Type::INT8 => LogicalType::I64,
        Type::OID => LogicalType::U64,
        Type::FLOAT4 | Type::FLOAT8 => LogicalType::F64,
        Type::NUMERIC => LogicalType::Decimal,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => LogicalType::Str,
        Type::BYTEA => LogicalType::Bytes,
        Type::DATE => LogicalType::Date,
        Type::TIME | Type::TIMETZ => LogicalType::Time,
        Type::TIMESTAMP => LogicalType::Timestamp,
        Type::TIMESTAMPTZ => LogicalType::Timestamp,
        Type::INTERVAL => LogicalType::Interval,
        Type::UUID => LogicalType::Uuid,
        Type::JSON | Type::JSONB => LogicalType::Json,
        _ => LogicalType::Unknown,
    }
}

/// A single decoded cell. Implements `FromSql` itself (rather than relying on
/// per-type convenience impls) so it can accept *every* Postgres type and
/// fall back to [`Value::Unsupported`] instead of erroring: an unrecognized
/// type must still surface its bytes rather than fail the whole row.
pub struct DecodedCell(pub Value);

impl<'a> FromSql<'a> for DecodedCell {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(DecodedCell(decode_binary(ty, raw)))
    }

    fn from_sql_null(_ty: &Type) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(DecodedCell(Value::Null))
    }

    fn accepts(_ty: &Type) -> bool {
        // We decode (or honestly fall back to `Unsupported`) every type
        // Postgres can report — there is no type this driver refuses to see.
        true
    }
}

/// Decode one non-NULL cell already known to be in Postgres binary format.
/// Never fails: unrecognized types become [`Value::Unsupported`] carrying the
/// raw bytes untouched.
pub fn decode_binary(ty: &Type, raw: &[u8]) -> Value {
    if let Kind::Array(elem_ty) = ty.kind() {
        return decode_array(elem_ty, raw);
    }
    match *ty {
        Type::BOOL => raw
            .first()
            .map(|b| Value::Bool(*b != 0))
            .unwrap_or(unsupported(ty, raw)),
        Type::INT2 => read_i16(raw)
            .map(|v| Value::I64(v as i64))
            .unwrap_or(unsupported(ty, raw)),
        Type::INT4 => read_i32(raw)
            .map(|v| Value::I64(v as i64))
            .unwrap_or(unsupported(ty, raw)),
        Type::INT8 => read_i64(raw)
            .map(Value::I64)
            .unwrap_or(unsupported(ty, raw)),
        Type::OID => read_i32(raw)
            .map(|v| Value::U64(v as u32 as u64))
            .unwrap_or(unsupported(ty, raw)),
        Type::FLOAT4 => read_i32(raw)
            .map(|v| Value::F64(f32::from_bits(v as u32) as f64))
            .unwrap_or(unsupported(ty, raw)),
        Type::FLOAT8 => read_i64(raw)
            .map(|v| Value::F64(f64::from_bits(v as u64)))
            .unwrap_or(unsupported(ty, raw)),
        Type::NUMERIC => decode_numeric(raw).unwrap_or(unsupported(ty, raw)),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            std::str::from_utf8(raw)
                .map(|s| Value::Str(Arc::from(s)))
                .unwrap_or(unsupported(ty, raw))
        }
        Type::BYTEA => Value::Bytes(Bytes::copy_from_slice(raw)),
        Type::DATE => read_i32(raw)
            .map(|days| Value::Date(days.saturating_add(PG_EPOCH_DAYS)))
            .unwrap_or(unsupported(ty, raw)),
        Type::TIME => read_i64(raw)
            .map(|micros| Value::Time {
                nanos: micros.saturating_mul(1_000),
            })
            .unwrap_or(unsupported(ty, raw)),
        Type::TIMESTAMP => read_i64(raw)
            .map(|micros| Value::Timestamp {
                micros: micros.saturating_add(PG_EPOCH_MICROS),
                tz: TzSpec::Naive,
            })
            .unwrap_or(unsupported(ty, raw)),
        Type::TIMESTAMPTZ => read_i64(raw)
            .map(|micros| Value::Timestamp {
                micros: micros.saturating_add(PG_EPOCH_MICROS),
                tz: TzSpec::Utc,
            })
            .unwrap_or(unsupported(ty, raw)),
        Type::INTERVAL => decode_interval(raw).unwrap_or(unsupported(ty, raw)),
        Type::UUID => {
            if raw.len() == 16 {
                let mut buf = [0u8; 16];
                buf.copy_from_slice(raw);
                Value::Uuid(buf)
            } else {
                unsupported(ty, raw)
            }
        }
        Type::JSON => std::str::from_utf8(raw)
            .map(|s| Value::Json(Arc::from(s)))
            .unwrap_or(unsupported(ty, raw)),
        Type::JSONB => decode_jsonb(raw).unwrap_or(unsupported(ty, raw)),
        _ => unsupported(ty, raw),
    }
}

fn unsupported(ty: &Type, raw: &[u8]) -> Value {
    let display =
        String::from_utf8(raw.to_vec()).unwrap_or_else(|_| format!("<{} raw bytes>", raw.len()));
    Value::Unsupported {
        type_name: Arc::from(ty.name()),
        raw: Bytes::copy_from_slice(raw),
        display: Arc::from(display),
    }
}

fn read_i16(raw: &[u8]) -> Option<i16> {
    Some(i16::from_be_bytes(raw.try_into().ok()?))
}
fn read_i32(raw: &[u8]) -> Option<i32> {
    Some(i32::from_be_bytes(raw.try_into().ok()?))
}
fn read_i64(raw: &[u8]) -> Option<i64> {
    Some(i64::from_be_bytes(raw.try_into().ok()?))
}
fn read_u16(raw: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(raw.try_into().ok()?))
}

fn decode_jsonb(raw: &[u8]) -> Option<Value> {
    // jsonb binary format: a 1-byte version prefix (always 1 today) then the
    // JSON text itself.
    let (version, text) = raw.split_first()?;
    if *version != 1 {
        return None;
    }
    std::str::from_utf8(text)
        .ok()
        .map(|s| Value::Json(Arc::from(s)))
}

fn decode_interval(raw: &[u8]) -> Option<Value> {
    if raw.len() != 16 {
        return None;
    }
    let micros = read_i64(&raw[0..8])?;
    let days = read_i32(&raw[8..12])?;
    let months = read_i32(&raw[12..16])?;
    Some(Value::Interval {
        months,
        days,
        nanos: micros.saturating_mul(1_000),
    })
}

fn decode_array(elem_ty: &Type, raw: &[u8]) -> Value {
    let mut cur = raw;
    let take = |cur: &mut &[u8], n: usize| -> Option<Vec<u8>> {
        if cur.len() < n {
            return None;
        }
        let (a, b) = cur.split_at(n);
        *cur = b;
        Some(a.to_vec())
    };
    let ndim = match take(&mut cur, 4).and_then(|b| read_i32(&b)) {
        Some(v) => v,
        None => return unsupported(elem_ty, raw),
    };
    // has-null flag (i32) + element type oid (u32) — not needed for decode.
    if take(&mut cur, 8).is_none() {
        return unsupported(elem_ty, raw);
    }
    if ndim <= 0 {
        return Value::Array(Arc::from(Vec::<Value>::new()));
    }
    // Every number below this point comes off the wire, so none of them may
    // size an allocation on its own say-so. Each dimension header is 8 bytes
    // (length + lower bound), so a payload too short to hold `ndim` of them is
    // malformed rather than merely large — and a server claiming `ndim =
    // 0x7fffffff` in a twelve-byte message would otherwise have this reserve
    // ~17 GB before the first bounds-checked read ran.
    let ndim = ndim as usize;
    if ndim > cur.len() / 8 {
        return unsupported(elem_ty, raw);
    }
    let mut dims = Vec::with_capacity(ndim);
    for _ in 0..ndim {
        let len = match take(&mut cur, 4).and_then(|b| read_i32(&b)) {
            Some(v) => v.max(0) as usize,
            None => return unsupported(elem_ty, raw),
        };
        // lower bound (i32) — datagrep has no concept of non-1 lower bounds; drop.
        if take(&mut cur, 4).is_none() {
            return unsupported(elem_ty, raw);
        }
        dims.push(len);
    }
    // Same reasoning for the element count, plus one more hazard: three
    // dimensions of 2^31 overflow the `product()` and wrap to a small, wrong
    // total, so multiply with `checked_mul`. Every element carries at least its
    // own four-byte length prefix — a NULL is exactly that and nothing more —
    // so anything above `cur.len() / 4` cannot be in this payload.
    let total = match dims.iter().try_fold(1usize, |acc, d| acc.checked_mul(*d)) {
        Some(total) if total <= cur.len() / 4 => total,
        _ => return unsupported(elem_ty, raw),
    };
    let mut flat = Vec::with_capacity(total);
    for _ in 0..total {
        let elen = match take(&mut cur, 4).and_then(|b| read_i32(&b)) {
            Some(v) => v,
            None => return unsupported(elem_ty, raw),
        };
        if elen < 0 {
            flat.push(Value::Null);
            continue;
        }
        match take(&mut cur, elen as usize) {
            Some(bytes) => flat.push(decode_binary(elem_ty, &bytes)),
            None => return unsupported(elem_ty, raw),
        }
    }
    Value::Array(Arc::from(nest(&dims, &mut flat.into_iter())))
}

fn nest(dims: &[usize], iter: &mut impl Iterator<Item = Value>) -> Vec<Value> {
    match dims.split_first() {
        None => Vec::new(),
        Some((&n, [])) => (0..n).filter_map(|_| iter.next()).collect(),
        Some((&n, rest)) => (0..n)
            .map(|_| Value::Array(Arc::from(nest(rest, iter))))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// NUMERIC binary <-> decimal string. Never f64: NUMERIC is arbitrary
// precision, and rounding it through a float would silently corrupt money.
//
// Wire format (`numeric_send`/`numeric_recv` in Postgres's `numeric.c`):
//   i16 ndigits, i16 weight, u16 sign, u16 dscale, then `ndigits` base-10000
//   digits (i16 each), most significant first. `weight` is the base-10000
//   exponent of the first stored digit.
// ---------------------------------------------------------------------------

const NUMERIC_POS: u16 = 0x0000;
const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;

fn decode_numeric(raw: &[u8]) -> Option<Value> {
    if raw.len() < 8 {
        return None;
    }
    let ndigits = read_i16(&raw[0..2])?;
    let weight = read_i16(&raw[2..4])? as i32;
    let sign = read_u16(&raw[4..6])?;
    let dscale = read_u16(&raw[6..8])?;
    if sign == NUMERIC_NAN {
        return Some(Value::Decimal(Arc::from("NaN")));
    }
    if sign != NUMERIC_POS && sign != NUMERIC_NEG {
        // Positive/negative infinity (PG 17+) or an unrecognized sign code —
        // no stable decimal-string representation, so this is an honest
        // "unsupported", not a silent wrong number.
        return None;
    }
    let neg = sign == NUMERIC_NEG;
    if ndigits < 0 {
        return None;
    }
    let ndigits = ndigits as usize;
    if raw.len() < 8 + ndigits * 2 {
        return None;
    }
    let mut digits = Vec::with_capacity(ndigits);
    for i in 0..ndigits {
        let off = 8 + i * 2;
        digits.push(read_i16(&raw[off..off + 2])? as i32);
    }
    Some(Value::Decimal(Arc::from(numeric_digits_to_string(
        neg, weight, dscale, &digits,
    ))))
}

fn numeric_digits_to_string(neg: bool, weight: i32, dscale: u16, digits: &[i32]) -> String {
    if digits.is_empty() {
        return if dscale == 0 {
            "0".to_string()
        } else {
            format!("0.{}", "0".repeat(dscale as usize))
        };
    }
    let ndigits = digits.len() as i32;
    let get = |exp: i32| -> i32 {
        let i = weight - exp;
        if i >= 0 && (i as usize) < digits.len() {
            digits[i as usize]
        } else {
            0
        }
    };

    let mut int_part = String::new();
    if weight >= 0 {
        for (g, exp) in (0..=weight).rev().enumerate() {
            let v = get(exp);
            if g == 0 {
                int_part.push_str(&v.to_string());
            } else {
                int_part.push_str(&format!("{v:04}"));
            }
        }
    } else {
        int_part.push('0');
    }

    // Fractional groups needed to cover `dscale` decimal digits.
    let min_group_exp = -div_ceil(dscale as i32, 4);
    // Also cover any stored digits further right than dscale implies (should
    // not happen with well-formed server output, but never drop real data).
    let last_stored_exp = weight - (ndigits - 1);
    let lowest_exp = min_group_exp.min(last_stored_exp.min(-1));

    let mut frac_part = String::new();
    if weight >= -1 || ndigits > 0 {
        let mut exp = -1;
        while exp >= lowest_exp {
            frac_part.push_str(&format!("{:04}", get(exp)));
            exp -= 1;
        }
    }
    if frac_part.len() > dscale as usize {
        frac_part.truncate(dscale as usize);
    } else {
        while frac_part.len() < dscale as usize {
            frac_part.push('0');
        }
    }

    let sign_str = if neg && !(int_part == "0" && frac_part.chars().all(|c| c == '0')) {
        "-"
    } else {
        ""
    };
    if dscale == 0 {
        format!("{sign_str}{int_part}")
    } else {
        format!("{sign_str}{int_part}.{frac_part}")
    }
}

fn div_ceil(a: i32, b: i32) -> i32 {
    (a + b - 1) / b
}

/// Encode a decimal string to Postgres NUMERIC binary wire format — the
/// inverse of [`decode_numeric`]. Used by [`PgParam`] when binding a
/// `Value::Decimal` query parameter.
fn encode_numeric(s: &str) -> Result<Vec<u8>, String> {
    if s.eq_ignore_ascii_case("nan") {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&NUMERIC_NAN.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        return Ok(out);
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_str, frac_str) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    let int_str = if int_str.is_empty() { "0" } else { int_str };
    if !int_str.bytes().all(|b| b.is_ascii_digit()) || !frac_str.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!("not a decimal number: {s:?}"));
    }
    let dscale = frac_str.len() as u16;

    // Pad to a multiple of 4 so grouping aligns on the decimal point:
    // integer part padded on the left, fractional part padded on the right.
    let int_pad = (4 - int_str.len() % 4) % 4;
    let padded_int: String = "0".repeat(int_pad) + int_str;
    let frac_pad = (4 - frac_str.len() % 4) % 4;
    let padded_frac: String = frac_str.to_string() + &"0".repeat(frac_pad);

    let mut groups: Vec<i32> = Vec::new();
    let int_groups = padded_int.len() / 4;
    for g in 0..int_groups {
        let chunk = &padded_int[g * 4..g * 4 + 4];
        groups.push(chunk.parse::<i32>().map_err(|e| e.to_string())?);
    }
    let mut weight = int_groups as i32 - 1;
    let frac_groups = padded_frac.len() / 4;
    for g in 0..frac_groups {
        let chunk = &padded_frac[g * 4..g * 4 + 4];
        groups.push(chunk.parse::<i32>().map_err(|e| e.to_string())?);
    }

    // Trim leading all-zero groups (adjusting weight) and trailing all-zero
    // groups (dscale already captures the intended display precision).
    let mut start = 0;
    while start < groups.len() && groups[start] == 0 {
        start += 1;
        weight -= 1;
    }
    let mut end = groups.len();
    while end > start && groups[end - 1] == 0 {
        end -= 1;
    }
    let digits = &groups[start..end];

    let mut out = Vec::with_capacity(8 + digits.len() * 2);
    out.extend_from_slice(&(digits.len() as i16).to_be_bytes());
    out.extend_from_slice(&(if digits.is_empty() { 0 } else { weight as i16 }).to_be_bytes());
    let sign = if neg && !digits.is_empty() {
        NUMERIC_NEG
    } else {
        NUMERIC_POS
    };
    out.extend_from_slice(&sign.to_be_bytes());
    out.extend_from_slice(&dscale.to_be_bytes());
    for d in digits {
        out.extend_from_slice(&(*d as i16).to_be_bytes());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Query parameter encoding (`ToSql`) — the inverse direction.
// ---------------------------------------------------------------------------

/// Wraps a `&datagrep_api::Value` so it can implement the foreign `ToSql` trait
/// (orphan rules forbid implementing it directly on `datagrep_api::Value`).
pub struct PgParam<'a>(pub &'a Value);

impl fmt::Debug for PgParam<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PgParam({:?})", self.0)
    }
}

impl ToSql for PgParam<'_> {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut bytes::BytesMut,
    ) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        match self.0 {
            Value::Null | Value::Absent => return Ok(IsNull::Yes),
            Value::Bool(b) => out.extend_from_slice(&[*b as u8]),
            Value::I64(v) => match *ty {
                Type::INT2 => out.extend_from_slice(&(i16::try_from(*v)?).to_be_bytes()),
                Type::INT4 => out.extend_from_slice(&(i32::try_from(*v)?).to_be_bytes()),
                _ => out.extend_from_slice(&v.to_be_bytes()),
            },
            Value::U64(v) => match *ty {
                Type::INT2 => out.extend_from_slice(&(i16::try_from(*v)?).to_be_bytes()),
                Type::INT4 | Type::OID => {
                    out.extend_from_slice(&(u32::try_from(*v)?).to_be_bytes())
                }
                _ => out.extend_from_slice(&(i64::try_from(*v)?).to_be_bytes()),
            },
            Value::F64(v) => match *ty {
                Type::FLOAT4 => out.extend_from_slice(&(*v as f32).to_bits().to_be_bytes()),
                _ => out.extend_from_slice(&v.to_bits().to_be_bytes()),
            },
            Value::Decimal(s) => {
                let bin = encode_numeric(s)
                    .map_err(|e| -> Box<dyn StdError + Sync + Send> { e.into() })?;
                out.extend_from_slice(&bin);
            }
            Value::Str(s) => out.extend_from_slice(s.as_bytes()),
            Value::Bytes(b) => out.extend_from_slice(b),
            Value::Date(days) => out.extend_from_slice(&(days - PG_EPOCH_DAYS).to_be_bytes()),
            Value::Time { nanos } => out.extend_from_slice(&(nanos / 1_000).to_be_bytes()),
            Value::Timestamp { micros, .. } => {
                out.extend_from_slice(&(micros - PG_EPOCH_MICROS).to_be_bytes())
            }
            Value::Interval {
                months,
                days,
                nanos,
            } => {
                out.extend_from_slice(&(nanos / 1_000).to_be_bytes());
                out.extend_from_slice(&days.to_be_bytes());
                out.extend_from_slice(&months.to_be_bytes());
            }
            Value::Uuid(bytes) => out.extend_from_slice(bytes),
            Value::Json(text) => {
                if *ty == Type::JSONB {
                    out.extend_from_slice(&[1u8]);
                }
                out.extend_from_slice(text.as_bytes());
            }
            Value::Array(_) => {
                return Err(
                    format!("array parameter binding not implemented for {}", ty.name()).into(),
                )
            }
            other => {
                return Err(format!("cannot bind {other:?} as a Postgres parameter").into());
            }
        }
        Ok(IsNull::No)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    tokio_postgres::types::to_sql_checked!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(s: &str) {
        let bin = encode_numeric(s).unwrap_or_else(|e| panic!("encode {s:?}: {e}"));
        let decoded = decode_numeric(&bin).unwrap_or_else(|| panic!("decode failed for {s:?}"));
        match decoded {
            Value::Decimal(d) => assert_eq!(&*d, s, "round-trip mismatch for {s:?}"),
            other => panic!("expected Decimal, got {other:?}"),
        }
    }

    #[test]
    fn numeric_round_trips_common_values() {
        for s in [
            "0",
            "0.00",
            "1",
            "-1",
            "12345.6789",
            "0.001",
            "-42.5",
            "100000000",
            "0.0001",
            "999999999999999999999999.999999",
            "1.10",
            "-0.5",
            "5.00",
        ] {
            roundtrip(s);
        }
    }

    #[test]
    fn numeric_decode_matches_hand_encoded_bytes() {
        // 12345.6789: digits [1, 2345, 6789], weight=1, dscale=4, positive.
        let mut raw = Vec::new();
        raw.extend_from_slice(&3i16.to_be_bytes());
        raw.extend_from_slice(&1i16.to_be_bytes());
        raw.extend_from_slice(&NUMERIC_POS.to_be_bytes());
        raw.extend_from_slice(&4u16.to_be_bytes());
        for d in [1i16, 2345, 6789] {
            raw.extend_from_slice(&d.to_be_bytes());
        }
        match decode_numeric(&raw).unwrap() {
            Value::Decimal(s) => assert_eq!(&*s, "12345.6789"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn numeric_zero_variants() {
        // ndigits=0, weight=0, dscale=2 -> "0.00" (Postgres's own encoding of 0.00).
        let mut raw = Vec::new();
        raw.extend_from_slice(&0i16.to_be_bytes());
        raw.extend_from_slice(&0i16.to_be_bytes());
        raw.extend_from_slice(&NUMERIC_POS.to_be_bytes());
        raw.extend_from_slice(&2u16.to_be_bytes());
        match decode_numeric(&raw).unwrap() {
            Value::Decimal(s) => assert_eq!(&*s, "0.00"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn numeric_nan() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&0i16.to_be_bytes());
        raw.extend_from_slice(&0i16.to_be_bytes());
        raw.extend_from_slice(&NUMERIC_NAN.to_be_bytes());
        raw.extend_from_slice(&0u16.to_be_bytes());
        match decode_numeric(&raw).unwrap() {
            Value::Decimal(s) => assert_eq!(&*s, "NaN"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn oid_to_logical_type_mapping() {
        assert_eq!(logical_type_of(&Type::BOOL), LogicalType::Bool);
        assert_eq!(logical_type_of(&Type::INT2), LogicalType::I64);
        assert_eq!(logical_type_of(&Type::INT4), LogicalType::I64);
        assert_eq!(logical_type_of(&Type::INT8), LogicalType::I64);
        assert_eq!(logical_type_of(&Type::FLOAT4), LogicalType::F64);
        assert_eq!(logical_type_of(&Type::FLOAT8), LogicalType::F64);
        assert_eq!(logical_type_of(&Type::NUMERIC), LogicalType::Decimal);
        assert_eq!(logical_type_of(&Type::TEXT), LogicalType::Str);
        assert_eq!(logical_type_of(&Type::VARCHAR), LogicalType::Str);
        assert_eq!(logical_type_of(&Type::BYTEA), LogicalType::Bytes);
        assert_eq!(logical_type_of(&Type::DATE), LogicalType::Date);
        assert_eq!(logical_type_of(&Type::TIME), LogicalType::Time);
        assert_eq!(logical_type_of(&Type::TIMESTAMP), LogicalType::Timestamp);
        assert_eq!(logical_type_of(&Type::TIMESTAMPTZ), LogicalType::Timestamp);
        assert_eq!(logical_type_of(&Type::UUID), LogicalType::Uuid);
        assert_eq!(logical_type_of(&Type::JSON), LogicalType::Json);
        assert_eq!(logical_type_of(&Type::JSONB), LogicalType::Json);
        assert_eq!(logical_type_of(&Type::INT4_ARRAY), LogicalType::Array);
        assert_eq!(logical_type_of(&Type::REGCLASS), LogicalType::Unknown);
    }

    #[test]
    fn decode_bool_int_text() {
        assert_eq!(decode_binary(&Type::BOOL, &[1]), Value::Bool(true));
        assert_eq!(decode_binary(&Type::BOOL, &[0]), Value::Bool(false));
        assert_eq!(
            decode_binary(&Type::INT4, &42i32.to_be_bytes()),
            Value::I64(42)
        );
        assert_eq!(
            decode_binary(&Type::INT8, &(-7i64).to_be_bytes()),
            Value::I64(-7)
        );
        assert_eq!(
            decode_binary(&Type::TEXT, b"hello"),
            Value::Str(Arc::from("hello"))
        );
    }

    #[test]
    fn decode_date_uses_unix_epoch_offset() {
        // Postgres day 0 = 2000-01-01 = 10957 Unix days.
        assert_eq!(
            decode_binary(&Type::DATE, &0i32.to_be_bytes()),
            Value::Date(10_957)
        );
    }

    #[test]
    fn decode_timestamptz_is_utc() {
        let v = decode_binary(&Type::TIMESTAMPTZ, &0i64.to_be_bytes());
        assert_eq!(
            v,
            Value::Timestamp {
                micros: PG_EPOCH_MICROS,
                tz: TzSpec::Utc
            }
        );
    }

    #[test]
    fn decode_timestamp_without_tz_is_naive() {
        let v = decode_binary(&Type::TIMESTAMP, &0i64.to_be_bytes());
        assert_eq!(
            v,
            Value::Timestamp {
                micros: PG_EPOCH_MICROS,
                tz: TzSpec::Naive
            }
        );
    }

    #[test]
    fn decode_uuid() {
        let raw = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        assert_eq!(decode_binary(&Type::UUID, &raw), Value::Uuid(raw));
    }

    #[test]
    fn decode_jsonb_strips_version_byte() {
        let mut raw = vec![1u8];
        raw.extend_from_slice(br#"{"a":1}"#);
        assert_eq!(
            decode_binary(&Type::JSONB, &raw),
            Value::Json(Arc::from(r#"{"a":1}"#))
        );
    }

    #[test]
    fn decode_unknown_type_is_unsupported_never_lost() {
        let v = decode_binary(&Type::POINT, b"\x00\x01\x02");
        match v {
            Value::Unsupported { type_name, raw, .. } => {
                assert_eq!(&*type_name, "point");
                assert_eq!(&raw[..], b"\x00\x01\x02");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn decode_int4_array_one_dim() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&1i32.to_be_bytes()); // ndim
        raw.extend_from_slice(&0i32.to_be_bytes()); // has_null
        raw.extend_from_slice(&(Type::INT4.oid()).to_be_bytes()); // elem oid
        raw.extend_from_slice(&3i32.to_be_bytes()); // dim len
        raw.extend_from_slice(&1i32.to_be_bytes()); // lower bound
        for v in [10i32, 20, 30] {
            raw.extend_from_slice(&4i32.to_be_bytes());
            raw.extend_from_slice(&v.to_be_bytes());
        }
        let v = decode_binary(&Type::INT4_ARRAY, &raw);
        assert_eq!(
            v,
            Value::Array(Arc::from(vec![
                Value::I64(10),
                Value::I64(20),
                Value::I64(30)
            ]))
        );
    }

    #[test]
    fn decode_array_with_null_element() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&1i32.to_be_bytes());
        raw.extend_from_slice(&1i32.to_be_bytes());
        raw.extend_from_slice(&(Type::TEXT.oid()).to_be_bytes());
        raw.extend_from_slice(&2i32.to_be_bytes());
        raw.extend_from_slice(&1i32.to_be_bytes());
        raw.extend_from_slice(&3i32.to_be_bytes());
        raw.extend_from_slice(b"abc");
        raw.extend_from_slice(&(-1i32).to_be_bytes()); // NULL element
        let v = decode_binary(&Type::TEXT_ARRAY, &raw);
        assert_eq!(
            v,
            Value::Array(Arc::from(vec![Value::Str(Arc::from("abc")), Value::Null]))
        );
    }

    /// An array payload is a *server* message, so its dimension count and
    /// per-dimension lengths are attacker-controlled numbers. They used to size
    /// `Vec::with_capacity` directly: a claimed `ndim` of 2^31 reserved ~17 GB
    /// from a twelve-byte message, and three dimensions of 2^31 overflowed the
    /// `product()` outright. Both are now bounded by what the payload can
    /// actually carry, and a malformed one degrades to `Unsupported` — the
    /// driver's existing "never lose bytes, never crash on a quirk" answer.
    #[test]
    fn a_hostile_array_header_is_unsupported_not_an_allocation() {
        let hdr = |ndim: i32, dims: &[i32]| {
            let mut b = Vec::new();
            b.extend_from_slice(&ndim.to_be_bytes());
            b.extend_from_slice(&0i32.to_be_bytes()); // has-null flag
            b.extend_from_slice(&23u32.to_be_bytes()); // element oid
            for d in dims {
                b.extend_from_slice(&d.to_be_bytes());
                b.extend_from_slice(&1i32.to_be_bytes()); // lower bound
            }
            b
        };

        // A dimension count no payload this size could carry.
        let v = decode_array(&Type::INT4, &hdr(i32::MAX, &[]));
        assert!(matches!(v, Value::Unsupported { .. }), "got {v:?}");

        // Dimensions whose product overflows `usize`.
        let huge = hdr(3, &[i32::MAX, i32::MAX, i32::MAX]);
        let v = decode_array(&Type::INT4, &huge);
        assert!(matches!(v, Value::Unsupported { .. }), "got {v:?}");

        // A single dimension claiming more elements than the bytes allow.
        let v = decode_array(&Type::INT4, &hdr(1, &[1_000_000]));
        assert!(matches!(v, Value::Unsupported { .. }), "got {v:?}");

        // The honest case still decodes.
        let mut ok = hdr(1, &[2]);
        for n in [7i32, 8i32] {
            ok.extend_from_slice(&4i32.to_be_bytes());
            ok.extend_from_slice(&n.to_be_bytes());
        }
        match decode_array(&Type::INT4, &ok) {
            Value::Array(items) => assert_eq!(items.len(), 2, "{items:?}"),
            other => panic!("expected an array, got {other:?}"),
        }
    }
}
