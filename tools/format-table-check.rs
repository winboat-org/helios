//! Run `umd/src/format.rs`'s equivalence tests on the Linux host.
//!
//! `helios_umd` is `crate-type = ["cdylib"]` with `panic = "abort"` in both
//! profiles and has no test harness; a `cargo test` there would drag in the WDK
//! bindgen run and the cxx/DXVK bridge, and cannot run off Windows at all.
//! `format.rs` is deliberately free of `windows`-crate and WDK types precisely
//! so it can be included here instead.
//!
//!   rustc --test --edition 2021 -o /tmp/format-table-check \
//!       tools/format-table-check.rs
//!   /tmp/format-table-check
//!
//! `--edition 2021` matters: it is the crate's own edition, and without it the
//! captured-identifier `assert!` messages are silently not format strings.
//!
//! Seconds, on the host, no VM. This is R1010's primary proof: the table is
//! asserted against verbatim copies of the eight pre-change predicates over
//! every format number 0..=200.

// `format.rs` is `pub` throughout (it lives in `umd_common` and is consumed by
// two cdylibs); the readers are exercised only by its own `#[cfg(test)] mod
// tests`, so everything else is dead to this single-file crate.
#![allow(dead_code)]

// `#[path]` rather than `include!`: the file is a real module, so its own `//!`
// documentation stays where an inner doc comment is legal.
#[path = "../umd_common/src/format.rs"]
mod format;

fn main() {
    println!("run with `rustc --test`; see the module doc");
}
