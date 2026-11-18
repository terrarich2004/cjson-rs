use std::ffi::{CStr, c_char, c_void};
use std::ptr;

use cjson_core::{
    CJSON_ARRAY, CJSON_FALSE, CJSON_NESTING_LIMIT, CJSON_NULL, CJSON_NUMBER, CJSON_OBJECT,
    CJSON_RAW, CJSON_STRING, CJSON_TRUE, escaped_string_metrics, format_number_buf,
    write_escaped_string,
};

use crate::hooks::{InternalHooks, cJSON, cJSON_bool, current_hooks};

struct PrintBuffer {
    buffer: *mut u8,
    length: usize,
    offset: usize,
    depth: usize,
    noalloc: bool,
    format: bool,
    hooks: InternalHooks,
}

fn ensure(p: &mut PrintBuffer, needed: usize) -> *mut u8 {
    if p.buffer.is_null() {
        return ptr::null_mut();
    }
    if p.length > 0 && p.offset >= p.length {
        return ptr::null_mut();
    }
    if needed > i32::MAX as usize {
        return ptr::null_mut();
    }

    let Some(total) = p.offset.checked_add(1).and_then(|v| v.checked_add(needed)) else {
        return ptr::null_mut();
    };
    if total <= p.length {
        // SAFETY: `p.offset < total <= p.length`, so `p.buffer.add(p.offset)` is in bounds.
        return unsafe { p.buffer.add(p.offset) };
    }
    if p.noalloc {
        return ptr::null_mut();
    }

    let newsize = if total > (i32::MAX as usize / 2) {
        if total <= i32::MAX as usize {
            i32::MAX as usize
        } else {
            return ptr::null_mut();
        }
    } else {
        total * 2
    };

    let newbuf = if let Some(reallocate) = p.hooks.reallocate {
        // SAFETY: `p.buffer` was allocated by `p.hooks.allocate` and `newsize > 0`.
        let nb = unsafe { reallocate(p.buffer.cast::<c_void>(), newsize) }.cast::<u8>();
        if nb.is_null() {
            // SAFETY: `reallocate` failed so `p.buffer` is still valid to free.
            unsafe { (p.hooks.deallocate)(p.buffer.cast::<c_void>()) };
            p.length = 0;
            p.buffer = ptr::null_mut();
            return ptr::null_mut();
        }
        nb
    } else {
        // SAFETY: `p.hooks.allocate` is a valid C allocator function.
        let nb = unsafe { (p.hooks.allocate)(newsize) }.cast::<u8>();
        if nb.is_null() {
            // SAFETY: `p.buffer` is valid to free on allocation failure.
            unsafe { (p.hooks.deallocate)(p.buffer.cast::<c_void>()) };
            p.length = 0;
            p.buffer = ptr::null_mut();
            return ptr::null_mut();
        }
        // SAFETY: `p.offset + 1 <= p.length` and `nb` has `newsize >= total > p.offset + 1` bytes.
        unsafe {
            ptr::copy_nonoverlapping(p.buffer, nb, p.offset + 1);
            (p.hooks.deallocate)(p.buffer.cast::<c_void>());
        }
        nb
    };

    p.length = newsize;
    p.buffer = newbuf;
    // SAFETY: `p.offset < newsize` so offset is within `newbuf`.
    unsafe { newbuf.add(p.offset) }
}

fn update_offset(p: &mut PrintBuffer) {
    if p.buffer.is_null() {
        return;
    }
    // SAFETY: All printers write a NUL-terminated string starting at `p.buffer + p.offset`.
    let len = unsafe { CStr::from_ptr(p.buffer.add(p.offset).cast::<c_char>()) }
        .to_bytes()
        .len();
    p.offset += len;
}

