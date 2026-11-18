use std::ffi::{CStr, c_char};
use std::ptr;

use cjson_core::{
    CJSON_ARRAY, CJSON_FALSE, CJSON_NESTING_LIMIT, CJSON_NULL, CJSON_NUMBER, CJSON_OBJECT,
    CJSON_STRING, CJSON_TRUE, parse_json_string_cow, parse_number_slice, skip_utf8_bom,
    skip_whitespace,
};

use crate::hooks::{
    InternalHooks, cJSON, cJSON_bool, current_hooks, delete_item, new_item, reset_global_error,
    set_global_error,
};

struct ParseBuffer<'a> {
    content: &'a [u8],
    offset: usize,
    depth: usize,
    hooks: InternalHooks,
}

impl<'a> ParseBuffer<'a> {
    fn can_read(&self, size: usize) -> bool {
        self.offset
            .checked_add(size)
            .is_some_and(|end| end <= self.content.len())
    }

    fn current(&self) -> Option<u8> {
        self.content.get(self.offset).copied()
    }

    fn rest(&self) -> &'a [u8] {
        self.content.get(self.offset..).unwrap_or(&[])
    }

    fn skip_ws(&mut self) {
        self.offset = skip_whitespace(self.content, self.offset);
    }
}

fn parse_number(item: *mut cJSON, buf: &mut ParseBuffer<'_>) -> bool {
    let rest = buf.rest();
    if buf.hooks.reallocate.is_none() {
        let mut scan_len = 0usize;
        while let Some(&b) = rest.get(scan_len) {
            if matches!(b, b'0'..=b'9' | b'+' | b'-' | b'e' | b'E' | b'.') {
                scan_len += 1;
            } else {
                break;
            }
        }
        // SAFETY: Custom allocator hook is active; allocate `scan_len + 1` matching cJSON's custom hook allocation semantics.
        let scratch = unsafe { (buf.hooks.allocate)(scan_len + 1) };
        if scratch.is_null() {
            return false;
        }
        // SAFETY: `scratch` was just returned by `buf.hooks.allocate`.
        unsafe { (buf.hooks.deallocate)(scratch) };
    }
    let Some((number, valueint, consumed)) = parse_number_slice(rest) else {
        return false;
    };
    // SAFETY: `item` is a valid non-null `cJSON` node owned by the parser.
    unsafe {
        (*item).valuedouble = number;
        (*item).valueint = valueint;
        (*item).type_ = CJSON_NUMBER;
    }
    buf.offset += consumed;
    true
}

fn parse_string(item: *mut cJSON, buf: &mut ParseBuffer<'_>) -> bool {
    match parse_json_string_cow(buf.rest()) {
        Ok((bytes, consumed)) => {
            // SAFETY: `buf.hooks.allocate` is a valid C allocator; we allocate `bytes.len() + 1` once.
            let out_ptr = unsafe { (buf.hooks.allocate)(bytes.len() + 1) }.cast::<u8>();
            if out_ptr.is_null() {
                return false;
            }
            // SAFETY: `out_ptr` has `bytes.len() + 1` writable bytes; `item` is valid non-null `cJSON`.
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, bytes.len());
                *out_ptr.add(bytes.len()) = 0;
                (*item).type_ = CJSON_STRING;
                (*item).valuestring = out_ptr.cast::<c_char>();
            }
            buf.offset += consumed;
            true
        }
        Err(rel_err) => {
            buf.offset += rel_err;
            false
        }
    }
}

