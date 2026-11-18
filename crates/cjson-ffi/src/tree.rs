use std::ffi::{CStr, c_char, c_float, c_int, c_void};
use std::ptr;

use cjson_core::{
    CJSON_ARRAY, CJSON_CIRCULAR_LIMIT, CJSON_FALSE, CJSON_INVALID, CJSON_IS_REFERENCE, CJSON_NULL,
    CJSON_NUMBER, CJSON_OBJECT, CJSON_RAW, CJSON_STRING, CJSON_STRING_IS_CONST, CJSON_TRUE,
    case_insensitive_cmp, compare_double, minify_bytes, saturate_to_c_int,
};

use crate::hooks::{cJSON, cJSON_bool, cjson_strdup, current_hooks, delete_item, new_item};

fn cstr_bytes<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        None
    } else {
        // SAFETY: Caller passes a valid NUL-terminated C string (NULL checked above).
        Some(unsafe { CStr::from_ptr(ptr) }.to_bytes())
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_GetStringValue(item: *const cJSON) -> *mut c_char {
    if check_type(item, CJSON_STRING) == 0 {
        return ptr::null_mut();
    }
    // SAFETY: `cJSON_IsString` verified `item` is non-null.
    unsafe { (*item).valuestring }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_GetNumberValue(item: *const cJSON) -> f64 {
    if check_type(item, CJSON_NUMBER) == 0 {
        return f64::NAN;
    }
    // SAFETY: `cJSON_IsNumber` verified `item` is non-null.
    unsafe { (*item).valuedouble }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_SetNumberHelper(object: *mut cJSON, number: f64) -> f64 {
    if object.is_null() {
        return f64::NAN;
    }
    let vi = saturate_to_c_int(number);
    // SAFETY: `object` is non-null (checked above).
    unsafe {
        (*object).valueint = vi;
        (*object).valuedouble = number;
    }
    number
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_SetValuestring(
    object: *mut cJSON,
    valuestring: *const c_char,
) -> *mut c_char {
    if object.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `object` is non-null (checked above).
    let (type_, cur_vs) = unsafe { ((*object).type_, (*object).valuestring) };
    if (type_ & CJSON_STRING) == 0 || (type_ & CJSON_IS_REFERENCE) != 0 {
        return ptr::null_mut();
    }
    if cur_vs.is_null() || valuestring.is_null() {
        return ptr::null_mut();
    }

    // SAFETY: `valuestring` and `cur_vs` are non-null NUL-terminated C strings.
    let v1_len = unsafe { CStr::from_ptr(valuestring) }.to_bytes().len();
    // SAFETY: `cur_vs` is non-null NUL-terminated C string.
    let v2_len = unsafe { CStr::from_ptr(cur_vs) }.to_bytes().len();

    if v1_len <= v2_len {
        let src_start = valuestring as usize;
        let src_end = src_start + v1_len;
        let dst_start = cur_vs as usize;
        let dst_end = dst_start + v2_len;
        if !(src_end < dst_start || dst_end < src_start) {
            return ptr::null_mut();
        }
        // SAFETY: Non-overlapping ranges verified above; `cur_vs` has capacity `>= v2_len + 1`.
        unsafe {
            ptr::copy_nonoverlapping(valuestring.cast::<u8>(), cur_vs.cast::<u8>(), v1_len + 1);
        }
        return cur_vs;
    }

    let hooks = current_hooks();
    let copy = cjson_strdup(valuestring, &hooks);
    if copy.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `cur_vs` was allocated by `hooks.allocate` and is replaced by `copy`.
    unsafe {
        (hooks.deallocate)(cur_vs.cast::<c_void>());
        (*object).valuestring = copy;
    }
    copy
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_GetArraySize(array: *const cJSON) -> c_int {
    if array.is_null() {
        return 0;
    }
    // SAFETY: `array` is non-null (checked above).
    let mut child = unsafe { (*array).child };
    let mut size = 0usize;
    while !child.is_null() {
        size += 1;
        // SAFETY: `child` is a valid node in the linked list.
        child = unsafe { (*child).next };
    }
    size as c_int
}

fn get_array_item(array: *const cJSON, mut index: usize) -> *mut cJSON {
    if array.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `array` is non-null (checked above).
    let mut current = unsafe { (*array).child };
    while !current.is_null() && index > 0 {
        index -= 1;
        // SAFETY: `current` is a valid node in the linked list.
        current = unsafe { (*current).next };
    }
    current
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_GetArrayItem(array: *const cJSON, index: c_int) -> *mut cJSON {
    if index < 0 {
        return ptr::null_mut();
    }
    get_array_item(array, index as usize)
}

fn get_object_item(object: *const cJSON, name: *const c_char, case_sensitive: bool) -> *mut cJSON {
    let Some(target) = cstr_bytes(name) else {
        return ptr::null_mut();
    };
    if object.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `object` is non-null (checked above).
    let mut current = unsafe { (*object).child };
    while !current.is_null() {
        // SAFETY: `current` is a valid node in the linked list.
        let key_ptr = unsafe { (*current).string };
        let Some(key_bytes) = cstr_bytes(key_ptr) else {
            break;
        };
        let eq = if case_sensitive {
            target == key_bytes
        } else {
            case_insensitive_cmp(target, key_bytes) == 0
        };
        if eq {
            return current;
        }
        // SAFETY: `current` is a valid node in the linked list.
        current = unsafe { (*current).next };
    }
    ptr::null_mut()
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_GetObjectItem(
    object: *const cJSON,
    string: *const c_char,
) -> *mut cJSON {
    get_object_item(object, string, false)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_GetObjectItemCaseSensitive(
    object: *const cJSON,
    string: *const c_char,
) -> *mut cJSON {
    get_object_item(object, string, true)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_HasObjectItem(
    object: *const cJSON,
    string: *const c_char,
) -> cJSON_bool {
    cJSON_bool::from(!get_object_item(object, string, false).is_null())
}

fn suffix_object(prev: *mut cJSON, item: *mut cJSON) {
    // SAFETY: Caller ensures `prev` and `item` are non-null nodes.
    unsafe {
        (*prev).next = item;
        (*item).prev = prev;
    }
}

fn create_reference(item: *const cJSON) -> *mut cJSON {
    if item.is_null() {
        return ptr::null_mut();
    }
    let hooks = current_hooks();
    let reference = new_item(&hooks);
    if reference.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `item` and `reference` are valid non-null `cJSON` structs.
    unsafe {
        ptr::copy_nonoverlapping(item, reference, 1);
        (*reference).string = ptr::null_mut();
        (*reference).type_ |= CJSON_IS_REFERENCE;
        (*reference).next = ptr::null_mut();
        (*reference).prev = ptr::null_mut();
    }
    reference
}

fn add_item_to_array(array: *mut cJSON, item: *mut cJSON) -> cJSON_bool {
    if item.is_null() || array.is_null() || std::ptr::eq(array, item) {
        return 0;
    }
    // SAFETY: `array` and `item` are non-null distinct `cJSON` pointers.
    unsafe {
        let child = (*array).child;
        if child.is_null() {
            (*array).child = item;
            (*item).prev = item;
            (*item).next = ptr::null_mut();
        } else if !(*child).prev.is_null() {
            suffix_object((*child).prev, item);
            (*(*array).child).prev = item;
        }
    }
    1
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddItemToArray(array: *mut cJSON, item: *mut cJSON) -> cJSON_bool {
    add_item_to_array(array, item)
}

fn add_item_to_object(
    object: *mut cJSON,
    string: *const c_char,
    item: *mut cJSON,
    constant_key: bool,
) -> cJSON_bool {
    if object.is_null() || string.is_null() || item.is_null() || std::ptr::eq(object, item) {
        return 0;
    }
    let hooks = current_hooks();
    // SAFETY: `item` is non-null (checked above).
    let item_type = unsafe { (*item).type_ };
    let (new_key, new_type) = if constant_key {
        (string.cast_mut(), item_type | CJSON_STRING_IS_CONST)
    } else {
        let dup = cjson_strdup(string, &hooks);
        if dup.is_null() {
            return 0;
        }
        (dup, item_type & !CJSON_STRING_IS_CONST)
    };

    // SAFETY: `item` is non-null; old non-const `string` is freed before overwriting.
    unsafe {
        if (item_type & CJSON_STRING_IS_CONST) == 0 && !(*item).string.is_null() {
            (hooks.deallocate)((*item).string.cast::<c_void>());
        }
        (*item).string = new_key;
        (*item).type_ = new_type;
    }

    add_item_to_array(object, item)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddItemToObject(
    object: *mut cJSON,
    string: *const c_char,
    item: *mut cJSON,
) -> cJSON_bool {
    add_item_to_object(object, string, item, false)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddItemToObjectCS(
    object: *mut cJSON,
    string: *const c_char,
    item: *mut cJSON,
) -> cJSON_bool {
    add_item_to_object(object, string, item, true)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddItemReferenceToArray(
    array: *mut cJSON,
    item: *mut cJSON,
) -> cJSON_bool {
    if array.is_null() {
        return 0;
    }
    add_item_to_array(array, create_reference(item))
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddItemReferenceToObject(
    object: *mut cJSON,
    string: *const c_char,
    item: *mut cJSON,
) -> cJSON_bool {
    if object.is_null() || string.is_null() {
        return 0;
    }
    add_item_to_object(object, string, create_reference(item), false)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddNullToObject(
    object: *mut cJSON,
    name: *const c_char,
) -> *mut cJSON {
    let n = cJSON_CreateNull();
    if add_item_to_object(object, name, n, false) != 0 {
        n
    } else {
        delete_item(n);
        ptr::null_mut()
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddTrueToObject(
    object: *mut cJSON,
    name: *const c_char,
) -> *mut cJSON {
    let n = cJSON_CreateTrue();
    if add_item_to_object(object, name, n, false) != 0 {
        n
    } else {
        delete_item(n);
        ptr::null_mut()
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddFalseToObject(
    object: *mut cJSON,
    name: *const c_char,
) -> *mut cJSON {
    let n = cJSON_CreateFalse();
    if add_item_to_object(object, name, n, false) != 0 {
        n
    } else {
        delete_item(n);
        ptr::null_mut()
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddBoolToObject(
    object: *mut cJSON,
    name: *const c_char,
    boolean: cJSON_bool,
) -> *mut cJSON {
    let n = cJSON_CreateBool(boolean);
    if add_item_to_object(object, name, n, false) != 0 {
        n
    } else {
        delete_item(n);
        ptr::null_mut()
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddNumberToObject(
    object: *mut cJSON,
    name: *const c_char,
    number: f64,
) -> *mut cJSON {
    let n = cJSON_CreateNumber(number);
    if add_item_to_object(object, name, n, false) != 0 {
        n
    } else {
        delete_item(n);
        ptr::null_mut()
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddStringToObject(
    object: *mut cJSON,
    name: *const c_char,
    string: *const c_char,
) -> *mut cJSON {
    let n = create_string(string);
    if add_item_to_object(object, name, n, false) != 0 {
        n
    } else {
        delete_item(n);
        ptr::null_mut()
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddRawToObject(
    object: *mut cJSON,
    name: *const c_char,
    raw: *const c_char,
) -> *mut cJSON {
    let n = create_raw(raw);
    if add_item_to_object(object, name, n, false) != 0 {
        n
    } else {
        delete_item(n);
        ptr::null_mut()
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddObjectToObject(
    object: *mut cJSON,
    name: *const c_char,
) -> *mut cJSON {
    let n = cJSON_CreateObject();
    if add_item_to_object(object, name, n, false) != 0 {
        n
    } else {
        delete_item(n);
        ptr::null_mut()
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_AddArrayToObject(
    object: *mut cJSON,
    name: *const c_char,
) -> *mut cJSON {
    let n = cJSON_CreateArray();
    if add_item_to_object(object, name, n, false) != 0 {
        n
    } else {
        delete_item(n);
        ptr::null_mut()
    }
}

fn detach_item_via_pointer(parent: *mut cJSON, item: *mut cJSON) -> *mut cJSON {
    if parent.is_null() || item.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `parent` and `item` are non-null; verify `item` has `prev` unless it is `parent->child`.
    unsafe {
        if !std::ptr::eq(item, (*parent).child) && (*item).prev.is_null() {
            return ptr::null_mut();
        }
        if !std::ptr::eq(item, (*parent).child) {
            (*(*item).prev).next = (*item).next;
        }
        if !(*item).next.is_null() {
            (*(*item).next).prev = (*item).prev;
        }
        if std::ptr::eq(item, (*parent).child) {
            (*parent).child = (*item).next;
        } else if (*item).next.is_null() && !(*parent).child.is_null() {
            (*(*parent).child).prev = (*item).prev;
        }
        (*item).prev = ptr::null_mut();
        (*item).next = ptr::null_mut();
    }
    item
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_DetachItemViaPointer(
    parent: *mut cJSON,
    item: *mut cJSON,
) -> *mut cJSON {
    detach_item_via_pointer(parent, item)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_DetachItemFromArray(array: *mut cJSON, which: c_int) -> *mut cJSON {
    if which < 0 {
        return ptr::null_mut();
    }
    detach_item_via_pointer(array, get_array_item(array, which as usize))
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_DeleteItemFromArray(array: *mut cJSON, which: c_int) {
    if which >= 0 {
        delete_item(detach_item_via_pointer(
            array,
            get_array_item(array, which as usize),
        ));
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_DetachItemFromObject(
    object: *mut cJSON,
    string: *const c_char,
) -> *mut cJSON {
    detach_item_via_pointer(object, get_object_item(object, string, false))
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_DetachItemFromObjectCaseSensitive(
    object: *mut cJSON,
    string: *const c_char,
) -> *mut cJSON {
    detach_item_via_pointer(object, get_object_item(object, string, true))
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_DeleteItemFromObject(object: *mut cJSON, string: *const c_char) {
    delete_item(detach_item_via_pointer(
        object,
        get_object_item(object, string, false),
    ));
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_DeleteItemFromObjectCaseSensitive(
    object: *mut cJSON,
    string: *const c_char,
) {
    delete_item(detach_item_via_pointer(
        object,
        get_object_item(object, string, true),
    ));
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_InsertItemInArray(
    array: *mut cJSON,
    which: c_int,
    newitem: *mut cJSON,
) -> cJSON_bool {
    if which < 0 || newitem.is_null() {
        return 0;
    }
    let after = get_array_item(array, which as usize);
    if after.is_null() {
        return add_item_to_array(array, newitem);
    }
    // SAFETY: `after` is non-null in `array->child` chain and `newitem` is non-null.
    unsafe {
        if !std::ptr::eq(after, (*array).child) && (*after).prev.is_null() {
            return 0;
        }
        (*newitem).next = after;
        (*newitem).prev = (*after).prev;
        (*after).prev = newitem;
        if std::ptr::eq(after, (*array).child) {
            (*array).child = newitem;
        } else {
            (*(*newitem).prev).next = newitem;
        }
    }
    1
}

fn replace_item_via_pointer(
    parent: *mut cJSON,
    item: *mut cJSON,
    replacement: *mut cJSON,
) -> cJSON_bool {
    if parent.is_null() || replacement.is_null() || item.is_null() {
        return 0;
    }
    // SAFETY: `parent`, `item`, `replacement` are non-null.
    unsafe {
        if (*parent).child.is_null() {
            return 0;
        }
        if std::ptr::eq(replacement, item) {
            return 1;
        }
        (*replacement).next = (*item).next;
        (*replacement).prev = (*item).prev;
        if !(*replacement).next.is_null() {
            (*(*replacement).next).prev = replacement;
        }
        if std::ptr::eq((*parent).child, item) {
            if std::ptr::eq((*(*parent).child).prev, (*parent).child) {
                (*replacement).prev = replacement;
            }
            (*parent).child = replacement;
        } else {
            if !(*replacement).prev.is_null() {
                (*(*replacement).prev).next = replacement;
            }
            if (*replacement).next.is_null() {
                (*(*parent).child).prev = replacement;
            }
        }
        (*item).next = ptr::null_mut();
        (*item).prev = ptr::null_mut();
    }
    delete_item(item);
    1
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_ReplaceItemViaPointer(
    parent: *mut cJSON,
    item: *mut cJSON,
    replacement: *mut cJSON,
) -> cJSON_bool {
    replace_item_via_pointer(parent, item, replacement)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_ReplaceItemInArray(
    array: *mut cJSON,
    which: c_int,
    newitem: *mut cJSON,
) -> cJSON_bool {
    if which < 0 {
        return 0;
    }
    replace_item_via_pointer(array, get_array_item(array, which as usize), newitem)
}

fn replace_item_in_object(
    object: *mut cJSON,
    string: *const c_char,
    replacement: *mut cJSON,
    case_sensitive: bool,
) -> cJSON_bool {
    if replacement.is_null() || string.is_null() {
        return 0;
    }
    let hooks = current_hooks();
    let dup = cjson_strdup(string, &hooks);
    if dup.is_null() {
        return 0;
    }
    // SAFETY: `replacement` is non-null; replace its `string` before splicing into `object`.
    unsafe {
        if ((*replacement).type_ & CJSON_STRING_IS_CONST) == 0 && !(*replacement).string.is_null() {
            (hooks.deallocate)((*replacement).string.cast::<c_void>());
        }
        (*replacement).string = dup;
        (*replacement).type_ &= !CJSON_STRING_IS_CONST;
    }
    replace_item_via_pointer(
        object,
        get_object_item(object, string, case_sensitive),
        replacement,
    )
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_ReplaceItemInObject(
    object: *mut cJSON,
    string: *const c_char,
    newitem: *mut cJSON,
) -> cJSON_bool {
    replace_item_in_object(object, string, newitem, false)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_ReplaceItemInObjectCaseSensitive(
    object: *mut cJSON,
    string: *const c_char,
    newitem: *mut cJSON,
) -> cJSON_bool {
    replace_item_in_object(object, string, newitem, true)
}

#[unsafe(no_mangle)]
pub extern "C" fn cJSON_CreateNull() -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        // SAFETY: `item` is non-null freshly allocated node.
        unsafe { (*item).type_ = CJSON_NULL };
    }
    item
}

#[unsafe(no_mangle)]
pub extern "C" fn cJSON_CreateTrue() -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        // SAFETY: `item` is non-null freshly allocated node.
        unsafe { (*item).type_ = CJSON_TRUE };
    }
    item
}

#[unsafe(no_mangle)]
pub extern "C" fn cJSON_CreateFalse() -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        // SAFETY: `item` is non-null freshly allocated node.
        unsafe { (*item).type_ = CJSON_FALSE };
    }
    item
}

#[unsafe(no_mangle)]
pub extern "C" fn cJSON_CreateBool(boolean: cJSON_bool) -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        // SAFETY: `item` is non-null freshly allocated node.
        unsafe {
            (*item).type_ = if boolean != 0 {
                CJSON_TRUE
            } else {
                CJSON_FALSE
            }
        };
    }
    item
}

#[unsafe(no_mangle)]
pub extern "C" fn cJSON_CreateNumber(num: f64) -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        let vi = saturate_to_c_int(num);
        // SAFETY: `item` is non-null freshly allocated node.
        unsafe {
            (*item).type_ = CJSON_NUMBER;
            (*item).valuedouble = num;
            (*item).valueint = vi;
        }
    }
    item
}

fn create_string(string: *const c_char) -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        let dup = cjson_strdup(string, &hooks);
        if dup.is_null() {
            delete_item(item);
            return ptr::null_mut();
        }
        // SAFETY: `item` is non-null freshly allocated node.
        unsafe {
            (*item).type_ = CJSON_STRING;
            (*item).valuestring = dup;
        }
    }
    item
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_CreateString(string: *const c_char) -> *mut cJSON {
    create_string(string)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_CreateStringReference(string: *const c_char) -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        // SAFETY: `item` is non-null; `CJSON_IS_REFERENCE` ensures `valuestring` is never freed.
        unsafe {
            (*item).type_ = CJSON_STRING | CJSON_IS_REFERENCE;
            (*item).valuestring = string.cast_mut();
        }
    }
    item
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_CreateObjectReference(child: *const cJSON) -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        // SAFETY: `item` is non-null; `CJSON_IS_REFERENCE` ensures `child` is never freed.
        unsafe {
            (*item).type_ = CJSON_OBJECT | CJSON_IS_REFERENCE;
            (*item).child = child.cast_mut();
        }
    }
    item
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_CreateArrayReference(child: *const cJSON) -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        // SAFETY: `item` is non-null; `CJSON_IS_REFERENCE` ensures `child` is never freed.
        unsafe {
            (*item).type_ = CJSON_ARRAY | CJSON_IS_REFERENCE;
            (*item).child = child.cast_mut();
        }
    }
    item
}

fn create_raw(raw: *const c_char) -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        let dup = cjson_strdup(raw, &hooks);
        if dup.is_null() {
            delete_item(item);
            return ptr::null_mut();
        }
        // SAFETY: `item` is non-null freshly allocated node.
        unsafe {
            (*item).type_ = CJSON_RAW;
            (*item).valuestring = dup;
        }
    }
    item
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_CreateRaw(raw: *const c_char) -> *mut cJSON {
    create_raw(raw)
}

#[unsafe(no_mangle)]
pub extern "C" fn cJSON_CreateArray() -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        // SAFETY: `item` is non-null freshly allocated node.
        unsafe { (*item).type_ = CJSON_ARRAY };
    }
    item
}

#[unsafe(no_mangle)]
pub extern "C" fn cJSON_CreateObject() -> *mut cJSON {
    let hooks = current_hooks();
    let item = new_item(&hooks);
    if !item.is_null() {
        // SAFETY: `item` is non-null freshly allocated node.
        unsafe { (*item).type_ = CJSON_OBJECT };
    }
    item
}

fn create_typed_array<T: Copy>(
    items: *const T,
    count: c_int,
    make: impl Fn(T) -> *mut cJSON,
) -> *mut cJSON {
    if count < 0 || items.is_null() {
        return ptr::null_mut();
    }
    let a = cJSON_CreateArray();
    if a.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: Caller guarantees `items` points to at least `count` readable elements.
    let slice = unsafe { std::slice::from_raw_parts(items, count as usize) };
    let mut prev: *mut cJSON = ptr::null_mut();
    let mut last: *mut cJSON = ptr::null_mut();
    for (i, &val) in slice.iter().enumerate() {
        let n = make(val);
        if n.is_null() {
            delete_item(a);
            return ptr::null_mut();
        }
        if i == 0 {
            // SAFETY: `a` is non-null.
            unsafe { (*a).child = n };
        } else {
            suffix_object(prev, n);
        }
        prev = n;
        last = n;
    }
    // SAFETY: `a` is non-null; if `(*a).child` is non-null then `last` is non-null.
    unsafe {
        if !(*a).child.is_null() {
            (*(*a).child).prev = last;
        }
    }
    a
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_CreateIntArray(numbers: *const c_int, count: c_int) -> *mut cJSON {
    create_typed_array(numbers, count, |v| cJSON_CreateNumber(f64::from(v)))
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_CreateFloatArray(
    numbers: *const c_float,
    count: c_int,
) -> *mut cJSON {
    create_typed_array(numbers, count, |v| cJSON_CreateNumber(f64::from(v)))
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_CreateDoubleArray(numbers: *const f64, count: c_int) -> *mut cJSON {
    create_typed_array(numbers, count, |v| cJSON_CreateNumber(v))
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_CreateStringArray(
    strings: *const *const c_char,
    count: c_int,
) -> *mut cJSON {
    create_typed_array(strings, count, create_string)
}

fn duplicate_rec(item: *const cJSON, depth: usize, recurse: cJSON_bool) -> *mut cJSON {
    if item.is_null() {
        return ptr::null_mut();
    }
    let hooks = current_hooks();
    let newitem = new_item(&hooks);
    if newitem.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `item` and `newitem` are non-null `cJSON` structs.
    unsafe {
        (*newitem).type_ = (*item).type_ & !CJSON_IS_REFERENCE;
        (*newitem).valueint = (*item).valueint;
        (*newitem).valuedouble = (*item).valuedouble;
        if !(*item).valuestring.is_null() {
            (*newitem).valuestring = cjson_strdup((*item).valuestring, &hooks);
            if (*newitem).valuestring.is_null() {
                delete_item(newitem);
                return ptr::null_mut();
            }
        }
        if !(*item).string.is_null() {
            (*newitem).string = if ((*item).type_ & CJSON_STRING_IS_CONST) != 0 {
                (*item).string
            } else {
                cjson_strdup((*item).string, &hooks)
            };
            if (*newitem).string.is_null() {
                delete_item(newitem);
                return ptr::null_mut();
            }
        }
    }

    if recurse == 0 {
        return newitem;
    }

    // SAFETY: `item` is non-null.
    let mut child = unsafe { (*item).child };
    let mut next: *mut cJSON = ptr::null_mut();
    let mut last_newchild: *mut cJSON = ptr::null_mut();
    while !child.is_null() {
        if depth >= CJSON_CIRCULAR_LIMIT {
            delete_item(newitem);
            return ptr::null_mut();
        }
        let newchild = duplicate_rec(child, depth + 1, 1);
        if newchild.is_null() {
            delete_item(newitem);
            return ptr::null_mut();
        }
        if !next.is_null() {
            // SAFETY: `next` and `newchild` are valid non-null nodes.
            unsafe {
                (*next).next = newchild;
                (*newchild).prev = next;
            }
            next = newchild;
        } else {
            // SAFETY: `newitem` and `newchild` are valid non-null nodes.
            unsafe { (*newitem).child = newchild };
            next = newchild;
        }
        last_newchild = newchild;
        // SAFETY: `child` is non-null.
        child = unsafe { (*child).next };
    }
    // SAFETY: `newitem` is non-null.
    unsafe {
        if !(*newitem).child.is_null() {
            (*(*newitem).child).prev = last_newchild;
        }
    }
    newitem
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_Duplicate(item: *const cJSON, recurse: cJSON_bool) -> *mut cJSON {
    duplicate_rec(item, 0, recurse)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_Minify(json: *mut c_char) {
    if json.is_null() {
        return;
    }
    // SAFETY: `json` is a valid mutable NUL-terminated C string (NULL checked above).
    let len = unsafe { CStr::from_ptr(json) }.to_bytes().len();
    // SAFETY: `json` points to `len + 1` writable bytes.
    let slice = unsafe { std::slice::from_raw_parts_mut(json.cast::<u8>(), len + 1) };
    let new_len = minify_bytes(slice);
    slice[new_len] = 0;
}

fn check_type(item: *const cJSON, expected: c_int) -> cJSON_bool {
    if item.is_null() {
        return 0;
    }
    // SAFETY: `item` is non-null (checked above).
    cJSON_bool::from((unsafe { (*item).type_ } & 0xFF) == expected)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_IsInvalid(item: *const cJSON) -> cJSON_bool {
    check_type(item, CJSON_INVALID)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_IsFalse(item: *const cJSON) -> cJSON_bool {
    check_type(item, CJSON_FALSE)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_IsTrue(item: *const cJSON) -> cJSON_bool {
    check_type(item, CJSON_TRUE)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_IsBool(item: *const cJSON) -> cJSON_bool {
    if item.is_null() {
        return 0;
    }
    // SAFETY: `item` is non-null (checked above).
    cJSON_bool::from((unsafe { (*item).type_ } & (CJSON_TRUE | CJSON_FALSE)) != 0)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_IsNull(item: *const cJSON) -> cJSON_bool {
    check_type(item, CJSON_NULL)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_IsNumber(item: *const cJSON) -> cJSON_bool {
    check_type(item, CJSON_NUMBER)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_IsString(item: *const cJSON) -> cJSON_bool {
    check_type(item, CJSON_STRING)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_IsArray(item: *const cJSON) -> cJSON_bool {
    check_type(item, CJSON_ARRAY)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_IsObject(item: *const cJSON) -> cJSON_bool {
    check_type(item, CJSON_OBJECT)
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_IsRaw(item: *const cJSON) -> cJSON_bool {
    check_type(item, CJSON_RAW)
}

fn compare_items(a: *const cJSON, b: *const cJSON, case_sensitive: cJSON_bool) -> cJSON_bool {
    if a.is_null() || b.is_null() {
        return 0;
    }
    // SAFETY: `a` and `b` are non-null (checked above).
    let (ta, tb) = unsafe { ((*a).type_ & 0xFF, (*b).type_ & 0xFF) };
    if ta != tb {
        return 0;
    }
    if !matches!(
        ta,
        CJSON_FALSE
            | CJSON_TRUE
            | CJSON_NULL
            | CJSON_NUMBER
            | CJSON_STRING
            | CJSON_RAW
            | CJSON_ARRAY
            | CJSON_OBJECT
    ) {
        return 0;
    }
    if std::ptr::eq(a, b) {
        return 1;
    }
    match ta {
        CJSON_FALSE | CJSON_TRUE | CJSON_NULL => 1,
        CJSON_NUMBER => {
            // SAFETY: `a` and `b` are non-null `cJSON` nodes.
            let (da, db) = unsafe { ((*a).valuedouble, (*b).valuedouble) };
            cJSON_bool::from(compare_double(da, db))
        }
        CJSON_STRING | CJSON_RAW => {
            // SAFETY: `a` and `b` are non-null `cJSON` nodes.
            let (sa, sb) = unsafe { ((*a).valuestring, (*b).valuestring) };
            match (cstr_bytes(sa), cstr_bytes(sb)) {
                (Some(ba), Some(bb)) => cJSON_bool::from(ba == bb),
                _ => 0,
            }
        }
        CJSON_ARRAY => {
            // SAFETY: `a` and `b` are non-null `cJSON` nodes.
            let (mut ea, mut eb) = unsafe { ((*a).child, (*b).child) };
            while !ea.is_null() && !eb.is_null() {
                if compare_items(ea, eb, case_sensitive) == 0 {
                    return 0;
                }
                // SAFETY: `ea` and `eb` are valid non-null nodes.
                unsafe {
                    ea = (*ea).next;
                    eb = (*eb).next;
                }
            }
            cJSON_bool::from(ea.is_null() && eb.is_null())
        }
        CJSON_OBJECT => {
            let cs = case_sensitive != 0;
            // SAFETY: `a` and `b` are non-null `cJSON` nodes.
            let mut ea = unsafe { (*a).child };
            while !ea.is_null() {
                // SAFETY: `ea` is a valid non-null node.
                let key = unsafe { (*ea).string };
                let eb = get_object_item(b, key, cs);
                if eb.is_null() || compare_items(ea, eb, case_sensitive) == 0 {
                    return 0;
                }
                // SAFETY: `ea` is a valid non-null node.
                ea = unsafe { (*ea).next };
            }
            // SAFETY: `b` is a non-null `cJSON` node.
            let mut eb = unsafe { (*b).child };
            while !eb.is_null() {
                // SAFETY: `eb` is a valid non-null node.
                let key = unsafe { (*eb).string };
                let ea2 = get_object_item(a, key, cs);
                if ea2.is_null() || compare_items(eb, ea2, case_sensitive) == 0 {
                    return 0;
                }
                // SAFETY: `eb` is a valid non-null node.
                eb = unsafe { (*eb).next };
            }
            1
        }
        _ => 0,
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_Compare(
    a: *const cJSON,
    b: *const cJSON,
    case_sensitive: cJSON_bool,
) -> cJSON_bool {
    compare_items(a, b, case_sensitive)
}
