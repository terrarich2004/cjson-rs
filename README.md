# cjson-rs

Safe Rust implementation of `cJSON` (v1.7.19 ABI) providing both a pure-Rust library (`cjson-core`, `#![forbid(unsafe_code)]`) and a drop-in C shared/static library (`libcjson`).

## Workspace Layout

- `crates/cjson-core` — pure safe Rust (`#![forbid(unsafe_code)]`) JSON parser, serializer, minifier, number/string codecs, and `Value` / `ParseError` types. Zero external crate dependencies.
- `crates/cjson-ffi` — C ABI layer (`cdylib`, `staticlib`, `rlib`) named `cjson`, exporting all 77 symbols declared in `include/cJSON.h`.
- `include/cJSON.h` — standard C header for compiling and linking C/C++ callers against `libcjson.so`, `libcjson.a`, or `cjson.dll`.
- `examples/` — standalone C and Rust usage examples.

## Key Details

- **Header and struct ABI compatibility**: `cJSON` and `cJSON_Hooks` use `#[repr(C)]` with the exact field layout of `include/cJSON.h`, so C macros and direct field accesses (`item->next`, `item->child`, `item->type`, `item->valuestring`, `item->valueint`, `item->valuedouble`, `item->string`, `cJSON_ArrayForEach`) work identically.
- **Custom allocators (`cJSON_InitHooks`)**: Supports user-supplied `malloc_fn` and `free_fn`. When default `malloc`/`free` are active, allocations follow a lock-free atomic fast path with `realloc`; when custom hooks are installed, `realloc` is disabled and buffers grow via `allocate` + `memcpy` + `deallocate`.
- **No panics across FFI**: All `extern "C"` functions handle `NULL` pointers, truncated buffers, invalid UTF-8/UTF-16 sequences, and syntax errors without unwinding. Errors return `NULL`, `0`, or `NaN` and populate `cJSON_GetErrorPtr()` where specified.
- **Boundary numbers**:
  - `NaN`, `+Infinity`, and `-Infinity` serialize as `"null"`.
  - `-0.0` with `valueint == 0` serializes as `"0"`.
  - `valueint` saturates to `[INT_MIN, INT_MAX]` (`NaN` maps to `INT_MIN`, matching x86_64 C `(int)NAN`).
  - Finite floats format using `%1.15g` with a round-trip check within `DBL_EPSILON`, falling back to `%1.17g`.
- **UTF-16 escapes and BOM**: Decodes surrogate pairs (such as `\uD83D\uDE00`) to UTF-8, rejects lone surrogates with exact `cJSON_GetErrorPtr()` offsets, and skips a leading UTF-8 BOM (`\xEF\xBB\xBF`) when present.
- **Nesting limit**: Rejects arrays/objects exceeding `CJSON_NESTING_LIMIT` (`1000`) to prevent stack overflow, matching `cJSON` (depth `1000` succeeds, `1001` fails).

## Building & Testing

```sh
cargo build --release --workspace
cargo test --workspace
```

The release artifacts are written to `target/release/libcjson.so` and `target/release/libcjson.a` (`cjson.dll` / `cjson.lib` on Windows). The test suite includes 2,104 compatibility cases in `crates/cjson-ffi/tests/fixtures/compat_cases.jsonl` covering boundary numbers, deep nesting, UTF-16 surrogates, `NULL` pointers, custom allocator hooks, and malformed JSON.

## Using from C / C++

Compile and run `examples/c_usage.c` against `target/release/libcjson.so`:

```sh
gcc examples/c_usage.c -Iinclude -Ltarget/release -lcjson -Wl,-rpath,target/release -o c_usage
./c_usage
```

Example (`examples/c_usage.c`):

```c
#include <stdio.h>
#include "cJSON.h"

int main(void) {
    cJSON *root = cJSON_CreateObject();
    cJSON_AddStringToObject(root, "status", "ok");
    cJSON_AddNumberToObject(root, "abi_patch", 19);

    char *out = cJSON_Print(root);
    printf("%s\n", out);

    cJSON_free(out);
    cJSON_Delete(root);
    return 0;
}
```

## Using from Rust

Add `cjson-core` as a dependency or run the workspace example:

```sh
cargo run --example rust_usage
```

Example (`examples/rust_usage.rs`):

```rust
use cjson_core::{Value, minify, parse, to_string_pretty};

fn main() {
    let value = parse(r#"{"project":"cjson-rs","emoji":"\uD83D\uDE00"}"#).expect("valid JSON");
    println!("{}", value.get("emoji").and_then(Value::as_str).unwrap_or(""));

    let built = Value::Object(vec![
        ("status".to_string(), Value::String("ok".to_string())),
        ("safe".to_string(), Value::Bool(true)),
    ]);
    println!("{}", to_string_pretty(&built));
    println!("{}", minify("{ \"a\" : 1 } // comment\n"));
}
```

## Benchmarks

Measured against the original C implementation compiled with `-O2` across 2,104 cases (`min 200ms` per function):

| Operation | Cases | C (`ns/call`) | Rust (`ns/call`) | Speedup |
| --- | ---: | ---: | ---: | ---: |
| `create_number_print_hash` | 43 | 400 | 272 | 1.47x |
| `nesting_depth_ok` | 25 | 6630 | 5490 | 1.21x |
| `parse_type` | 122 | 282 | 237 | 1.19x |
| `hooks_roundtrip` | 95 | 691 | 588 | 1.17x |
| `print_hash` | 251 | 666 | 576 | 1.16x |
| `print_buffered_hash` | 142 | 444 | 390 | 1.14x |
| `parse_number_int` | 94 | 55 | 49 | 1.13x |
| `parse_error_offset` | 211 | 70 | 62 | 1.13x |
| `parse_number_double` | 93 | 57 | 52 | 1.09x |
| `print_len` | 146 | 713 | 656 | 1.09x |
| `duplicate_hash` | 135 | 652 | 607 | 1.08x |
| `parse_length_error_offset` | 158 | 43 | 43 | 1.01x |
| `print_preallocated_hash` | 131 | 67 | 67 | 1.01x |
| `array_ops_hash` | 73 | 72 | 72 | 0.99x |
| `compare` | 101 | 88 | 94 | 0.93x |
| `minify_hash` | 75 | 201 | 229 | 0.88x |
| `object_ops_hash` | 89 | 63 | 77 | 0.81x |
| `string_value_hash` | 77 | 36 | 47 | 0.76x |
| `create_number_int` | 43 | 9 | 16 | 0.57x |
| **Overall (geometric mean)** | **2104** | — | — | **1.02x** |

## License

MIT License.

Copyright (c) 2026 terrarich2004

Original cJSON header and specification:
Copyright (c) 2009-2017 Dave Gamble and cJSON contributors