fn parse_array(item: *mut cJSON, buf: &mut ParseBuffer<'_>) -> bool {
    if buf.depth >= CJSON_NESTING_LIMIT {
        return false;
    }
    buf.depth += 1;

    if buf.current() != Some(b'[') {
        return false;
    }

    buf.offset += 1;
    buf.skip_ws();
    if buf.current() == Some(b']') {
        buf.depth -= 1;
        // SAFETY: `item` is a valid non-null `cJSON` node.
        unsafe {
            (*item).type_ = CJSON_ARRAY;
            (*item).child = ptr::null_mut();
        }
        buf.offset += 1;
        return true;
    }

    if buf.offset >= buf.content.len() {
        buf.offset = buf.offset.saturating_sub(1);
        return false;
    }

    let mut head: *mut cJSON = ptr::null_mut();
    let mut current_item: *mut cJSON = ptr::null_mut();

    buf.offset -= 1;
    loop {
        let new_node = new_item(&buf.hooks);
        if new_node.is_null() {
            delete_item(head);
            return false;
        }

        if head.is_null() {
            head = new_node;
            current_item = new_node;
        } else {
            // SAFETY: `current_item` and `new_node` are valid non-null `cJSON` nodes.
            unsafe {
                (*current_item).next = new_node;
                (*new_node).prev = current_item;
            }
            current_item = new_node;
        }

        buf.offset += 1;
        buf.skip_ws();
        if !parse_value(current_item, buf) {
            delete_item(head);
            return false;
        }
        buf.skip_ws();

        if buf.current() != Some(b',') {
            break;
        }
    }

    if buf.current() != Some(b']') {
        delete_item(head);
        return false;
    }

    buf.depth -= 1;
    if !head.is_null() {
        // SAFETY: `head` and `current_item` are valid non-null nodes in this list.
        unsafe { (*head).prev = current_item };
    }
    // SAFETY: `item` is a valid non-null `cJSON` node.
    unsafe {
        (*item).type_ = CJSON_ARRAY;
        (*item).child = head;
    }
    buf.offset += 1;
    true
}

fn parse_object(item: *mut cJSON, buf: &mut ParseBuffer<'_>) -> bool {
    if buf.depth >= CJSON_NESTING_LIMIT {
        return false;
    }
    buf.depth += 1;

    if buf.current() != Some(b'{') {
        return false;
    }

    buf.offset += 1;
    buf.skip_ws();
    if buf.current() == Some(b'}') {
        buf.depth -= 1;
        // SAFETY: `item` is a valid non-null `cJSON` node.
        unsafe {
            (*item).type_ = CJSON_OBJECT;
            (*item).child = ptr::null_mut();
        }
        buf.offset += 1;
        return true;
    }

    if buf.offset >= buf.content.len() {
        buf.offset = buf.offset.saturating_sub(1);
        return false;
    }

    let mut head: *mut cJSON = ptr::null_mut();
    let mut current_item: *mut cJSON = ptr::null_mut();

    buf.offset -= 1;
    loop {
        let new_node = new_item(&buf.hooks);
        if new_node.is_null() {
            delete_item(head);
            return false;
        }

        if head.is_null() {
            head = new_node;
            current_item = new_node;
        } else {
            // SAFETY: `current_item` and `new_node` are valid non-null `cJSON` nodes.
            unsafe {
                (*current_item).next = new_node;
                (*new_node).prev = current_item;
            }
            current_item = new_node;
        }

        if !buf.can_read(2) {
            delete_item(head);
            return false;
        }

        buf.offset += 1;
        buf.skip_ws();
        if !parse_string(current_item, buf) {
            delete_item(head);
            return false;
        }
        buf.skip_ws();

        // SAFETY: `current_item` is a valid node whose `valuestring` was just populated by `parse_string`.
        unsafe {
            (*current_item).string = (*current_item).valuestring;
            (*current_item).valuestring = ptr::null_mut();
        }

        if buf.current() != Some(b':') {
            delete_item(head);
            return false;
        }

        buf.offset += 1;
        buf.skip_ws();
        if !parse_value(current_item, buf) {
            delete_item(head);
            return false;
        }
        buf.skip_ws();

        if buf.current() != Some(b',') {
            break;
        }
    }

    if buf.current() != Some(b'}') {
        delete_item(head);
        return false;
    }

    buf.depth -= 1;
    if !head.is_null() {
        // SAFETY: `head` and `current_item` are valid non-null nodes in this list.
        unsafe { (*head).prev = current_item };
    }
    // SAFETY: `item` is a valid non-null `cJSON` node.
    unsafe {
        (*item).type_ = CJSON_OBJECT;
        (*item).child = head;
    }
    buf.offset += 1;
    true
}

