# TODO

What is known to be left. Each security item is a test case marked `//@ todo: <why>` (or `//@ todo(valgrind):` when
only valgrind sees the failure): `cargo test --release --test security` runs it and reports it as TODO without
failing. Once a case passes, the test fails until its `todo` line and its entry here are removed, so this list can
only shrink. `todo_md_lists_every_todo` checks that every marked case is named here. How a case reads:
[tests/security/README.md](tests/security/README.md). Background: [docs/reports/2026-11-21-01-security-cves.md](docs/reports/2026-11-21-01-security-cves.md).

## Security: holes zustc does not catch yet (`tests/security/`)

These are in the type and borrow checks that the compat and region-check work changes, so they were left there.

### Borrow and region checking

| case | what is accepted that rustc rejects |
|---|---|
| `borrow_closure_escapes` | E0373 is only checked for `thread::spawn`: a boxed closure borrowing a local is returned |
| `borrow_closure_mut_and_shared` | a closure's captures are not loans: `v` is read while a closure holds `&mut v` |
| `borrow_slice_into_vec_then_clear` | `&v[..]` is not a loan of `v`: a `clear` (dropping the elements) is accepted under it |
| `region_temporary_dropped` | E0716 is not found when the reference is assigned after its `let` |
| `region_dropck` | no drop check: a value with Drop may outlive what it borrows |
| `region_block_tail_escapes` | a loan held by a block's local, or by what a by-value method returns (`Some(&v).unwrap()`), is not followed out of the block's tail (E0597) |
| `borrow_uninit_read` | rejected, but reported at the function's first line instead of the read |

### Privacy

| case | what is accepted that rustc rejects |
|---|---|
| `privacy_field` | reading a private field from outside its module (E0616) |
| `privacy_field_write` | writing a private field from outside its module (E0616) |
| `privacy_ctor` | building a struct with private fields from outside (E0451) |
| `privacy_fn` | calling a private function from outside (E0603) |

Visibility is not checked at all. Unsafe code inside a module relies on safe code outside not touching its
invariants.

### Traits, types and the unsafe boundary

| case | what is accepted that rustc rejects |
|---|---|
| `unsafe_fn_pointer` | fn pointer types do not record `unsafe`, so calling an `unsafe fn` pointer needs no unsafe block |
| `send_dyn_thread` | `dyn Trait` types do not keep `+ Send`, so a `Box<dyn Fn()>` (which may hold an `Rc`) moves to a thread |
| `cve_2019_12083_error_type_id` | an impl may define a method its trait does not declare (E0407). zustc has no downcast, so no type confusion follows |
| `lit_overflow` | `let x: u8 = 256;` (overflowing_literals) is accepted and wraps to 0 |
| `const_overflow` | overflow in a const (E0080) is found only when it is evaluated at run time (a panic) |
| `unsafe_nested_fn_in_unsafe_block` | rejected with E0133, but at a wrong line (a nested fn's lines are counted from its item) |
| `unsafe_union_pattern` | rejected, but as an unsupported pattern (a union pattern of one field), not as E0133 |

### Runtime

| case | what goes wrong |
|---|---|
| `rt_box_move_out` | `let x = *b;` moves the value but never frees the Box's block (a leak; valgrind) |
| `rt_box_move_patterns` | the same leak through patterns (`let Pair { a, b } = *b`, `match *b`); no double free (valgrind) |
| `rt_pattern_partial_move` | what a pattern leaves to `_` / `..` (`let (a, _) = pair();`, `let W { a, .. } = w;`) is never dropped (a leak) |

## Security: CVEs whose API zustc does not have (`expect: unsupported`)

These cannot happen today. Each case fails as soon as zustc gets the API, and must then become a test of the CVE's
behavior: `cve_2020_36323_join_borrow` (join over a `Borrow<str>` of the program's), `cve_2021_28875_read_to_end`
(a `Read` impl of the program's), `cve_2021_28876_28878_zip_both_ends` (`Zip::next_back`), `cve_2021_29922_ip_octal`
(`std::net`), `cve_2022_21658_remove_dir_all` and `rt_char_boundary_get` (`str::get`). The full table is
[tests/security/cves.txt](tests/security/cves.txt).

## wasm32-unknown-unknown (`crates/wasm`, `tests/wasm/`)

- `Op::Fma` is a multiply and an add, not a fused multiply-add: `f64::mul_add` can differ from the native build in
  the last bit. Use a correctly rounded helper, or reject `mul_add` on wasm until there is one.
- Speed: every block is a case of one `loop` + `br_table`, and every value a local (3–15x slower than native under
  Node). A relooper or stackifier, and reuse of locals.
- Not supported yet:
  - i128 division and i128 to / from float;
  - threads (green threads need `swapcontext`);
  - workspace builds (`zustc build`);
  - zustc built as wasm by itself.
- Traps carry no panic message (the native build prints the location from its trap table).
- `wasm32-wasip1` / `wasm32-wasip2` run under wasmtime via a synthesized `_start` (`tests/driver2_wasm.rs`).
  No stdin, files or arguments yet: `getchar` / `read` are EOF, and `open` is not provided, so `std::env::args`
  and `std::fs` do not run. `fd_read` / `args_get` of preview1 are the next step.

## Tooling

- `scripts/docs_check.sh` without an API key skips a manifest whose record is out of date. jev-lint rejects the whole
  record then, so a recorded contradiction in an unchanged pair of that manifest is not seen until someone with a key
  records it again. (A record that is current but judged `insufficient_context` still fails.)
- `tests/selfhost_compiler.sh` fails on main: the combined source has no `regions` module (`crate::regions::...` is
  not found).