fn print_number(item: *const cJSON, p: &mut PrintBuffer) -> bool {
    // SAFETY: Caller ensures `item` is non-null.
    let (d, valueint) = unsafe { ((*item).valuedouble, (*item).valueint) };
    let mut num_buf = [0u8; 32];
    let len = format_number_buf(d, valueint, &mut num_buf);
    let out = ensure(p, len + 1);
    if out.is_null() {
        return false;
    }
    // SAFETY: `ensure` reserved `len + 1` writable bytes at `out`.
    unsafe {
        ptr::copy_nonoverlapping(num_buf.as_ptr(), out, len);
        *out.add(len) = 0;
    }
    p.offset += len;
    true
}

fn print_string_ptr(input: *const c_char, p: &mut PrintBuffer) -> bool {
    if input.is_null() {
        return write_literal(p, b"\"\"\0");
    }
    // SAFETY: `input` is a valid NUL-terminated C string (NULL checked above).
    let slice = unsafe { CStr::from_ptr(input) }.to_bytes();
    let (content_len, escape_chars) = escaped_string_metrics(slice);
    let total_with_quotes = content_len + 2;
    let out = ensure(p, total_with_quotes + 1);
    if out.is_null() {
        return false;
    }
    // SAFETY: `ensure` reserved `total_with_quotes + 1` writable bytes at `out`.
    let dst = unsafe { std::slice::from_raw_parts_mut(out, total_with_quotes + 1) };
    write_escaped_string(slice, escape_chars, dst);
    dst[total_with_quotes] = 0;
    true
}

fn print_array(item: *const cJSON, p: &mut PrintBuffer) -> bool {
    if p.depth >= CJSON_NESTING_LIMIT {
        return false;
    }
    let out = ensure(p, 1);
    if out.is_null() {
        return false;
    }
    // SAFETY: `ensure(p, 1)` guarantees at least 2 bytes (1 + NUL) at `out`.
    unsafe { *out = b'[' };
    p.offset += 1;
    p.depth += 1;

    // SAFETY: Caller ensures `item` is non-null.
    let mut current = unsafe { (*item).child };
    while !current.is_null() {
        if !print_value(current, p) {
            return false;
        }
        update_offset(p);
        // SAFETY: `current` is a valid non-null node in the array child chain.
        let next = unsafe { (*current).next };
        if !next.is_null() {
            let sep_len = if p.format { 2 } else { 1 };
            let sep = ensure(p, sep_len + 1);
            if sep.is_null() {
                return false;
            }
            // SAFETY: `ensure` reserved `sep_len + 1` bytes at `sep`.
            unsafe {
                *sep = b',';
                if p.format {
                    *sep.add(1) = b' ';
                    *sep.add(2) = 0;
                } else {
                    *sep.add(1) = 0;
                }
            }
            p.offset += sep_len;
        }
        current = next;
    }

    let close = ensure(p, 2);
    if close.is_null() {
        return false;
    }
    // SAFETY: `ensure(p, 2)` reserved at least 2 bytes at `close`.
    unsafe {
        *close = b']';
        *close.add(1) = 0;
    }
    p.depth -= 1;
    true
}