fn parse_value(item: *mut cJSON, buf: &mut ParseBuffer<'_>) -> bool {
    let rest = buf.rest();
    if rest.starts_with(b"null") {
        // SAFETY: `item` is a valid non-null `cJSON` node.
        unsafe { (*item).type_ = CJSON_NULL };
        buf.offset += 4;
        return true;
    }
    if rest.starts_with(b"false") {
        // SAFETY: `item` is a valid non-null `cJSON` node.
        unsafe { (*item).type_ = CJSON_FALSE };
        buf.offset += 5;
        return true;
    }
    if rest.starts_with(b"true") {
        // SAFETY: `item` is a valid non-null `cJSON` node.
        unsafe {
            (*item).type_ = CJSON_TRUE;
            (*item).valueint = 1;
        }
        buf.offset += 4;
        return true;
    }
    match buf.current() {
        Some(b'"') => parse_string(item, buf),
        Some(b'-' | b'0'..=b'9') => parse_number(item, buf),
        Some(b'[') => parse_array(item, buf),
        Some(b'{') => parse_object(item, buf),
        _ => false,
    }
}

fn parse_with_opts(
    value: *const c_char,
    return_parse_end: *mut *const c_char,
    require_null_terminated: cJSON_bool,
) -> *mut cJSON {
    if value.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `value` is non-null and caller guarantees a NUL-terminated C string.
    let len_with_nul = unsafe { CStr::from_ptr(value) }.to_bytes_with_nul().len();
    parse_with_length_opts(
        value,
        len_with_nul,
        return_parse_end,
        require_null_terminated,
    )
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_ParseWithOpts(
    value: *const c_char,
    return_parse_end: *mut *const c_char,
    require_null_terminated: cJSON_bool,
) -> *mut cJSON {
    parse_with_opts(value, return_parse_end, require_null_terminated)
}

fn parse_with_length_opts(
    value: *const c_char,
    buffer_length: usize,
    return_parse_end: *mut *const c_char,
    require_null_terminated: cJSON_bool,
) -> *mut cJSON {
    reset_global_error();

    if value.is_null() || buffer_length == 0 {
        if !value.is_null() {
            if !return_parse_end.is_null() {
                // SAFETY: `return_parse_end` is non-null caller out-pointer.
                unsafe { *return_parse_end = value };
            }
            set_global_error(value.cast::<u8>(), 0);
        }
        return ptr::null_mut();
    }

    // SAFETY: `value` is non-null and caller guarantees `buffer_length` readable bytes.
    let content = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), buffer_length) };
    let hooks = current_hooks();
    let mut buf = ParseBuffer {
        content,
        offset: 0,
        depth: 0,
        hooks,
    };

    let item = new_item(&hooks);
    if item.is_null() {
        set_global_error(value.cast::<u8>(), 0);
        if !return_parse_end.is_null() {
            // SAFETY: `return_parse_end` is non-null caller out-pointer.
            unsafe { *return_parse_end = value };
        }
        return ptr::null_mut();
    }

    buf.offset = skip_utf8_bom(buf.content, buf.offset);
    buf.skip_ws();

    let mut ok = parse_value(item, &mut buf);
    if ok && require_null_terminated != 0 {
        buf.skip_ws();
        if buf.offset >= buf.content.len() || buf.current() != Some(0) {
            ok = false;
        }
    }

    if ok {
        if !return_parse_end.is_null() {
            // SAFETY: `buf.offset <= buffer_length` and `return_parse_end` is a valid out-pointer.
            unsafe { *return_parse_end = value.add(buf.offset) };
        }
        return item;
    }

    delete_item(item);
    let err_pos = if buf.offset < buf.content.len() {
        buf.offset
    } else {
        buf.content.len() - 1
    };
    if !return_parse_end.is_null() {
        // SAFETY: `err_pos < buffer_length` and `return_parse_end` is a valid out-pointer.
        unsafe { *return_parse_end = value.add(err_pos) };
    }
    set_global_error(value.cast::<u8>(), err_pos);
    ptr::null_mut()
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_ParseWithLengthOpts(
    value: *const c_char,
    buffer_length: usize,
    return_parse_end: *mut *const c_char,
    require_null_terminated: cJSON_bool,
) -> *mut cJSON {
    parse_with_length_opts(
        value,
        buffer_length,
        return_parse_end,
        require_null_terminated,
    )
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_Parse(value: *const c_char) -> *mut cJSON {
    parse_with_opts(value, ptr::null_mut(), 0)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_ParseWithLength(
    value: *const c_char,
    buffer_length: usize,
) -> *mut cJSON {
    parse_with_length_opts(value, buffer_length, ptr::null_mut(), 0)
}
