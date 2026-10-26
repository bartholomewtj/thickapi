use serde_json::Value;

// ---------------------------------------------------------------------------------------------
// Protobuf + Connect encoding (hand-rolled to match the captured wire)
// ---------------------------------------------------------------------------------------------

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[doc(hidden)]
pub fn field_ld(field: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4);
    encode_varint((field << 3) | 2, &mut out);
    encode_varint(data.len() as u64, &mut out);
    out.extend_from_slice(data);
    out
}

#[doc(hidden)]
pub fn field_varint(field: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint(field << 3, &mut out);
    encode_varint(value, &mut out);
    out
}

#[doc(hidden)]
pub fn field_str(field: u64, s: &str) -> Vec<u8> {
    field_ld(field, s.as_bytes())
}

fn field_double(field: u64, value: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    encode_varint((field << 3) | 1, &mut out);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

/// JSON → `google.protobuf.Value` (oneof tags: null=1 varint, number=2 double, string=3,
/// bool=4 varint, struct=5, list=6).
pub fn encode_protobuf_value(value: &Value) -> Vec<u8> {
    match value {
        Value::Null => field_varint(1, 0),
        Value::Bool(b) => field_varint(4, u64::from(*b)),
        Value::Number(n) => field_double(2, n.as_f64().unwrap_or(0.0)),
        Value::String(s) => field_str(3, s),
        Value::Array(items) => {
            let mut list = Vec::new();
            for item in items {
                list.extend(field_ld(1, &encode_protobuf_value(item)));
            }
            field_ld(6, &list)
        }
        Value::Object(map) => {
            let mut fields = Vec::new();
            for (key, val) in map {
                let mut entry = field_str(1, key);
                entry.extend(field_ld(2, &encode_protobuf_value(val)));
                fields.extend(field_ld(1, &entry));
            }
            field_ld(5, &fields)
        }
    }
}

pub(super) struct PbField<'a> {
    pub(super) field: u64,
    pub(super) wire: u8,
    pub(super) data: &'a [u8],
}

pub(super) fn read_varint(buf: &[u8]) -> Option<(u64, &[u8])> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, buf.get(i + 1..)?));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

pub(super) fn iter_fields(mut buf: &[u8]) -> impl Iterator<Item = PbField<'_>> {
    std::iter::from_fn(move || {
        if buf.is_empty() {
            return None;
        }
        let (tag, rest) = read_varint(buf)?;
        let field = tag >> 3;
        let wire = (tag & 7) as u8;
        buf = rest;
        match wire {
            0 => {
                let rest_before = buf;
                let (_v, rest) = read_varint(buf)?;
                let n = rest_before.len() - rest.len();
                let data = rest_before.get(..n)?;
                buf = rest;
                Some(PbField { field, wire, data })
            }
            2 => {
                let (len, rest) = read_varint(buf)?;
                let len = usize::try_from(len).ok()?;
                if rest.len() < len {
                    return None;
                }
                let data = rest.get(..len)?;
                buf = rest.get(len..)?;
                Some(PbField { field, wire, data })
            }
            5 => {
                if buf.len() < 4 {
                    return None;
                }
                buf = buf.get(4..)?;
                Some(PbField {
                    field,
                    wire,
                    data: &[],
                })
            }
            1 => {
                if buf.len() < 8 {
                    return None;
                }
                buf = buf.get(8..)?;
                Some(PbField {
                    field,
                    wire,
                    data: &[],
                })
            }
            _ => None,
        }
    })
}

const MAX_VALUE_DEPTH: usize = 64;

/// `google.protobuf.Value` → JSON (bounded depth; malformed → `Null`).
pub fn decode_protobuf_value(buf: &[u8], depth: usize) -> Value {
    if depth >= MAX_VALUE_DEPTH {
        return Value::Null;
    }
    let Some((tag, rest)) = read_varint(buf) else {
        return Value::Null;
    };
    let ld = |rest: &[u8]| -> Option<Vec<u8>> {
        let (len, body) = read_varint(rest)?;
        body.get(..usize::try_from(len).ok()?).map(<[u8]>::to_vec)
    };
    match (tag >> 3, (tag & 7) as u8) {
        (1, 0) => Value::Null,
        (2, 1) => rest
            .get(..8)
            .and_then(|b| b.try_into().ok())
            .map(f64::from_le_bytes)
            .and_then(|f| {
                if f.fract() == 0.0 && f.abs() < 9.0e15 {
                    Some(Value::from(f as i64))
                } else {
                    serde_json::Number::from_f64(f).map(Value::Number)
                }
            })
            .unwrap_or(Value::Null),
        (3, 2) => ld(rest)
            .map(|s| Value::String(String::from_utf8_lossy(&s).into_owned()))
            .unwrap_or(Value::Null),
        (4, 0) => read_varint(rest)
            .map(|(v, _)| Value::Bool(v != 0))
            .unwrap_or(Value::Null),
        (5, 2) => ld(rest)
            .map(|body| {
                let mut map = serde_json::Map::new();
                for entry in iter_fields(&body) {
                    if entry.field != 1 || entry.wire != 2 {
                        continue;
                    }
                    let mut key = None;
                    let mut value = None;
                    for kv in iter_fields(entry.data) {
                        match (kv.field, kv.wire) {
                            (1, 2) => key = std::str::from_utf8(kv.data).ok().map(str::to_string),
                            (2, 2) => value = Some(kv.data),
                            _ => {}
                        }
                    }
                    if let (Some(k), Some(v)) = (key, value) {
                        map.insert(k, decode_protobuf_value(v, depth + 1));
                    }
                }
                Value::Object(map)
            })
            .unwrap_or(Value::Null),
        (6, 2) => ld(rest)
            .map(|body| {
                Value::Array(
                    iter_fields(&body)
                        .filter(|f| f.field == 1 && f.wire == 2)
                        .map(|f| decode_protobuf_value(f.data, depth + 1))
                        .collect(),
                )
            })
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}