fn print_object(item: *const cJSON, p: &mut PrintBuffer) -> bool {
    if p.depth >= CJSON_NESTING_LIMIT {
        return false;
    }
    let open_len = if p.format { 2 } else { 1 };
    let open = ensure(p, open_len + 1);
    if open.is_null() {
        return false;
    }
    // SAFETY: `ensure` reserved `open_len + 1` bytes at `open`.
    unsafe {
        *open = b'{';
        if p.format {
            *open.add(1) = b'\n';
        }
    }
    p.depth += 1;
    p.offset += open_len;

    // SAFETY: Caller ensures `item` is non-null.
    let mut current = unsafe { (*item).child };
    while !current.is_null() {
        if p.format {
            let tabs = ensure(p, p.depth);
            if tabs.is_null() {
                return false;
            }
            // SAFETY: `ensure` reserved `p.depth` bytes at `tabs`.
            unsafe { ptr::write_bytes(tabs, b'\t', p.depth) };
            p.offset += p.depth;
        }

        // SAFETY: `current` is a valid non-null node in the object child chain.
        let key_ptr = unsafe { (*current).string };
        if !print_string_ptr(key_ptr, p) {
            return false;
        }
        update_offset(p);

        let colon_len = if p.format { 2 } else { 1 };
        let colon = ensure(p, colon_len);
        if colon.is_null() {
            return false;
        }
        // SAFETY: `ensure` reserved `colon_len` bytes at `colon`.
        unsafe {
            *colon = b':';
            if p.format {
                *colon.add(1) = b'\t';
            }
        }
        p.offset += colon_len;

        if !print_value(current, p) {
            return false;
        }
        update_offset(p);

        // SAFETY: `current` is a valid non-null node in the object child chain.
        let next = unsafe { (*current).next };
        let tail_len = usize::from(p.format) + usize::from(!next.is_null());
        let tail = ensure(p, tail_len + 1);
        if tail.is_null() {
            return false;
        }
        // SAFETY: `ensure` reserved `tail_len + 1` bytes at `tail`.
        unsafe {
            let mut idx = 0usize;
            if !next.is_null() {
                *tail.add(idx) = b',';
                idx += 1;
            }
            if p.format {
                *tail.add(idx) = b'\n';
                idx += 1;
            }
            *tail.add(idx) = 0;
        }
        p.offset += tail_len;

        current = next;
    }

    let close_need = if p.format { p.depth + 1 } else { 2 };
    let close = ensure(p, close_need);
    if close.is_null() {
        return false;
    }
    // SAFETY: `ensure` reserved `close_need` bytes at `close`.
    unsafe {
        let mut idx = 0usize;
        if p.format {
            let indent = p.depth - 1;
            ptr::write_bytes(close, b'\t', indent);
            idx = indent;
        }
        *close.add(idx) = b'}';
        *close.add(idx + 1) = 0;
    }
    p.depth -= 1;
    true
}

fn print_value(item: *const cJSON, p: &mut PrintBuffer) -> bool {
    if item.is_null() {
        return false;
    }
    // SAFETY: `item` is non-null (checked above).
    let (type_, valuestring) = unsafe { ((*item).type_ & 0xFF, (*item).valuestring) };
    match type_ {
        CJSON_NULL => write_literal(p, b"null\0"),
        CJSON_FALSE => write_literal(p, b"false\0"),
        CJSON_TRUE => write_literal(p, b"true\0"),
        CJSON_NUMBER => print_number(item, p),
        CJSON_RAW => {
            if valuestring.is_null() {
                return false;
            }
            // SAFETY: `valuestring` is a valid NUL-terminated C string (NULL checked above).
            let raw_bytes = unsafe { CStr::from_ptr(valuestring) }.to_bytes_with_nul();
            let out = ensure(p, raw_bytes.len());
            if out.is_null() {
                return false;
            }
            // SAFETY: `ensure` reserved `raw_bytes.len()` bytes at `out`.
            unsafe { ptr::copy_nonoverlapping(raw_bytes.as_ptr(), out, raw_bytes.len()) };
            true
        }
        CJSON_STRING => print_string_ptr(valuestring, p),
        CJSON_ARRAY => print_array(item, p),
        CJSON_OBJECT => print_object(item, p),
        _ => false,
    }
}

fn write_literal(p: &mut PrintBuffer, literal_with_nul: &[u8]) -> bool {
    let out = ensure(p, literal_with_nul.len());
    if out.is_null() {
        return false;
    }
    // SAFETY: `ensure` reserved `literal_with_nul.len()` bytes at `out`.
    unsafe { ptr::copy_nonoverlapping(literal_with_nul.as_ptr(), out, literal_with_nul.len()) };
    true
}

