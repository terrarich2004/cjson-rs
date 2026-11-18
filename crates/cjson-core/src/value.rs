use std::fmt;

use crate::{
    CJSON_NESTING_LIMIT, case_insensitive_cmp, escaped_string_metrics, format_number,
    parse_json_string_cow, parse_number_slice, skip_utf8_bom, skip_whitespace,
    write_escaped_string,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    EmptyInput,
    UnexpectedToken,
    InvalidNumber,
    InvalidString,
    NestingLimitExceeded,
    TrailingData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub kind: ErrorKind,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self.kind {
            ErrorKind::EmptyInput => "empty input",
            ErrorKind::UnexpectedToken => "unexpected token",
            ErrorKind::InvalidNumber => "invalid number",
            ErrorKind::InvalidString => "invalid string or escape sequence",
            ErrorKind::NestingLimitExceeded => "nesting limit exceeded",
            ErrorKind::TrailingData => "trailing non-whitespace bytes",
        };
        write!(f, "{msg} at byte offset {}", self.offset)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        Self::parse_bytes(input.as_bytes())
    }

    pub fn parse_bytes(input: &[u8]) -> Result<Self, ParseError> {
        parse_bytes_with_opts(input, false)
    }

    pub fn parse_bytes_exact(input: &[u8]) -> Result<Self, ParseError> {
        parse_bytes_with_opts(input, true)
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Self::Bool(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::Number(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_i32(&self) -> Option<i32> {
        self.as_f64().map(crate::saturate_to_c_int)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Self::Array(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Self::Object(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn get_case_insensitive(&self, key: &str) -> Option<&Value> {
        let target = key.as_bytes();
        self.as_object()?
            .iter()
            .find(|(k, _)| case_insensitive_cmp(target, k.as_bytes()) == 0)
            .map(|(_, v)| v)
    }

    pub fn get_index(&self, index: usize) -> Option<&Value> {
        self.as_array()?.get(index)
    }

    pub fn to_string_compact(&self) -> String {
        let mut out = Vec::with_capacity(64);
        write_value(self, false, 0, &mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    pub fn to_string_pretty(&self) -> String {
        let mut out = Vec::with_capacity(128);
        write_value(self, true, 0, &mut out);
        String::from_utf8_lossy(&out).into_owned()
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_compact())
    }
}

pub fn parse(input: &str) -> Result<Value, ParseError> {
    Value::parse(input)
}

pub fn parse_bytes(input: &[u8]) -> Result<Value, ParseError> {
    Value::parse_bytes(input)
}

pub fn to_string(value: &Value) -> String {
    value.to_string_compact()
}

pub fn to_string_pretty(value: &Value) -> String {
    value.to_string_pretty()
}

fn parse_bytes_with_opts(input: &[u8], require_eof: bool) -> Result<Value, ParseError> {
    if input.is_empty() {
        return Err(ParseError {
            offset: 0,
            kind: ErrorKind::EmptyInput,
        });
    }
    let mut offset = skip_utf8_bom(input, 0);
    offset = skip_whitespace(input, offset);
    let val = parse_value(input, &mut offset, 0)?;
    if require_eof {
        while offset < input.len() && input[offset] <= 32 {
            offset += 1;
        }
        if offset < input.len() {
            return Err(ParseError {
                offset,
                kind: ErrorKind::TrailingData,
            });
        }
    }
    Ok(val)
}

fn parse_value(input: &[u8], offset: &mut usize, depth: usize) -> Result<Value, ParseError> {
    let rest = input.get(*offset..).unwrap_or(&[]);
    if rest.starts_with(b"null") {
        *offset += 4;
        return Ok(Value::Null);
    }
    if rest.starts_with(b"false") {
        *offset += 5;
        return Ok(Value::Bool(false));
    }
    if rest.starts_with(b"true") {
        *offset += 4;
        return Ok(Value::Bool(true));
    }
    match rest.first().copied() {
        Some(b'"') => parse_str_value(input, offset).map(Value::String),
        Some(b'-' | b'0'..=b'9') => {
            let Some((num, _, consumed)) = parse_number_slice(rest) else {
                return Err(ParseError {
                    offset: *offset,
                    kind: ErrorKind::InvalidNumber,
                });
            };
            *offset += consumed;
            Ok(Value::Number(num))
        }
        Some(b'[') => parse_array(input, offset, depth),
        Some(b'{') => parse_object(input, offset, depth),
        _ => Err(ParseError {
            offset: (*offset).min(input.len().saturating_sub(1)),
            kind: ErrorKind::UnexpectedToken,
        }),
    }
}

fn parse_str_value(input: &[u8], offset: &mut usize) -> Result<String, ParseError> {
    let rest = input.get(*offset..).unwrap_or(&[]);
    match parse_json_string_cow(rest) {
        Ok((bytes, consumed)) => {
            *offset += consumed;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }
        Err(rel) => Err(ParseError {
            offset: *offset + rel,
            kind: ErrorKind::InvalidString,
        }),
    }
}

fn parse_array(input: &[u8], offset: &mut usize, depth: usize) -> Result<Value, ParseError> {
    if depth >= CJSON_NESTING_LIMIT {
        return Err(ParseError {
            offset: *offset,
            kind: ErrorKind::NestingLimitExceeded,
        });
    }
    *offset += 1;
    *offset = skip_whitespace(input, *offset);
    if input.get(*offset) == Some(&b']') {
        *offset += 1;
        return Ok(Value::Array(Vec::new()));
    }
    if *offset >= input.len() {
        return Err(ParseError {
            offset: input.len().saturating_sub(1),
            kind: ErrorKind::UnexpectedToken,
        });
    }
    let mut items = Vec::new();
    *offset -= 1;
    loop {
        *offset += 1;
        *offset = skip_whitespace(input, *offset);
        items.push(parse_value(input, offset, depth + 1)?);
        *offset = skip_whitespace(input, *offset);
        if input.get(*offset) != Some(&b',') {
            break;
        }
    }
    if input.get(*offset) != Some(&b']') {
        return Err(ParseError {
            offset: (*offset).min(input.len().saturating_sub(1)),
            kind: ErrorKind::UnexpectedToken,
        });
    }
    *offset += 1;
    Ok(Value::Array(items))
}

fn parse_object(input: &[u8], offset: &mut usize, depth: usize) -> Result<Value, ParseError> {
    if depth >= CJSON_NESTING_LIMIT {
        return Err(ParseError {
            offset: *offset,
            kind: ErrorKind::NestingLimitExceeded,
        });
    }
    *offset += 1;
    *offset = skip_whitespace(input, *offset);
    if input.get(*offset) == Some(&b'}') {
        *offset += 1;
        return Ok(Value::Object(Vec::new()));
    }
    if *offset >= input.len() {
        return Err(ParseError {
            offset: input.len().saturating_sub(1),
            kind: ErrorKind::UnexpectedToken,
        });
    }
    let mut entries = Vec::new();
    *offset -= 1;
    loop {
        if *offset + 1 >= input.len() {
            return Err(ParseError {
                offset: input.len().saturating_sub(1),
                kind: ErrorKind::UnexpectedToken,
            });
        }
        *offset += 1;
        *offset = skip_whitespace(input, *offset);
        let key = parse_str_value(input, offset)?;
        *offset = skip_whitespace(input, *offset);
        if input.get(*offset) != Some(&b':') {
            return Err(ParseError {
                offset: (*offset).min(input.len().saturating_sub(1)),
                kind: ErrorKind::UnexpectedToken,
            });
        }
        *offset += 1;
        *offset = skip_whitespace(input, *offset);
        let val = parse_value(input, offset, depth + 1)?;
        entries.push((key, val));
        *offset = skip_whitespace(input, *offset);
        if input.get(*offset) != Some(&b',') {
            break;
        }
    }
    if input.get(*offset) != Some(&b'}') {
        return Err(ParseError {
            offset: (*offset).min(input.len().saturating_sub(1)),
            kind: ErrorKind::UnexpectedToken,
        });
    }
    *offset += 1;
    Ok(Value::Object(entries))
}

fn write_escaped(bytes: &[u8], out: &mut Vec<u8>) {
    let (content_len, escape_chars) = escaped_string_metrics(bytes);
    let needed = content_len + 2;
    let start = out.len();
    out.resize(start + needed, 0);
    write_escaped_string(bytes, escape_chars, &mut out[start..]);
}

fn write_value(val: &Value, pretty: bool, depth: usize, out: &mut Vec<u8>) {
    match val {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Number(d) => {
            let vi = crate::saturate_to_c_int(*d);
            let s = format_number(*d, vi);
            out.extend_from_slice(s.as_bytes());
        }
        Value::String(s) => write_escaped(s.as_bytes(), out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                    if pretty {
                        out.push(b' ');
                    }
                }
                write_value(item, pretty, depth + 1, out);
            }
            out.push(b']');
        }
        Value::Object(entries) => {
            out.push(b'{');
            if pretty {
                out.push(b'\n');
            }
            let child_depth = depth + 1;
            for (i, (k, v)) in entries.iter().enumerate() {
                if pretty {
                    out.extend(std::iter::repeat_n(b'\t', child_depth));
                }
                write_escaped(k.as_bytes(), out);
                out.push(b':');
                if pretty {
                    out.push(b'\t');
                }
                write_value(v, pretty, child_depth, out);
                if i + 1 < entries.len() {
                    out.push(b',');
                }
                if pretty {
                    out.push(b'\n');
                }
            }
            if pretty {
                out.extend(std::iter::repeat_n(b'\t', depth));
            }
            out.push(b'}');
        }
    }
}
