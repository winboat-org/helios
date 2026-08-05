//! `d3d12umddi` DDI types, generated from the SDK's `d3d12umddi.h` by bindgen
//! (see `build.rs::generate_d3d12umddi_bindings`).
//!
//! These are the exact ABI structs the OS D3D12 runtime lowers app and DWM
//! calls into: `D3D12DDIARG_OPENADAPTER`, the `_0109` create-device arg with its
//! `D3DDDI_DEVICECALLBACKS`, the DDI function tables, and the 43-enumerator
//! `D3D12DDICAPS_TYPE` caps gauntlet.
//!
//! # This module is the stage's whole deliverable
//!
//! `build.rs` sets `layout_tests(true)`, so bindgen emits a compile-time
//! size/alignment/offset assertion for every generated type. **If this crate
//! compiles, the D3D12 DDI ABI is machine-checked against the header.**
//!
//! `ARCHITECTURE.md` §12 rule 1 is *never hand-transcribe a DDI ABI struct*, and
//! R908 (`e315d03`) is what that cost: five hand-written `D3d12Ddi*` structs and
//! seven hand-transcribed `D3D12DDICAPS_TYPE_*` values, deleted. §12 rule 2 is
//! the other half — a table filled to the wrong size is *"a 376..392 byte
//! out-of-bounds write into the runtime's heap"*.
//!
//! ⚠ What these assertions do NOT catch, stated because `umd/src/ddi.rs` had to
//! learn it: bindgen derives both the struct and its assertion from the same
//! header, so they are self-consistent by construction. They catch **bindgen
//! disagreeing with the C compiler**, not the ABI moving under us. Anything this
//! crate ends up depending on *positionally* needs its own `const _: () =
//! assert!(…)` beside the code that depends on it, exactly as `umd/src/ddi.rs`
//! pins `D3D10DDIARG_CREATEDEVICE`'s offsets. There is nothing to pin yet — no
//! DDI code exists.
//!
//! # ⛔ Generated, unreachable, and that is correct
//!
//! Nothing in this crate uses these types. `OpenAdapter12` still refuses and
//! must keep refusing until S5 (`DECISIONS.md` §7.1). `#![allow(dead_code)]`
//! below is therefore about generated ABI definitions with no call sites — it is
//! **not** the `#[allow(unreachable_code)]` R908 deleted, which hid a
//! hand-written *body* the compiler had already proved dead.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
// 28 of these, all generated: bindgen emits `use self::_FOO as FOO;` aliases for
// C enums whose typedef name differs from their tag name, and this crate names
// none of them yet. ⚠ `umd/src/ddi.rs` needs no such allow because the D3D11
// header has no such enums — the difference is in the headers, not in how the
// two crates are configured.
#![allow(unused_imports)]
// 9 of these, all in bindgen's generated bitfield accessors — e.g.
// `DXGK_ERROR_CODE::GeneralErrorCode()` transmuting a `u32` to an enum. Generated
// code, not ours; the alternative is post-processing bindgen's output, which
// would put a hand edit between the header and the ABI this stage exists to
// machine-check.
#![allow(unnecessary_transmutes)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/d3d12umddi.rs"));