fn print_alloc(item: *const cJSON, format: bool) -> *mut c_char {
    const DEFAULT_BUF_SIZE: usize = 256;
    let hooks = current_hooks();
    // SAFETY: `hooks.allocate` is a valid C allocator function.
    let initial = unsafe { (hooks.allocate)(DEFAULT_BUF_SIZE) }.cast::<u8>();
    if initial.is_null() {
        return ptr::null_mut();
    }
    let mut pb = PrintBuffer {
        buffer: initial,
        length: DEFAULT_BUF_SIZE,
        offset: 0,
        depth: 0,
        noalloc: false,
        format,
        hooks,
    };

    if !print_value(item, &mut pb) {
        if !pb.buffer.is_null() {
            // SAFETY: `pb.buffer` was allocated with `hooks.allocate`.
            unsafe { (hooks.deallocate)(pb.buffer.cast::<c_void>()) };
        }
        return ptr::null_mut();
    }
    update_offset(&mut pb);

    let final_size = pb.offset + 1;
    if let Some(reallocate) = hooks.reallocate {
        // SAFETY: `pb.buffer` was allocated with `hooks.allocate` and `final_size > 0`.
        let shrunk = unsafe { reallocate(pb.buffer.cast::<c_void>(), final_size) }.cast::<u8>();
        if shrunk.is_null() {
            // SAFETY: `reallocate` failed so `pb.buffer` is still valid to free.
            unsafe { (hooks.deallocate)(pb.buffer.cast::<c_void>()) };
            return ptr::null_mut();
        }
        shrunk.cast::<c_char>()
    } else {
        // SAFETY: `hooks.allocate` is a valid C allocator function.
        let copy = unsafe { (hooks.allocate)(final_size) }.cast::<u8>();
        if copy.is_null() {
            // SAFETY: `pb.buffer` is valid to free on allocation failure.
            unsafe { (hooks.deallocate)(pb.buffer.cast::<c_void>()) };
            return ptr::null_mut();
        }
        let n = pb.length.min(final_size);
        // SAFETY: `pb.buffer` has at least `n` bytes and `copy` has `final_size` bytes.
        unsafe {
            ptr::copy_nonoverlapping(pb.buffer, copy, n);
            *copy.add(pb.offset) = 0;
            (hooks.deallocate)(pb.buffer.cast::<c_void>());
        }
        copy.cast::<c_char>()
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_Print(item: *const cJSON) -> *mut c_char {
    print_alloc(item, true)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_PrintUnformatted(item: *const cJSON) -> *mut c_char {
    print_alloc(item, false)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_PrintBuffered(
    item: *const cJSON,
    prebuffer: std::ffi::c_int,
    fmt: cJSON_bool,
) -> *mut c_char {
    if prebuffer < 0 {
        return ptr::null_mut();
    }
    let hooks = current_hooks();
    let len = prebuffer as usize;
    // SAFETY: `hooks.allocate` is a valid C allocator function.
    let buf = unsafe { (hooks.allocate)(len) }.cast::<u8>();
    if buf.is_null() {
        return ptr::null_mut();
    }
    let mut pb = PrintBuffer {
        buffer: buf,
        length: len,
        offset: 0,
        depth: 0,
        noalloc: false,
        format: fmt != 0,
        hooks,
    };
    if !print_value(item, &mut pb) {
        if !pb.buffer.is_null() {
            // SAFETY: `pb.buffer` was allocated with `hooks.allocate`.
            unsafe { (hooks.deallocate)(pb.buffer.cast::<c_void>()) };
        }
        return ptr::null_mut();
    }
    pb.buffer.cast::<c_char>()
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_PrintPreallocated(
    item: *mut cJSON,
    buffer: *mut c_char,
    length: std::ffi::c_int,
    format: cJSON_bool,
) -> cJSON_bool {
    if length < 0 || buffer.is_null() {
        return 0;
    }
    let mut pb = PrintBuffer {
        buffer: buffer.cast::<u8>(),
        length: length as usize,
        offset: 0,
        depth: 0,
        noalloc: true,
        format: format != 0,
        hooks: current_hooks(),
    };
    cJSON_bool::from(print_value(item, &mut pb))
}
