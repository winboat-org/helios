# The Exit 8: psychedelic output in exclusive fullscreen

Date: 2026-08-13
Status: Resolved and manually validated

## Symptom

The Exit 8 renders correctly in windowed and borderless modes. In exclusive
fullscreen at 1280x800, the geometry remains correct but the output becomes
strongly posterized with neon colors. The artifact is visible in the QEMU/VNC
output and is stable from frame to frame.

## Diagnosis

This is a pixel-format identity failure, not a shader, gamma, compression, or
game-rendering failure.

The game produces a `DXGI_FORMAT_R10G10B10A2_UNORM` surface (DXGI value 24).
Its packed 10:10:10:2 words reach the fullscreen scanout unchanged, while the
KMD and QEMU describe that scanout as an 8:8:8:8 AR24 surface (the live KMD
value is DXGI 87, `B8G8R8A8_UNORM`). QEMU consequently treats the first three
bytes of each packed 10-bit pixel as ordinary color bytes. That bytewise
reinterpretation is the psychedelic image.

The end-to-end defect is proven. A traced reproduction identifies the transfer
as DXGI's cross-format fullscreen blit from Vulkan format 64
(`A2B10G10R10_UNORM_PACK32`) to Vulkan format 37 (`R8G8B8A8_UNORM`). The old
UMD implemented this with D3D11 `CopySubresourceRegion`; DXVK correctly treated
the equal-sized color texels as copy-compatible and emitted a raw image copy.
The KMD cannot repair the values after those packed bits have entered an
8-bit-labeled fullscreen primary.

## Paired-capture oracle

Two captures of the same paused frame localize the fault:

- `Graphics.CopyFromScreen` inside the guest is clean.
- A raw VNC capture from QEMU is psychedelic.
- Both images are 1280x800 and have identical geometry.

The session artifacts are `tmp/screen_copy.png` and
`tmp/exit8-host-vnc.png`. Comparing the same coordinates gives an exact packed
pixel signature:

| coordinate | clean guest RGB | quantized 10-bit RGB | packed RGB10A2 word | host RGB |
| --- | --- | --- | --- | --- |
| (0, 349) | (232, 232, 232) | (931, 931, 931) | `0xfa3e8fa3` | (163, 143, 62) |
| (739, 4) | (170, 170, 170) | (682, 682, 682) | `0xeaaaaaaa` | (170, 170, 170) |
| (636, 334) | (127, 127, 127) | (509, 509, 509) | `0xdfd7f5fd` | (253, 245, 215) |
| (740, 441) | (64, 64, 64) | (257, 257, 257) | `0xd0140501` | (1, 5, 20) |

For example, `0xfa3e8fa3` is stored little-endian as `a3 8f 3e fa`.
Interpreting those bytes as an 8-bit RGB pixel produces exactly
`(163, 143, 62)`, the host capture. Across the asynchronous full-frame pair,
87,267 pixels match this transformation exactly; temporal and capture-time
differences account for the rest.

## Live format evidence

The Exit 8 UMD log records the fullscreen buffers as format 24:

```text
DDI create_resource(tex2d): 1280x800 fmt=24 ...
DDI allocate_wddm_resource ... 1280x800 fmt=24 ...
CreateDdiScanoutTexture2D REFUSED: fmt=24 is not a 32bpp scan-out format
scanout-snapshot: ring slot 0 create FAILED 1280x800 fmt=24
DXGI Present: #0 src=0x40003040 ... copied=false flags=0x1
```

The current KMD diagnostics instead describe the active direct-flip ring as
8-bit BGRA:

```text
PBsrc=37  PBsFmt=87  PBsPch=5120  PBsSz=4587520  PBsDir=1
ScRid=37  ScDir=1    ScPch=5120   ScCpy=2
VpFlip=42561  VpMmio=42561  VpDmaF=0  SnSub=0
```

QEMU receives the same surface as DRM fourcc `0x34325241`, or `AR24`, and
successfully imports it as a 1280x800 OPTIMAL DMA-BUF. The transport and image
shape are therefore healthy; the declared format is wrong for the payload.

## Code-path findings

The existing KMD conversion machinery is capable of handling this format:

- `PresentPixelFormat::from_dxgi(24)` maps to Vulkan
  `A2B10G10R10_UNORM_PACK32` in `kmd_render/src/virtio/venus/present.rs`.
- The scanout/present copy code compares the exact source and destination
  Vulkan formats and uses an image blit for numeric conversion when they
  differ.
