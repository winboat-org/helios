/* SEH shims for Windows memory-manager calls that raise on failure.
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
typedef unsigned long ULONG;

PMDL
IoAllocateMdl(
    PVOID VirtualAddress,
    ULONG Length,
    unsigned char SecondaryBuffer,
    unsigned char ChargeQuota,
    PVOID Irp);

void IoFreeMdl(PMDL Mdl);

void
MmProbeAndLockPages(
    PMDL MemoryDescriptorList,
    signed char AccessMode,
    int Operation);

void MmUnlockPages(PMDL MemoryDescriptorList);

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

/*
 * Acquire an independent, long-lived lock on an ordinary-RAM byte range and
 * keep one kernel mapping for the lock's lifetime.
 *
 * The input VA is only needed while this function runs.  On success the MDL's
 * PFN array owns the page lock and the returned system VA remains valid until
 * helios_unlock_system_buffer is called.  This is intentionally different
 * from remembering PFNs from somebody else's paging MDL: MmProbeAndLockPages
 * increments the memory manager's real lock/reference state for this MDL.
 *
 * KernelMode=0, IoWriteAccess=1, MmCached=1, NormalPagePriority=16 and
 * MdlMappingNoExecute=0x40000000 are stable WDK ABI values.  MmCached is only a
 * request for pages that do not already have a cache attribute; ordinary RAM
 * already does, so the memory manager preserves the established attribute.
 */
PMDL
helios_lock_system_buffer_seh(
    PVOID VirtualAddress,
    ULONG Length,
    PVOID *MappedSystemAddress)
{
    PMDL mdl;
    PVOID mapping;

    if (!VirtualAddress || !Length || !MappedSystemAddress)
        return (PMDL)0;
    *MappedSystemAddress = (PVOID)0;

    mdl = IoAllocateMdl(VirtualAddress, Length,
                        /*SecondaryBuffer*/ 0, /*ChargeQuota*/ 0, (PVOID)0);
    if (!mdl)
        return (PMDL)0;

    __try {
        MmProbeAndLockPages(mdl, /*KernelMode*/ 0, /*IoWriteAccess*/ 1);
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        IoFreeMdl(mdl);
        return (PMDL)0;
    }

    mapping = MmMapLockedPagesSpecifyCache(
        mdl, /*KernelMode*/ 0, /*MmCached*/ 1, (PVOID)0,
        /*BugCheckOnFailure*/ 0,
        /*NormalPagePriority | MdlMappingNoExecute*/ 0x40000010UL);
    if (!mapping) {
        MmUnlockPages(mdl);
        IoFreeMdl(mdl);
        return (PMDL)0;
    }

    *MappedSystemAddress = mapping;
    return mdl;
}

void
helios_unlock_system_buffer(PMDL Mdl)
{
    if (!Mdl)
        return;
    /* MmUnlockPages also releases a system mapping owned by this MDL. */
    MmUnlockPages(Mdl);
    IoFreeMdl(Mdl);
}
