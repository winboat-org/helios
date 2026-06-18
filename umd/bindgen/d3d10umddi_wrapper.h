/* bindgen wrapper for the D3D10/11 user-mode display driver DDI.
 *
 * Pulls in the WDK d3d10umddi.h (the d3d10umddi DDI the OS D3D runtime lowers
 * app calls into) so we can generate Rust types for the device-funcs tables,
 * the CREATEDEVICE/OPENADAPTER arg structs, and the runtime callback tables.
 *
 * Built against the WDK 10.0.26100 um+shared include trees under clang. The
 * DXGKDDI interface version self-defaults to WDDM 3.2 via the d3dkmthk chain
 * (matches the OS ABI), same as the Gate-5a d3dkmt header vendoring. */
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

/* d3dkmddi.h (pulled by d3d10umddi.h) uses NTSTATUS, which the um windows.h does
 * not define. Provide it the same way the Gate-5a d3dkmt vendoring did. */
#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif

#include <d3d10umddi.h>