- It is bypassed here because the allocation reaching the direct-flip scanout
  path is already recorded as DXGI 87.

The format was lost in the UMD handoff because:

- `ScanoutFormat::from_dxgi` and the direct-scanout creation gates accept only
  DXGI 28, 87, and 88. The format-24 snapshot is refused even though it is four
  bytes per pixel.
- The former `dxgi_blt` implementation always called
  `CopySubresourceRegion`, did not branch on conversion flags, and did not
  reject different source/destination formats.
- The former `dxgi_blt1` explicitly returned `DXGI_ERROR_UNSUPPORTED` for
  `BLT_CONVERT`; its non-convert arm was also a raw `CopySubresourceRegion`.
- `rotate_resource_backings` swaps DXVK image storages and layouts without a
  format-compatibility check. The Rust allocation/private records rotate in
  lockstep, but a mixed-format runtime handoff has no fail-closed guard.
- The live Present path reports `copied=false`, while rotation runs every
  frame. The log also shows both format-24 game buffers and format-28/87
  fullscreen primary/proxy buffers at the same 1280x800 geometry.

These paths explain how a four-byte-per-pixel copy can preserve every packed
10-bit bit while changing only the metadata used by scanout.

## Implemented correction

The correction preserves DXGI 24 until an explicit numeric conversion to the
canonical 8-bit scanout format has completed:

1. `dxgi_blt` and `dxgi_blt1` classify known unequal formats as numeric
   conversions. Same-format operations retain the ordinary copy path.
2. `DxvkContext::copyImageConverted` creates views in each image's native
   format and calls the Vulkan numeric blit path with nearest filtering.
3. Format-24 snapshot rings allocate format-28 slots and run the same numeric
   conversion before publication. Format 24 remains outside the AR24 direct
   scanout whitelist; it is accepted only as a conversion source.
4. `rotate_resource_backings` refuses mixed format, type, extent, sample count,
   mip count, layer count, or array size before changing any backing identity.

An intermediate implementation forced DXVK's private `copyImageFb` helper. A
live test correctly produced black rather than psychedelic output. Diagnostics
showed why: `copyImageFb` emulates *copy* semantics by creating both color
views in the destination format. The exported RGB10 WDDM image cannot be
relocated to add an RGBA8 reinterpretation view, so `ensureImageCompatibility`
refused the operation and left the new destination untouched. This was not a
KMD/QEMU transport failure: the new game process had queued the conversion,
all Presents succeeded, and a guest GDI capture remained correct, while raw
QEMU/VNC was all black. Native-format blit views remove that invalid relocation
and provide the required value conversion.

A future true 10-bit direct-scanout path is possible only if the exact 10-bit
DRM fourcc and Vulkan format are carried through KMD, QEMU, readback, EGL/VNC,
and capture. Advertising the current AR24 contract for packed 10-bit memory is
not such a path.

## Live validation

Revision 3 was built as a complete signed driver package and deployed as a
hash-addressed UMD after a controlled adapter restart:

```text
SHA256 0708FD6F708EB31CD4BA0F75072DEEB48B5048950DA8813485F9AA411ECE36FB
C:\ProgramData\HeliosUmd\helios_umd_0708fd6f708eb31c.dll
```

The fresh `Exit8-Win64-Shipping.exe` process loaded that exact module. Its UMD
log built a 1280x800 snapshot ring with `src_fmt=24 scanout_fmt=28`, with no
ring, copy, cache, or private-data failures. A raw RFB capture from QEMU—not a
guest-side screenshot—then showed the normal corridor with correct neutral
colors. The final clean capture had 823,640/1,024,000 non-black pixels
(80.43%), versus 0/16,000 sampled pixels lit in the failed intermediate build.
After a later WinBoat container recreation and fresh VM boot, the user repeated
the exclusive-fullscreen test through VNC and confirmed that The Exit 8 still
rendered correctly. Windowed and borderless behavior remained unaffected.

## Follow-up regression plan

- Add a small D3D11 fullscreen probe that presents known format-24 gray/color
  ramps. Compare guest and host pixels, including the four values above.
- Exercise The Exit 8 in exclusive fullscreen, borderless, and windowed modes.
- Regression-test an ordinary 8-bit exclusive-fullscreen title and the DWM
  desktop/windowed-Blt path.
- Assert that every published AR24 surface has a format-28/87/88 producer or an
  explicit completed conversion from format 24.
- Assert that cross-format `RotateResourceIdentities` input is rejected and
  counted rather than silently rotated.
