#![allow(non_camel_case_types, non_snake_case)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use cjson::*;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

fn fnv1a_bytes(data: &[u8]) -> u64 {
    let mut h: u64 = 14_695_981_039_346_656_037;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(1_099_511_628_211);
    }
    h ^= data.len() as u64;
    h = h.wrapping_mul(1_099_511_628_211);
    if h == 0 { 1 } else { h }
}

fn hash_cstr_ptr(ptr: *const c_char) -> u64 {
    if ptr.is_null() {
        return 0;
    }
    // SAFETY: `ptr` is non-null and NUL-terminated.
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    fnv1a_bytes(bytes)
}

fn with_cstr<R>(opt: Option<&[u8]>, f: impl FnOnce(*const c_char) -> R) -> R {
    match opt {
        None => f(ptr::null()),
        Some(bytes) if bytes.len() < 256 => {
            let mut stack = [0u8; 256];
            stack[..bytes.len()].copy_from_slice(bytes);
            f(stack.as_ptr().cast::<c_char>())
        }
        Some(bytes) => {
            let mut buf = Vec::with_capacity(bytes.len() + 1);
            buf.extend_from_slice(bytes);
            buf.push(0);
            f(buf.as_ptr().cast::<c_char>())
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum JVal {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(String),
    Arr(Vec<JVal>),
    Obj(Vec<(String, JVal)>),
}

impl JVal {
    fn as_opt_bytes(&self) -> Option<&[u8]> {
        match self {
            JVal::Null => None,
            JVal::Str(s) => Some(s.as_bytes()),
            _ => panic!("expected string or null, got {self:?}"),
        }
    }

    fn as_i32(&self) -> c_int {
        match *self {
            JVal::Int(i) => i as c_int,
            JVal::Uint(u) => u as c_int,
            JVal::Float(f) => f as c_int,
            _ => panic!("expected int, got {self:?}"),
        }
    }

    fn as_i64(&self) -> i64 {
        match *self {
            JVal::Int(i) => i,
            JVal::Uint(u) => u as i64,
            JVal::Float(f) => f as i64,
            _ => panic!("expected i64, got {self:?}"),
        }
    }

    fn as_u64(&self) -> u64 {
        match *self {
            JVal::Uint(u) => u,
            JVal::Int(i) => i as u64,
            _ => panic!("expected u64, got {self:?}"),
        }
    }

    fn as_f64(&self) -> f64 {
        match self {
            JVal::Float(f) => *f,
            JVal::Int(i) => *i as f64,
            JVal::Uint(u) => *u as f64,
            JVal::Obj(entries) if entries.len() == 1 && entries[0].0 == "$num" => {
                match &entries[0].1 {
                    JVal::Str(s) => match s.as_str() {
                        "NaN" => f64::NAN,
                        "Infinity" => f64::INFINITY,
                        "-Infinity" => f64::NEG_INFINITY,
                        "-0" => -0.0,
                        other => panic!("unknown $num marker: {other}"),
                    },
                    other => panic!("unexpected $num value: {other:?}"),
                }
            }
            _ => panic!("expected f64, got {self:?}"),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            JVal::Str(s) => s.as_str(),
            _ => panic!("expected string, got {self:?}"),
        }
    }

    fn as_arr(&self) -> &[JVal] {
        match self {
            JVal::Arr(v) => v.as_slice(),
            _ => panic!("expected array, got {self:?}"),
        }
    }

    fn field(&self, key: &str) -> Option<&JVal> {
        match self {
            JVal::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if b.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_val(&mut self) -> JVal {
        self.skip_ws();
        match self.bytes[self.pos] {
            b'n' => {
                assert!(self.bytes[self.pos..].starts_with(b"null"));
                self.pos += 4;
                JVal::Null
            }
            b't' => {
                assert!(self.bytes[self.pos..].starts_with(b"true"));
                self.pos += 4;
                JVal::Bool(true)
            }
            b'f' => {
                assert!(self.bytes[self.pos..].starts_with(b"false"));
                self.pos += 5;
                JVal::Bool(false)
            }
            b'"' => JVal::Str(self.parse_str()),
            b'[' => {
                self.pos += 1;
                self.skip_ws();
                let mut out = Vec::new();
                if self.bytes[self.pos] == b']' {
                    self.pos += 1;
                    return JVal::Arr(out);
                }
                loop {
                    out.push(self.parse_val());
                    self.skip_ws();
                    if self.bytes[self.pos] == b',' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                assert_eq!(self.bytes[self.pos], b']');
                self.pos += 1;
                JVal::Arr(out)
            }
            b'{' => {
                self.pos += 1;
                self.skip_ws();
                let mut out = Vec::new();
                if self.bytes[self.pos] == b'}' {
                    self.pos += 1;
                    return JVal::Obj(out);
                }
                loop {
                    self.skip_ws();
                    let k = self.parse_str();
                    self.skip_ws();
                    assert_eq!(self.bytes[self.pos], b':');
                    self.pos += 1;
                    let v = self.parse_val();
                    out.push((k, v));
                    self.skip_ws();
                    if self.bytes[self.pos] == b',' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                assert_eq!(self.bytes[self.pos], b'}');
                self.pos += 1;
                JVal::Obj(out)
            }
            _ => self.parse_num(),
        }
    }

    fn parse_str(&mut self) -> String {
        assert_eq!(self.bytes[self.pos], b'"');
        self.pos += 1;
        let mut out = String::new();
        while self.bytes[self.pos] != b'"' {
            let b = self.bytes[self.pos];
            if b != b'\\' {
                let ch = std::str::from_utf8(&self.bytes[self.pos..])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .expect("valid UTF-8 in fixture");
                out.push(ch);
                self.pos += ch.len_utf8();
                continue;
            }
            self.pos += 1;
            let esc = self.bytes[self.pos];
            self.pos += 1;
            match esc {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'b' => out.push('\x08'),
                b'f' => out.push('\x0c'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4]).unwrap();
                    let code = u32::from_str_radix(hex, 16).unwrap();
                    self.pos += 4;
                    if (0xD800..=0xDBFF).contains(&code) {
                        assert_eq!(&self.bytes[self.pos..self.pos + 2], b"\\u");
                        self.pos += 2;
                        let hex2 =
                            std::str::from_utf8(&self.bytes[self.pos..self.pos + 4]).unwrap();
                        let low = u32::from_str_radix(hex2, 16).unwrap();
                        self.pos += 4;
                        let cp = 0x10000 + (((code & 0x3FF) << 10) | (low & 0x3FF));
                        out.push(char::from_u32(cp).unwrap());
                    } else {
                        out.push(char::from_u32(code).unwrap());
                    }
                }
                other => panic!("unexpected escape: {other}"),
            }
        }
        self.pos += 1;
        out
    }

    fn parse_num(&mut self) -> JVal {
        let start = self.pos;
        let mut is_float = false;
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b'0'..=b'9' | b'-' | b'+') {
                self.pos += 1;
            } else if matches!(b, b'.' | b'e' | b'E') {
                is_float = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        if !is_float {
            if let Ok(v) = s.parse::<i64>() {
                return JVal::Int(v);
            }
            if let Ok(u) = s.parse::<u64>() {
                return JVal::Uint(u);
            }
        }
        JVal::Float(s.parse::<f64>().unwrap())
    }
}

static HOOK_LOCK: Mutex<()> = Mutex::new(());
static HOOK_ALLOCS: AtomicU64 = AtomicU64::new(0);
static HOOK_FREES: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn counting_malloc(sz: usize) -> *mut c_void {
    HOOK_ALLOCS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: Delegates directly to libc `malloc`.
    unsafe { malloc(sz) }
}

unsafe extern "C" fn counting_free(ptr: *mut c_void) {
    if !ptr.is_null() {
        HOOK_FREES.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: Delegates directly to libc `free`.
    unsafe { free(ptr) };
}

fn check_double(actual: f64, expected: f64) -> bool {
    if expected.is_nan() {
        actual.is_nan()
    } else if expected == 0.0 {
        actual == 0.0 && actual.is_sign_negative() == expected.is_sign_negative()
    } else {
        actual == expected
    }
}

#[test]
fn compat_2104_cases_match_c_abi() {
    let fixture = include_str!("fixtures/compat_cases.jsonl");
    let mut count = 0usize;

    for (line_no, line) in fixture.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        let mut p = JsonParser {
            bytes: line.as_bytes(),
            pos: 0,
        };
        let row = p.parse_val();
        let fn_name = row.field("fn").unwrap().as_str();
        let args = row.field("args").unwrap().as_arr();
        let expected = row.field("ok").unwrap();

        match fn_name {
            "parse_type" => {
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; node deleted immediately.
                    let root = cJSON_Parse(ptr);
                    if root.is_null() {
                        -1
                    } else {
                        let t = (*root).type_;
                        cJSON_Delete(root);
                        t
                    }
                });
                assert_eq!(got, expected.as_i32(), "line {}: {fn_name}", line_no + 1);
            }
            "parse_number_double" => {
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; node deleted immediately.
                    let root = cJSON_Parse(ptr);
                    let v = cJSON_GetNumberValue(root);
                    cJSON_Delete(root);
                    v
                });
                let exp = expected.as_f64();
                assert!(
                    check_double(got, exp),
                    "line {}: {fn_name} got {got:?} expected {exp:?}",
                    line_no + 1
                );
            }
            "parse_number_int" => {
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; node deleted immediately.
                    let root = cJSON_Parse(ptr);
                    if root.is_null() {
                        i32::MIN
                    } else {
                        let v = (*root).valueint;
                        cJSON_Delete(root);
                        v
                    }
                });
                assert_eq!(got, expected.as_i32(), "line {}: {fn_name}", line_no + 1);
            }
            "create_number_int" => {
                // SAFETY: `cJSON_CreateNumber` returns valid node or NULL.
                let got = unsafe {
                    let item = cJSON_CreateNumber(args[0].as_f64());
                    if item.is_null() {
                        0
                    } else {
                        let v = (*item).valueint;
                        cJSON_Delete(item);
                        v
                    }
                };
                assert_eq!(got, expected.as_i32(), "line {}: {fn_name}", line_no + 1);
            }
            "create_number_print_hash" => {
                // SAFETY: `cJSON_CreateNumber` and `cJSON_PrintUnformatted` results are freed.
                let got = unsafe {
                    let item = cJSON_CreateNumber(args[0].as_f64());
                    if item.is_null() {
                        0
                    } else {
                        let printed = cJSON_PrintUnformatted(item);
                        let h = hash_cstr_ptr(printed);
                        cJSON_free(printed.cast::<c_void>());
                        cJSON_Delete(item);
                        h
                    }
                };
                assert_eq!(got, expected.as_u64(), "line {}: {fn_name}", line_no + 1);
            }
            "parse_error_offset" => {
                let req = args[1].as_i32();
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; pointers stay within `ptr`.
                    let mut end: *const c_char = ptr::null();
                    if ptr.is_null() {
                        let _ = cJSON_ParseWithOpts(ptr, &raw mut end, req);
                        return -1i64;
                    }
                    let root = cJSON_ParseWithOpts(ptr, &raw mut end, req);
                    if !root.is_null() {
                        let off = if end.is_null() {
                            -2
                        } else {
                            end.offset_from(ptr) as i64
                        };
                        cJSON_Delete(root);
                        off
                    } else {
                        let err = cJSON_GetErrorPtr();
                        if err.is_null() {
                            -3
                        } else if !ptr::eq(end, err) {
                            -4
                        } else {
                            -(err.offset_from(ptr) as i64) - 10
                        }
                    }
                });
                assert_eq!(got, expected.as_i64(), "line {}: {fn_name}", line_no + 1);
            }
            "parse_length_error_offset" => {
                let opt_bytes = args[0].as_opt_bytes();
                let len = args[1].as_i32();
                let req = args[2].as_i32();
                let got = with_cstr(opt_bytes, |ptr| unsafe {
                    // SAFETY: `blen` is clamped to `bytes.len() + 1` so `ptr` is readable for `blen` bytes.
                    let mut blen = if len < 0 { 0 } else { len as usize };
                    if let Some(bytes) = opt_bytes
                        && len >= 0
                    {
                        blen = blen.min(bytes.len() + 1);
                    }
                    let mut end: *const c_char = ptr::null();
                    let root = cJSON_ParseWithLengthOpts(ptr, blen, &raw mut end, req);
                    if !root.is_null() {
                        let off = if !ptr.is_null() && !end.is_null() {
                            end.offset_from(ptr) as i64
                        } else {
                            -2
                        };
                        cJSON_Delete(root);
                        off
                    } else {
                        let err = cJSON_GetErrorPtr();
                        if ptr.is_null() || err.is_null() {
                            -1
                        } else if !ptr::eq(end, err) {
                            -4
                        } else {
                            -(err.offset_from(ptr) as i64) - 10
                        }
                    }
                });
                assert_eq!(got, expected.as_i64(), "line {}: {fn_name}", line_no + 1);
            }
            "print_hash" => {
                let fmt = args[1].as_i32();
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; all allocations freed.
                    let root = cJSON_Parse(ptr);
                    if root.is_null() {
                        return 0;
                    }
                    let out = if fmt != 0 {
                        cJSON_Print(root)
                    } else {
                        cJSON_PrintUnformatted(root)
                    };
                    let h = hash_cstr_ptr(out);
                    cJSON_free(out.cast::<c_void>());
                    cJSON_Delete(root);
                    h
                });
                assert_eq!(got, expected.as_u64(), "line {}: {fn_name}", line_no + 1);
            }
            "print_len" => {
                let fmt = args[1].as_i32();
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; all allocations freed.
                    let root = cJSON_Parse(ptr);
                    if root.is_null() {
                        return -1i64;
                    }
                    let out = if fmt != 0 {
                        cJSON_Print(root)
                    } else {
                        cJSON_PrintUnformatted(root)
                    };
                    let l = if out.is_null() {
                        -1
                    } else {
                        let len = CStr::from_ptr(out).to_bytes().len() as i64;
                        cJSON_free(out.cast::<c_void>());
                        len
                    };
                    cJSON_Delete(root);
                    l
                });
                assert_eq!(got, expected.as_i64(), "line {}: {fn_name}", line_no + 1);
            }
            "print_buffered_hash" => {
                let prebuf = args[1].as_i32();
                let fmt = args[2].as_i32();
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; all allocations freed.
                    let root = cJSON_Parse(ptr);
                    if root.is_null() {
                        return 0;
                    }
                    let out = cJSON_PrintBuffered(root, prebuf, fmt);
                    let h = hash_cstr_ptr(out);
                    cJSON_free(out.cast::<c_void>());
                    cJSON_Delete(root);
                    h
                });
                assert_eq!(got, expected.as_u64(), "line {}: {fn_name}", line_no + 1);
            }
            "print_preallocated_hash" => {
                let buf_len = args[1].as_i32();
                let fmt = args[2].as_i32();
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `buf` has `capped + 1` writable bytes when `buf_len >= 0`.
                    let root = cJSON_Parse(ptr);
                    if root.is_null() {
                        return 0;
                    }
                    if buf_len < 0 {
                        let ok = cJSON_PrintPreallocated(root, ptr::null_mut(), buf_len, fmt);
                        cJSON_Delete(root);
                        return u64::from(ok != 0);
                    }
                    let capped = buf_len.min(1_048_576);
                    let mut buf = vec![0u8; capped as usize + 1];
                    let ok = cJSON_PrintPreallocated(
                        root,
                        buf.as_mut_ptr().cast::<c_char>(),
                        capped,
                        fmt,
                    );
                    let h = if ok != 0 {
                        hash_cstr_ptr(buf.as_ptr().cast::<c_char>())
                    } else {
                        0
                    };
                    cJSON_Delete(root);
                    h
                });
                assert_eq!(got, expected.as_u64(), "line {}: {fn_name}", line_no + 1);
            }
            "string_value_hash" => {
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; node deleted immediately.
                    let root = cJSON_Parse(ptr);
                    let sv = cJSON_GetStringValue(root);
                    let h = hash_cstr_ptr(sv);
                    cJSON_Delete(root);
                    h
                });
                assert_eq!(got, expected.as_u64(), "line {}: {fn_name}", line_no + 1);
            }
            "minify_hash" => {
                let got = match args[0].as_opt_bytes() {
                    None => unsafe {
                        // SAFETY: `cJSON_Minify(NULL)` is a valid no-op.
                        cJSON_Minify(ptr::null_mut());
                        0
                    },
                    Some(bytes) => {
                        let mut buf = Vec::with_capacity(bytes.len() + 1);
                        buf.extend_from_slice(bytes);
                        buf.push(0);
                        // SAFETY: `buf` is a mutable NUL-terminated buffer.
                        unsafe { cJSON_Minify(buf.as_mut_ptr().cast::<c_char>()) };
                        hash_cstr_ptr(buf.as_ptr().cast::<c_char>())
                    }
                };
                assert_eq!(got, expected.as_u64(), "line {}: {fn_name}", line_no + 1);
            }
            "compare" => {
                let cs = args[2].as_i32();
                let got = with_cstr(args[0].as_opt_bytes(), |pa_str| {
                    with_cstr(args[1].as_opt_bytes(), |pb_str| unsafe {
                        // SAFETY: `pa_str` and `pb_str` are NULL or NUL-terminated.
                        let pa = cJSON_Parse(pa_str);
                        let pb = cJSON_Parse(pb_str);
                        let res = cJSON_Compare(pa, pb, cs);
                        cJSON_Delete(pa);
                        cJSON_Delete(pb);
                        res
                    })
                });
                assert_eq!(got, expected.as_i32(), "line {}: {fn_name}", line_no + 1);
            }
            "duplicate_hash" => {
                let recurse = args[1].as_i32();
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; all allocations freed.
                    let root = cJSON_Parse(ptr);
                    let dup = cJSON_Duplicate(root, recurse);
                    let mut h = 0u64;
                    if !dup.is_null() {
                        let out = cJSON_PrintUnformatted(dup);
                        h = hash_cstr_ptr(out);
                        cJSON_free(out.cast::<c_void>());
                        cJSON_Delete(dup);
                    }
                    cJSON_Delete(root);
                    h
                });
                assert_eq!(got, expected.as_u64(), "line {}: {fn_name}", line_no + 1);
            }
            "object_ops_hash" => {
                let cs = args[2].as_i32();
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| {
                    with_cstr(args[1].as_opt_bytes(), |kp| unsafe {
                        // SAFETY: `ptr` and `kp` are NULL or NUL-terminated; all allocations freed.
                        let root = cJSON_Parse(ptr);
                        let has = cJSON_HasObjectItem(root, kp);
                        if root.is_null() {
                            return 0;
                        }
                        let found = if cs != 0 {
                            cJSON_GetObjectItemCaseSensitive(root, kp)
                        } else {
                            cJSON_GetObjectItem(root, kp)
                        };
                        let mut h = 0u64;
                        if !found.is_null() {
                            let out = cJSON_PrintUnformatted(found);
                            h = hash_cstr_ptr(out);
                            cJSON_free(out.cast::<c_void>());
                        }
                        if cs != 0 {
                            cJSON_ReplaceItemInObjectCaseSensitive(
                                root,
                                kp,
                                cJSON_CreateNumber(42.5),
                            );
                        } else {
                            cJSON_ReplaceItemInObject(
                                root,
                                kp,
                                cJSON_CreateString(c"replaced".as_ptr()),
                            );
                        }
                        let after = cJSON_PrintUnformatted(root);
                        let ah = hash_cstr_ptr(after);
                        cJSON_free(after.cast::<c_void>());
                        cJSON_Delete(root);
                        let combined = h.wrapping_mul(1_315_423_911) ^ ah ^ u64::from(has != 0);
                        if combined == 0 { 1 } else { combined }
                    })
                });
                assert_eq!(got, expected.as_u64(), "line {}: {fn_name}", line_no + 1);
            }
            "array_ops_hash" => {
                let index = args[1].as_i32();
                let got = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; all allocations freed.
                    let root = cJSON_Parse(ptr);
                    let sz = cJSON_GetArraySize(root);
                    let item = cJSON_GetArrayItem(root, index);
                    let ih = if item.is_null() {
                        0
                    } else {
                        let item_out = cJSON_PrintUnformatted(item);
                        let h = hash_cstr_ptr(item_out);
                        cJSON_free(item_out.cast::<c_void>());
                        h
                    };
                    if cJSON_IsArray(root) != 0 {
                        cJSON_InsertItemInArray(root, index, cJSON_CreateBool(1));
                        cJSON_DeleteItemFromArray(root, index.saturating_add(1));
                    }
                    let after = cJSON_PrintUnformatted(root);
                    let ah = hash_cstr_ptr(after);
                    cJSON_free(after.cast::<c_void>());
                    cJSON_Delete(root);
                    let combined = ((sz as u32 as u64) << 32) ^ ih ^ ah;
                    if combined == 0 { 1 } else { combined }
                });
                assert_eq!(got, expected.as_u64(), "line {}: {fn_name}", line_no + 1);
            }
            "nesting_depth_ok" => {
                let depth = args[0].as_i32();
                let got = if !(0..=5000).contains(&depth) {
                    -1
                } else {
                    let d = depth as usize;
                    let mut buf = Vec::with_capacity(d * 2 + 1);
                    buf.resize(d, b'[');
                    buf.resize(d * 2, b']');
                    buf.push(0);
                    // SAFETY: `buf` is NUL-terminated.
                    unsafe {
                        let root = cJSON_Parse(buf.as_ptr().cast::<c_char>());
                        if root.is_null() {
                            0
                        } else {
                            cJSON_Delete(root);
                            1
                        }
                    }
                };
                assert_eq!(got, expected.as_i32(), "line {}: {fn_name}", line_no + 1);
            }
            "hooks_roundtrip" => {
                let _guard = HOOK_LOCK.lock().unwrap();
                HOOK_ALLOCS.store(0, Ordering::Relaxed);
                HOOK_FREES.store(0, Ordering::Relaxed);
                let mut hooks = cJSON_Hooks {
                    malloc_fn: Some(counting_malloc),
                    free_fn: Some(counting_free),
                };
                // SAFETY: `hooks` is valid and reset to default before dropping `_guard`.
                unsafe { cJSON_InitHooks(&raw mut hooks) };

                let (root_was_null, h) = with_cstr(args[0].as_opt_bytes(), |ptr| unsafe {
                    // SAFETY: `ptr` is NULL or NUL-terminated; all allocations freed before reset.
                    let root = cJSON_Parse(ptr);
                    if root.is_null() {
                        (true, 0u64)
                    } else {
                        let out = cJSON_PrintUnformatted(root);
                        let h = hash_cstr_ptr(out);
                        cJSON_free(out.cast::<c_void>());
                        cJSON_Delete(root);
                        (false, h)
                    }
                });

                // SAFETY: Restores default libc allocator hooks.
                unsafe { cJSON_InitHooks(ptr::null_mut()) };

                let allocs = HOOK_ALLOCS.load(Ordering::Relaxed);
                let frees = HOOK_FREES.load(Ordering::Relaxed);
                let balanced = u64::from(allocs == frees);
                let got = if allocs == 0 && root_was_null {
                    0
                } else {
                    (balanced << 63) | ((allocs & 0xFFFF) << 40) | (h & 0xFFFFFFFFFF)
                };
                assert_eq!(got, expected.as_u64(), "line {}: {fn_name}", line_no + 1);
            }
            other => panic!("unknown test function: {other}"),
        }
    }

    assert_eq!(count, 2104, "expected all 2104 compatibility cases");
}
