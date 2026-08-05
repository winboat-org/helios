/* bindgen wrapper for the D3D12 user-mode display driver DDI.
 *
 * Pulls in the SDK's d3d12umddi.h -- the DDI the OS D3D12 runtime
 * (d3d12.dll + D3D12Core.dll) lowers app calls into -- so bindgen can generate
 * Rust types for the DDI tables, the OPENADAPTER/CREATEDEVICE arg structs, the
 * caps enums and the runtime callback tables.
 *
 * ⛔ THE LAYOUT ASSERTIONS ARE THE DELIVERABLE. `ARCHITECTURE.md` §12 rule 1:
 * never hand-transcribe a DDI ABI struct. R908 (`e315d03`) deleted five
 * hand-written `D3d12Ddi*` structs and seven hand-transcribed
 * `D3D12DDICAPS_TYPE_*` values for exactly this reason, and §12 rule 2 records
 * what a wrong table size costs: "a 376..392 byte out-of-bounds write into the
 * runtime's heap".
 *
 * ⚠ THIS HEADER DOES NOT COMPILE IN USER MODE OUT OF THE BOX, which is the one
 * thing worth knowing before touching it. d3d12umddi.h pulls d3dkmddi.h, which
 * uses NTSTATUS -- a type the um `windows.h` does not define. The fix is the
 * same incantation `umd/bindgen/d3d10umddi_wrapper.h:14-18` already uses for
 * the D3D11 side; it is repeated rather than shared because the two wrappers
 * include different DDI headers and a shared wrapper would be a header that
 * pulls BOTH DDIs into BOTH drivers.
 *
 * Built against SDK 10.0.26100 um+shared under clang, matching the pin every
 * `d3d12umddi.h:NNNN` citation in docs/dx12/ is written against
 * (`DECISIONS.md`'s staging note). */
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

/* d3dkmddi.h (pulled by d3d12umddi.h) uses NTSTATUS, which the um windows.h
 * does not define. Provide it the same way the D3D11 wrapper does. */
#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif

#include <d3d12umddi.h>
