<!--
  SPDX-License-Identifier: Apache-2.0
  SPDX-FileCopyrightText: 2026 The Gleam contributors
-->

# The zig compilation target

This fork adds `--target zig` beside `erlang` and `javascript`: Gleam
compiles to Zig source, which compiles to a native binary. Scope is parity
with the JavaScript target — no BEAM, no OTP, no schedulers; async is a
future FFI package's job, the way `gleam_javascript` wraps promises.

## Status

- **Language**: everything the JavaScript target supports except bit
  arrays (they compile, but constructing or matching one panics at
  runtime). Custom types, pattern matching with guards and alternatives,
  closures with captures, pipes, `use`, `let assert`, record updates,
  module constants, self-tail-calls compiled to loops.
- **Memory**: Perceus-style reference counting, no GC. Naive counting
  plus conservative last-use moves plus FBIP cons-cell reuse
  (`list.map` mutates in place when the list is unshared). The generated
  binary leak-checks itself on exit during development.
- **Verified**: the [language tour](https://tour.gleam.run) corpus
  (60/63 lessons; 2 have no main function, 1 needs bit arrays) and 61
  rosetta-code programs produce output identical to the JavaScript
  target, leak-clean. 6,180 compiler tests pass.
- **Stdlib**: a [forked gleam_stdlib](https://github.com/escherize/gleam-zig-stdlib)
  implements the FFI for io, int, float, string, string_tree and dict.
  Modules that are pure Gleam work as-is.

## Trying it

Requirements: Rust toolchain, [Zig 0.16.0](https://ziglang.org/download/)
exactly (the generated code tracks one pinned Zig version).

```sh
git clone https://github.com/escherize/gleam-zig
git clone https://github.com/escherize/gleam-zig-stdlib
cd gleam-zig && cargo build --release -p gleam
```

A project's `gleam.toml`:

```toml
name = "hello"
version = "1.0.0"
target = "zig"

[dependencies]
gleam_stdlib = { path = "../gleam-zig-stdlib" }
```

Run it (`GLEAM_ZIG` points at the zig binary; defaults to `zig` on PATH):

```sh
GLEAM_ZIG=/path/to/zig-0.16.0/zig gleam run --target zig
```

## FFI

`@external(zig, "./my_ffi.zig", "function_name")` with the path relative
to the generated module in the build directory. The convention:

- One `Value` parameter per Gleam argument, returning `Value`.
- Arguments are **borrowed**; the return value is **owned**. Anything a
  result retains from an argument must be `P.dup`'d.
- `Value` and the runtime live in `prelude.zig`, importable from FFI
  files as `@import("../prelude.zig")`.

## Known gaps

- Bit arrays have no representation yet.
- Mutual tail recursion can overflow the stack (JavaScript-target
  parity; self-recursion is safe).
- Dict is an association list; grapheme functions segment by codepoint;
  case/trim functions are ASCII-only.
- Ints are wrapping i64 (JavaScript uses f64, Erlang has bignums —
  every target picks a pragmatic representation).
- The Perceus passes still to come: branch-aware last-use dataflow,
  drop specialization, record/tuple reuse, borrowing inference.

## Where things live

| What | Where |
|---|---|
| Code generation | `compiler-core/src/zig.rs` |
| Runtime (Value, RC, echo) | `compiler-core/templates/prelude.zig` |
| Stdlib FFI | [gleam-zig-stdlib](https://github.com/escherize/gleam-zig-stdlib) `src/gleam_stdlib.zig` |
| Design notes, corpus, harness | [gleam-zig-workspace](https://github.com/escherize/gleam-zig-workspace) |
