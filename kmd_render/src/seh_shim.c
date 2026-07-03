/* SEH shim for MmMapLockedPagesSpecifyCache(UserMode).
 *
 * For UserMode mappings, MmMapLockedPagesSpecifyCache RAISES an exception on
 * failure (e.g. address-space exhaustion in the target process) instead of
 * returning NULL. The KMD is a no_std Rust crate with no SEH support, so an
 * unhandled raise unwinds into the kernel and bugchecks — reachable from ANY
 * process via the D3DKMTEscape MAP_BLOB verb. This C shim converts the raise
 * into a NULL return the Rust caller already handles.
 *
 * Deliberately header-free: the WDK include paths are not plumbed into the cc
 * build, and the shim needs exactly one prototype. Types match the x64 WDK ABI:
 * KPROCESSOR_MODE = CCHAR (signed char), MEMORY_CACHING_TYPE = enum (int),
 * ULONG = unsigned long (32-bit on MSVC). __C_specific_handler is provided by
 * ntoskrnl.lib, which the Rust link already pulls in.
 */

typedef void *PVOID;
typedef struct _MDL *PMDL;

PVOID
MmMapLockedPagesSpecifyCache(
    PMDL MemoryDescriptorList,
    signed char AccessMode,
    int CacheType,
    PVOID RequestedAddress,
    unsigned long BugCheckOnFailure,
    unsigned long Priority);

#define EXCEPTION_EXECUTE_HANDLER 1

PVOID
helios_mm_map_locked_pages_user_seh(
    PMDL Mdl,
    signed char AccessMode,
    int CacheType,
    unsigned long Priority)
{
    __try {
        return MmMapLockedPagesSpecifyCache(Mdl, AccessMode, CacheType,
                                            (PVOID)0, /*BugCheckOnFailure*/ 0,
                                            Priority);
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        return (PVOID)0;
    }
}
