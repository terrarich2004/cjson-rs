use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

pub type cJSON_bool = c_int;

#[repr(C)]
pub struct cJSON {
    pub next: *mut cJSON,
    pub prev: *mut cJSON,
    pub child: *mut cJSON,
    pub type_: c_int,
    pub valuestring: *mut c_char,
    pub valueint: c_int,
    pub valuedouble: f64,
    pub string: *mut c_char,
}

#[repr(C)]
pub struct cJSON_Hooks {
    pub malloc_fn: Option<unsafe extern "C" fn(usize) -> *mut c_void>,
    pub free_fn: Option<unsafe extern "C" fn(*mut c_void)>,
}

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
}

#[derive(Clone, Copy)]
pub(crate) struct InternalHooks {
    pub allocate: unsafe extern "C" fn(usize) -> *mut c_void,
    pub deallocate: unsafe extern "C" fn(*mut c_void),
    pub reallocate: Option<unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void>,
}

static CUSTOM_HOOKS_ACTIVE: AtomicBool = AtomicBool::new(false);

static GLOBAL_HOOKS: Mutex<InternalHooks> = Mutex::new(InternalHooks {
    allocate: malloc,
    deallocate: free,
    reallocate: Some(realloc),
});

static ERROR_JSON: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());
static ERROR_POS: AtomicUsize = AtomicUsize::new(0);

#[inline]
pub(crate) fn current_hooks() -> InternalHooks {
    if !CUSTOM_HOOKS_ACTIVE.load(Ordering::Acquire) {
        return InternalHooks {
            allocate: malloc,
            deallocate: free,
            reallocate: Some(realloc),
        };
    }
    match GLOBAL_HOOKS.lock() {
        Ok(g) => *g,
        Err(p) => *p.into_inner(),
    }
}

pub(crate) fn reset_global_error() {
    ERROR_JSON.store(ptr::null_mut(), Ordering::Release);
    ERROR_POS.store(0, Ordering::Release);
}

pub(crate) fn set_global_error(json: *const u8, position: usize) {
    ERROR_JSON.store(json.cast_mut(), Ordering::Release);
    ERROR_POS.store(position, Ordering::Release);
}

/// Allocate and zero-initialize a `cJSON` node using `hooks`.
pub(crate) fn new_item(hooks: &InternalHooks) -> *mut cJSON {
    // SAFETY: `hooks.allocate` is a valid C allocator function (`malloc` or user-supplied hook);
    // `size_of::<cJSON>()` is non-zero, and if non-null we zero-initialize the exact struct size.
    let node = unsafe { (hooks.allocate)(size_of::<cJSON>()) }.cast::<cJSON>();
    if !node.is_null() {
        // SAFETY: `node` points to at least `size_of::<cJSON>()` writable bytes just allocated.
        unsafe { ptr::write_bytes(node, 0, 1) };
    }
    node
}

/// Duplicate a NUL-terminated C string using `hooks.allocate`. Returns `NULL` on `NULL` input or OOM.
pub(crate) fn cjson_strdup(s: *const c_char, hooks: &InternalHooks) -> *mut c_char {
    if s.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: Caller passes a valid NUL-terminated C string (NULL checked above).
    let bytes = unsafe { CStr::from_ptr(s) }.to_bytes_with_nul();
    // SAFETY: `hooks.allocate` is a valid C allocator function; returned pointer is checked for NULL.
    let copy = unsafe { (hooks.allocate)(bytes.len()) }.cast::<u8>();
    if copy.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `copy` is freshly allocated with `bytes.len()` bytes and does not overlap `bytes`.
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), copy, bytes.len()) };
    copy.cast::<c_char>()
}

