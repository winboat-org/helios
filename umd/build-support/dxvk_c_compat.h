/* Forced-include shim for building DXVK's C subprojects (libdisplay-info) under
 * clang-cl / MSVC. libdisplay-info uses the POSIX `ssize_t`, which the MSVC CRT
 * does not define. Map it to the Win32 signed-size type. Applied via
 * `-Dc_args=/FI<this>` in the DXVK meson configure (Gate 5b UMD bring-up). */
#pragma once
#include <BaseTsd.h>
typedef SSIZE_T ssize_t;
