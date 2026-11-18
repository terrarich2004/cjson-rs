//! Pure safe Rust JSON parser, serializer, and `cJSON`-compatible codecs.

#![forbid(unsafe_code)]

mod value;

use std::borrow::Cow;
use std::fmt::Write as _;

pub use value::{ErrorKind, ParseError, Value, parse, parse_bytes, to_string, to_string_pretty};

pub const CJSON_VERSION: &str = "1.7.19";

pub const CJSON_INVALID: i32 = 0;
pub const CJSON_FALSE: i32 = 1 << 0;
pub const CJSON_TRUE: i32 = 1 << 1;
pub const CJSON_NULL: i32 = 1 << 2;
pub const CJSON_NUMBER: i32 = 1 << 3;
pub const CJSON_STRING: i32 = 1 << 4;
pub const CJSON_ARRAY: i32 = 1 << 5;
pub const CJSON_OBJECT: i32 = 1 << 6;
pub const CJSON_RAW: i32 = 1 << 7;

pub const CJSON_IS_REFERENCE: i32 = 256;
pub const CJSON_STRING_IS_CONST: i32 = 512;

pub const CJSON_NESTING_LIMIT: usize = 1000;
pub const CJSON_CIRCULAR_LIMIT: usize = 10000;

/// Saturate an `f64` into C `int` (`i32`), matching x86_64 `(int)NAN == INT_MIN`.
pub fn saturate_to_c_int(number: f64) -> i32 {
    if number.is_nan() || number <= i32::MIN as f64 {
        i32::MIN
    } else if number >= i32::MAX as f64 {
        i32::MAX
    } else {
        number as i32
    }
}

/// Compare two `f64` values within `f64::EPSILON` relative to the larger magnitude.
pub fn compare_double(a: f64, b: f64) -> bool {
    let max_val = a.abs().max(b.abs());
    (a - b).abs() <= max_val * f64::EPSILON
}

/// ASCII case-insensitive byte-slice comparison in the `"C"` locale.
pub fn case_insensitive_cmp(a: &[u8], b: &[u8]) -> i32 {
    let min_len = a.len().min(b.len());
    for i in 0..min_len {
        let ca = a[i].to_ascii_lowercase();
        let cb = b[i].to_ascii_lowercase();
        if ca != cb {
            return i32::from(ca) - i32::from(cb);
        }
    }
    let ca = a.get(min_len).copied().unwrap_or(0).to_ascii_lowercase();
    let cb = b.get(min_len).copied().unwrap_or(0).to_ascii_lowercase();
    i32::from(ca) - i32::from(cb)
}

/// Skip a leading UTF-8 BOM (`\xEF\xBB\xBF`) at offset 0 when `content.len() > 4`.
pub fn skip_utf8_bom(content: &[u8], offset: usize) -> usize {
    if offset == 0 && content.len() > 4 && content.get(0..3) == Some(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        offset
    }
}

/// Advance past ASCII control/whitespace bytes (`<= 32`), backing up by 1 at end-of-buffer.
pub fn skip_whitespace(content: &[u8], mut offset: usize) -> usize {
    if offset >= content.len() {
        return offset;
    }
    while offset < content.len() && content[offset] <= 32 {
        offset += 1;
    }
    if offset == content.len() && !content.is_empty() {
        offset -= 1;
    }
    offset
}