#[unsafe(no_mangle)]
pub extern "C" fn cJSON_Version() -> *const c_char {
    c"1.7.19".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn cJSON_GetErrorPtr() -> *const c_char {
    let base = ERROR_JSON.load(Ordering::Acquire);
    let pos = ERROR_POS.load(Ordering::Acquire);
    if base.is_null() {
        ptr::null()
    } else {
        // SAFETY: `base` and `pos` come from the last failed `cJSON_Parse*` input buffer.
        unsafe { base.add(pos).cast::<c_char>() }
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_InitHooks(hooks: *mut cJSON_Hooks) {
    let (both_default, new_hooks) = if hooks.is_null() {
        (
            true,
            InternalHooks {
                allocate: malloc,
                deallocate: free,
                reallocate: Some(realloc),
            },
        )
    } else {
        // SAFETY: `hooks` is non-null and points to a valid `cJSON_Hooks` struct provided by caller.
        let h = unsafe { &*hooks };
        let alloc_fn = h.malloc_fn.unwrap_or(malloc);
        let free_fn = h.free_fn.unwrap_or(free);
        let both_default =
            std::ptr::fn_addr_eq(
                alloc_fn,
                malloc as unsafe extern "C" fn(usize) -> *mut c_void,
            ) && std::ptr::fn_addr_eq(free_fn, free as unsafe extern "C" fn(*mut c_void));
        (
            both_default,
            InternalHooks {
                allocate: alloc_fn,
                deallocate: free_fn,
                reallocate: if both_default { Some(realloc) } else { None },
            },
        )
    };

    match GLOBAL_HOOKS.lock() {
        Ok(mut g) => *g = new_hooks,
        Err(p) => *p.into_inner() = new_hooks,
    }
    CUSTOM_HOOKS_ACTIVE.store(!both_default, Ordering::Release);
}

pub(crate) fn delete_item(mut item: *mut cJSON) {
    let hooks = current_hooks();
    while !item.is_null() {
        // SAFETY: `item` is a valid `cJSON` node owned by `delete_item` (NULL checked above).
        let (next, child, type_, valuestring, string) = unsafe {
            let r = &mut *item;
            let next = r.next;
            let child = r.child;
            let type_ = r.type_;
            let valuestring = r.valuestring;
            let string = r.string;
            if (type_ & cjson_core::CJSON_IS_REFERENCE) == 0 && !valuestring.is_null() {
                r.valuestring = ptr::null_mut();
            }
            if (type_ & cjson_core::CJSON_STRING_IS_CONST) == 0 && !string.is_null() {
                r.string = ptr::null_mut();
            }
            (next, child, type_, valuestring, string)
        };

        if (type_ & cjson_core::CJSON_IS_REFERENCE) == 0 && !child.is_null() {
            delete_item(child);
        }
        if (type_ & cjson_core::CJSON_IS_REFERENCE) == 0 && !valuestring.is_null() {
            // SAFETY: `valuestring` was allocated via `hooks.allocate` and is owned by `item`.
            unsafe { (hooks.deallocate)(valuestring.cast::<c_void>()) };
        }
        if (type_ & cjson_core::CJSON_STRING_IS_CONST) == 0 && !string.is_null() {
            // SAFETY: `string` was allocated via `hooks.allocate` and is owned by `item`.
            unsafe { (hooks.deallocate)(string.cast::<c_void>()) };
        }
        // SAFETY: `item` itself was allocated via `hooks.allocate` and all owned subfields are freed.
        unsafe { (hooks.deallocate)(item.cast::<c_void>()) };
        item = next;
    }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_Delete(item: *mut cJSON) {
    delete_item(item);
}

#[unsafe(no_mangle)]
pub extern "C" fn cJSON_malloc(size: usize) -> *mut c_void {
    let hooks = current_hooks();
    // SAFETY: `hooks.allocate` is a valid C allocation function.
    unsafe { (hooks.allocate)(size) }
}

/// # Safety
///
/// Any non-NULL pointer arguments must satisfy the `include/cJSON.h` API contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cJSON_free(object: *mut c_void) {
    let hooks = current_hooks();
    // SAFETY: `object` must be NULL or a pointer previously allocated by `hooks.allocate`.
    unsafe { (hooks.deallocate)(object) };
}