fn strtod_prefix_len(s: &[u8]) -> usize {
    let mut i = 0;
    if matches!(s.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let mut digits = 0usize;
    while matches!(s.get(i), Some(b'0'..=b'9')) {
        i += 1;
        digits += 1;
    }
    if s.get(i) == Some(&b'.') {
        i += 1;
        while matches!(s.get(i), Some(b'0'..=b'9')) {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return 0;
    }
    if matches!(s.get(i), Some(b'e' | b'E')) {
        let mut exp_i = i + 1;
        if matches!(s.get(exp_i), Some(b'+' | b'-')) {
            exp_i += 1;
        }
        let mut exp_digits = 0usize;
        while matches!(s.get(exp_i), Some(b'0'..=b'9')) {
            exp_i += 1;
            exp_digits += 1;
        }
        if exp_digits > 0 {
            i = exp_i;
        }
    }
    i
}

/// Parse a numeric prefix from `input`, returning `(valuedouble, valueint, bytes_consumed)`.
pub fn parse_number_slice(input: &[u8]) -> Option<(f64, i32, usize)> {
    let mut scan_len = 0usize;
    while let Some(&b) = input.get(scan_len) {
        if matches!(b, b'0'..=b'9' | b'+' | b'-' | b'e' | b'E' | b'.') {
            scan_len += 1;
        } else {
            break;
        }
    }
    let prefix_len = strtod_prefix_len(input.get(..scan_len)?);
    if prefix_len == 0 {
        return None;
    }
    let prefix_str = std::str::from_utf8(input.get(..prefix_len)?).ok()?;
    let number = prefix_str.parse::<f64>().ok()?;
    let valueint = saturate_to_c_int(number);
    Some((number, valueint, prefix_len))
}

fn format_c_g(d: f64, prec: usize) -> String {
    if d == 0.0 {
        return if d.is_sign_negative() {
            "-0".to_string()
        } else {
            "0".to_string()
        };
    }

    let neg = d.is_sign_negative();
    let abs = d.abs();
    let p_frac = prec.saturating_sub(1);
    let sci = format!("{abs:.p_frac$e}");
    let (mantissa_part, exp_part) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp_part.parse().unwrap_or(0);

    let mut digits: Vec<u8> = mantissa_part
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .collect();
    while digits.len() > 1 && digits.last() == Some(&b'0') {
        digits.pop();
    }

    let mut out = String::with_capacity(32);
    if neg {
        out.push('-');
    }

    let p = prec as i32;
    if exp >= -4 && exp < p {
        if exp < 0 {
            out.push_str("0.");
            let zeros = (-exp - 1) as usize;
            for _ in 0..zeros {
                out.push('0');
            }
            for &b in &digits {
                out.push(b as char);
            }
        } else {
            let int_digits = (exp + 1) as usize;
            if digits.len() <= int_digits {
                for &b in &digits {
                    out.push(b as char);
                }
                for _ in 0..(int_digits - digits.len()) {
                    out.push('0');
                }
            } else {
                for &b in &digits[..int_digits] {
                    out.push(b as char);
                }
                out.push('.');
                for &b in &digits[int_digits..] {
                    out.push(b as char);
                }
            }
        }
    } else {
        if let Some(&first) = digits.first() {
            out.push(first as char);
        }
        if digits.len() > 1 {
            out.push('.');
            for &b in &digits[1..] {
                out.push(b as char);
            }
        }
        let exp_sign = if exp < 0 { '-' } else { '+' };
        let exp_abs = exp.unsigned_abs();
        let _ = write!(out, "e{exp_sign}{exp_abs:02}");
    }

    out
}

fn write_i32_ascii(val: i32, dst: &mut [u8; 32]) -> usize {
    let mut tmp = [0u8; 16];
    let mut pos = tmp.len();
    let neg = val < 0;
    let mut mag = val.unsigned_abs();
    if mag == 0 {
        pos -= 1;
        tmp[pos] = b'0';
    } else {
        while mag > 0 {
            pos -= 1;
            tmp[pos] = b'0' + (mag % 10) as u8;
            mag /= 10;
        }
    }
    if neg {
        pos -= 1;
        tmp[pos] = b'-';
    }
    let len = tmp.len() - pos;
    dst[..len].copy_from_slice(&tmp[pos..]);
    len
}

/// Format a number into a 32-byte stack buffer, returning the byte length.
pub fn format_number_buf(d: f64, valueint: i32, dst: &mut [u8; 32]) -> usize {
    if d.is_nan() || d.is_infinite() {
        dst[..4].copy_from_slice(b"null");
        return 4;
    }
    if d == f64::from(valueint) {
        return write_i32_ascii(valueint, dst);
    }
    let s15 = format_c_g(d, 15);
    let recovered_ok = s15
        .parse::<f64>()
        .map(|test| compare_double(test, d))
        .unwrap_or(false);
    let s = if recovered_ok { s15 } else { format_c_g(d, 17) };
    let bytes = s.as_bytes();
    let len = bytes.len().min(dst.len());
    dst[..len].copy_from_slice(&bytes[..len]);
    len
}

/// Format a number using `cJSON` semantics (`NaN`/`Inf` -> `"null"`, `-0.0` -> `"0"`).
pub fn format_number(d: f64, valueint: i32) -> String {
    let mut buf = [0u8; 32];
    let len = format_number_buf(d, valueint, &mut buf);
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// Parse 4 hex digits into an unsigned integer, returning `0` on invalid digits.
pub fn parse_hex4(input: &[u8]) -> u32 {
    if input.len() < 4 {
        return 0;
    }
    let mut h: u32 = 0;
    for (i, &b) in input.iter().take(4).enumerate() {
        let nibble = match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            b'A'..=b'F' => 10 + u32::from(b - b'A'),
            b'a'..=b'f' => 10 + u32::from(b - b'a'),
            _ => return 0,
        };
        h += nibble;
        if i < 3 {
            h <<= 4;
        }
    }
    h
}

/// Convert `\uXXXX` (including UTF-16 surrogate pairs) into UTF-8 bytes.
pub fn utf16_literal_to_utf8(input: &[u8]) -> Option<(usize, [u8; 4], usize)> {
    if input.len() < 6 {
        return None;
    }
    let first_code = parse_hex4(input.get(2..6)?);
    if (0xDC00..=0xDFFF).contains(&first_code) {
        return None;
    }

    let (sequence_len, mut codepoint) = if (0xD800..=0xDBFF).contains(&first_code) {
        let second = input.get(6..12)?;
        if second.get(0..2) != Some(b"\\u") {
            return None;
        }
        let second_code = parse_hex4(second.get(2..6)?);
        if !(0xDC00..=0xDFFF).contains(&second_code) {
            return None;
        }
        let cp = 0x10000 + (((first_code & 0x3FF) << 10) | (second_code & 0x3FF));
        (12usize, cp)
    } else {
        (6usize, first_code)
    };

    let (utf8_len, first_byte_mark) = if codepoint < 0x80 {
        (1usize, 0u8)
    } else if codepoint < 0x800 {
        (2usize, 0xC0u8)
    } else if codepoint < 0x10000 {
        (3usize, 0xE0u8)
    } else if codepoint <= 0x10_FFFF {
        (4usize, 0xF0u8)
    } else {
        return None;
    };

    let mut out = [0u8; 4];
    for pos in (1..utf8_len).rev() {
        out[pos] = ((codepoint | 0x80) & 0xBF) as u8;
        codepoint >>= 6;
    }
    out[0] = if utf8_len > 1 {
        ((codepoint | u32::from(first_byte_mark)) & 0xFF) as u8
    } else {
        (codepoint & 0x7F) as u8
    };

    Some((sequence_len, out, utf8_len))
}

/// Parse a JSON string literal starting at `input[0] == b'"'`, borrowing when unescaped.
pub fn parse_json_string_cow(input: &[u8]) -> Result<(Cow<'_, [u8]>, usize), usize> {
    if input.first() != Some(&b'"') {
        return Err(1);
    }

    let mut end = 1usize;
    let mut skipped_bytes = 0usize;
    while end < input.len() && input[end] != b'"' {
        if input[end] == b'\\' {
            if end + 1 >= input.len() {
                return Err(1);
            }
            skipped_bytes += 1;
            end += 1;
        }
        end += 1;
    }

    if end >= input.len() || input[end] != b'"' {
        return Err(1);
    }

    if skipped_bytes == 0 {
        return Ok((Cow::Borrowed(&input[1..end]), end + 1));
    }

    let cap = end.saturating_sub(skipped_bytes);
    let mut out = Vec::with_capacity(cap);
    let mut pos = 1usize;

    while pos < end {
        let b = input[pos];
        if b != b'\\' {
            out.push(b);
            pos += 1;
            continue;
        }

        let esc = *input.get(pos + 1).ok_or(pos)?;
        let seq_len = match esc {
            b'b' => {
                out.push(0x08);
                2
            }
            b'f' => {
                out.push(0x0c);
                2
            }
            b'n' => {
                out.push(b'\n');
                2
            }
            b'r' => {
                out.push(b'\r');
                2
            }
            b't' => {
                out.push(b'\t');
                2
            }
            b'"' | b'\\' | b'/' => {
                out.push(esc);
                2
            }
            b'u' => {
                let slice = input.get(pos..end).ok_or(pos)?;
                let (slen, utf8, ulen) = utf16_literal_to_utf8(slice).ok_or(pos)?;
                if let Some(chunk) = utf8.get(..ulen) {
                    out.extend_from_slice(chunk);
                }
                slen
            }
            _ => return Err(pos),
        };
        pos += seq_len;
    }

    Ok((Cow::Owned(out), end + 1))
}

/// Parse a JSON string literal starting at `input[0] == b'"'`.
pub fn parse_json_string(input: &[u8]) -> Result<(Vec<u8>, usize), usize> {
    parse_json_string_cow(input).map(|(cow, next)| (cow.into_owned(), next))
}

/// Compute `(escaped_content_len, escape_count)` excluding surrounding quotes.
pub fn escaped_string_metrics(bytes: &[u8]) -> (usize, usize) {
    let mut escape_chars = 0usize;
    for &b in bytes {
        match b {
            b'"' | b'\\' | 0x08 | 0x0c | b'\n' | b'\r' | b'\t' => escape_chars += 1,
            b if b < 32 => escape_chars += 5,
            _ => {}
        }
    }
    (bytes.len() + escape_chars, escape_chars)
}

/// Write an escaped JSON string literal (with surrounding quotes) into `dst`.
pub fn write_escaped_string(bytes: &[u8], escape_chars: usize, dst: &mut [u8]) -> usize {
    let needed = bytes.len() + escape_chars + 2;
    if dst.len() < needed {
        return 0;
    }
    dst[0] = b'"';
    if escape_chars == 0 {
        dst[1..1 + bytes.len()].copy_from_slice(bytes);
    } else {
        let mut pos = 1usize;
        for &b in bytes {
            if b > 31 && b != b'"' && b != b'\\' {
                dst[pos] = b;
                pos += 1;
            } else {
                dst[pos] = b'\\';
                pos += 1;
                match b {
                    b'\\' => {
                        dst[pos] = b'\\';
                        pos += 1;
                    }
                    b'"' => {
                        dst[pos] = b'"';
                        pos += 1;
                    }
                    0x08 => {
                        dst[pos] = b'b';
                        pos += 1;
                    }
                    0x0c => {
                        dst[pos] = b'f';
                        pos += 1;
                    }
                    b'\n' => {
                        dst[pos] = b'n';
                        pos += 1;
                    }
                    b'\r' => {
                        dst[pos] = b'r';
                        pos += 1;
                    }
                    b'\t' => {
                        dst[pos] = b't';
                        pos += 1;
                    }
                    other => {
                        const HEX: &[u8; 16] = b"0123456789abcdef";
                        dst[pos] = b'u';
                        dst[pos + 1] = b'0';
                        dst[pos + 2] = b'0';
                        dst[pos + 3] = HEX[(other >> 4) as usize & 0xF];
                        dst[pos + 4] = HEX[(other & 0xF) as usize];
                        pos += 5;
                    }
                }
            }
        }
    }
    dst[needed - 1] = b'"';
    needed
}

/// Render a byte slice as an escaped JSON string literal.
pub fn format_json_string(input: Option<&[u8]>) -> Vec<u8> {
    let Some(bytes) = input else {
        return b"\"\"".to_vec();
    };
    let (content_len, escape_chars) = escaped_string_metrics(bytes);
    let mut out = vec![0u8; content_len + 2];
    write_escaped_string(bytes, escape_chars, &mut out);
    out
}

/// Strip whitespace and `//` / `/* */` comments from a mutable JSON buffer in-place.
pub fn minify_bytes(buf: &mut [u8]) -> usize {
    let len = buf.len();
    let mut i = 0usize;
    let mut into = 0usize;

    while i < len && buf[i] != 0 {
        match buf[i] {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'/' => {
                if buf.get(i + 1) == Some(&b'/') {
                    i += 2;
                    while i < len && buf[i] != 0 {
                        if buf[i] == b'\n' {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                } else if buf.get(i + 1) == Some(&b'*') {
                    i += 2;
                    while i < len && buf[i] != 0 {
                        if buf[i] == b'*' && buf.get(i + 1) == Some(&b'/') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            b'"' => {
                buf[into] = buf[i];
                i += 1;
                into += 1;
                while i < len && buf[i] != 0 {
                    let cur = buf[i];
                    buf[into] = cur;
                    if cur == b'"' {
                        buf[into] = b'"';
                        i += 1;
                        into += 1;
                        break;
                    } else if cur == b'\\' && buf.get(i + 1) == Some(&b'"') {
                        if into + 1 < len {
                            buf[into + 1] = b'"';
                        }
                        i += 1;
                        into += 1;
                    }
                    i += 1;
                    into += 1;
                }
            }
            other => {
                buf[into] = other;
                i += 1;
                into += 1;
            }
        }
    }

    into
}

/// Strip whitespace and `//` / `/* */` comments from a JSON string.
pub fn minify(input: &str) -> String {
    let mut bytes = input.as_bytes().to_vec();
    let new_len = minify_bytes(&mut bytes);
    bytes.truncate(new_len);
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_roundtrip_and_edge_cases() {
        assert_eq!(
            parse_number_slice(b"-0").map(|(d, i, _)| (d.is_sign_negative(), i)),
            Some((true, 0))
        );
        assert_eq!(format_number(-0.0, 0), "0");
        assert_eq!(format_number(f64::NAN, 0), "null");
        assert_eq!(format_number(f64::INFINITY, 0), "null");
        assert_eq!(format_number(0.1 + 0.2, 0), "0.3");
        assert_eq!(format_number(1e-5, 0), "1e-05");
        assert_eq!(format_number(1e-4, 0), "0.0001");
    }

    #[test]
    fn utf16_surrogates() {
        let res = parse_json_string(br#""\uD83D\uDE00""#);
        assert_eq!(res, Ok(("😀".as_bytes().to_vec(), 14)));
        assert!(parse_json_string(br#""\uD83D""#).is_err());
        assert!(parse_json_string(br#""\uDE00""#).is_err());
    }

    #[test]
    fn high_level_value_api() {
        let v = parse(r#"{"name":"cjson","nums":[1, 2.5, true, null]}"#).expect("valid json");
        assert_eq!(v.get("name").and_then(Value::as_str), Some("cjson"));
        assert_eq!(
            v.get("nums")
                .and_then(|a| a.get_index(1))
                .and_then(Value::as_f64),
            Some(2.5)
        );
        assert_eq!(minify("{ \"a\" : 1 } // c\n"), "{\"a\":1}");
        assert!(parse("[1, 2").is_err());
    }
}
