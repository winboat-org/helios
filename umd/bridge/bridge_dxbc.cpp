// DXBC container synthesis, and nothing else.
//
// Moved verbatim out of `dxvk_bridge.cpp` by T8/R1105. This translation unit
// is compiled WITHOUT `dxvk_instance.h`, `dxvk_device.h`, `d3d11_device.h` or
// `d3d11_context_imm.h`, which makes "the signature encoder cannot touch the
// DXVK device or the immediate context" a link-time fact rather than a review
// convention -- and makes an off-VM test of `append_signature_chunk` possible
// for the first time.
//
// Honest limit, exactly as the item states it: this still needs
// `dxbc/dxbc_container.h` and `dxbc_spv::dxbc::hashDxbcBinary` from the
// dxbc-spirv subproject, so the firewall is against DXVK and COM, not against
// all third-party code.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include <array>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <string>
#include <vector>

#include "dxbc/dxbc_container.h"

#include "bridge_common.h"
#include "bridge_dxbc.h"

namespace dxbc_spv::dxbc {
  util::md5::Digest hashDxbcBinary(const void* data, size_t size);
}

namespace helios_bridge {

  const char* shader_bytecode_dump_path() {
    static std::string path = [] {
      char value[MAX_PATH] = {};
      DWORD size = sizeof(value);
      if (RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", "ShaderBytecodeDumpPath",
                       RRF_RT_REG_SZ, nullptr, value, &size) != ERROR_SUCCESS ||
          !value[0])
        return std::string();

      CreateDirectoryA(value, nullptr);
      return std::string(value);
    }();

    return path.empty() ? nullptr : path.c_str();
  }

  void dump_shader_bytecode(
      const char* stage,
      const char* form,
      const void* data,
      std::size_t len) {
    const char* dir = shader_bytecode_dump_path();
    if (!dir || !data || !len)
      return;

    static std::atomic<std::uint32_t> s_seq { 0u };
    const auto seq = s_seq.fetch_add(1u, std::memory_order_relaxed);

    char path[MAX_PATH] = {};
    _snprintf_s(path, sizeof(path), _TRUNCATE,
      "%s\\shader-%lu-%05u-%s-%s-%zu.dxbc",
      dir,
      static_cast<unsigned long>(GetCurrentProcessId()),
      seq,
      stage,
      form,
      len);

    std::ofstream file(path, std::ios_base::out | std::ios_base::binary | std::ios_base::trunc);
    if (!file) {
      umd_log("shader bytecode dump open failed");
      return;
    }

    file.write(reinterpret_cast<const char*>(data), len);
  }

  template<typename T>
  void write_le(std::vector<std::uint8_t>& dst, std::size_t offset, T value) {
    for (std::size_t i = 0; i < sizeof(T); i++)
      dst[offset + i] = std::uint8_t((value >> (8u * i)) & 0xffu);
  }

  // D3D11 has at most 32 input/output registers per stage; a few hundred is
  // generous. The bound exists so the length check below cannot be satisfied by
  // a wrapped sum, since it is the only thing standing between caller-supplied
  // counts and the indexing that follows.
  constexpr std::uint32_t kMaxSignatureEntries = 512u;

  std::atomic<std::uint32_t> g_signatureCountRefused{0};

  bool signature_count_ok(const char* what, std::uint32_t count) {
    if (count <= kMaxSignatureEntries)
      return true;
    const std::uint32_t n =
      g_signatureCountRefused.fetch_add(1, std::memory_order_relaxed) + 1;
    char msg[160];
    std::snprintf(msg, sizeof(msg),
      "%s REFUSED: signature entry count %u exceeds %u (x%u)",
      what, count, kMaxSignatureEntries, n);
    umd_log(msg);
    return false;
  }

  struct EncodedSignatureEntry {
    const char* semantic_name;
    std::uint32_t semantic_index;
    std::uint32_t system_value;
  };

  bool is_patch_signature_chunk(const char tag[4]) {
    return std::memcmp(tag, "PCSG", 4) == 0 || std::memcmp(tag, "PSG1", 4) == 0;
  }

  EncodedSignatureEntry encode_signature_entry(const char tag[4],
                                               std::uint32_t sysval,
                                               std::uint32_t reg) {
    EncodedSignatureEntry encoded = { "TEXCOORD", reg, sysval };

    if (!is_patch_signature_chunk(tag))
      return encoded;

    // D3D11 DDI tessellation signatures carry D3D10_SB_NAME token values
    // (individual final edge/inside factors: 11..22). DXBC container
    // signatures carry the collapsed D3D_NAME reflection values used by
    // dxbc-spv (edge/inside semantic plus semantic index). Without this
    // translation, hull shader tess factors are not declared as SPIR-V
    // TessLevelOuter/Inner built-ins and tessellated draws can disappear.
    switch (sysval) {
      case 11: encoded = { "SV_TessFactor",       0u, 11u }; break; // quad U0 edge
      case 12: encoded = { "SV_TessFactor",       1u, 11u }; break; // quad V0 edge
      case 13: encoded = { "SV_TessFactor",       2u, 11u }; break; // quad U1 edge
      case 14: encoded = { "SV_TessFactor",       3u, 11u }; break; // quad V1 edge
      case 15: encoded = { "SV_InsideTessFactor", 0u, 12u }; break; // quad U inside
      case 16: encoded = { "SV_InsideTessFactor", 1u, 12u }; break; // quad V inside
      case 17: encoded = { "SV_TessFactor",       0u, 13u }; break; // tri U edge
      case 18: encoded = { "SV_TessFactor",       1u, 13u }; break; // tri V edge
      case 19: encoded = { "SV_TessFactor",       2u, 13u }; break; // tri W edge
      case 20: encoded = { "SV_InsideTessFactor", 0u, 14u }; break; // tri inside
      case 21: encoded = { "SV_TessFactor",       0u, 15u }; break; // line detail
      case 22: encoded = { "SV_InsideTessFactor", 0u, 16u }; break; // line density
      default: break;
    }

    return encoded;
  }

  // Append one 24-byte DXBC signature chunk (ISGN/OSGN) built from flattened
  // D3D11_1DDIARG_SIGNATURE_ENTRY2 values. Semantic names are synthesized as
  // "TEXCOORD<register>" — names are only a matching key (the input-layout
  // path fabricates the same convention); the load-bearing fields are the
  // register, mask, system value and, critically, the COMPONENT TYPE the raw
  // token stream cannot express (dwm binds R16G16_SINT vertex data against
  // shaders whose ISGN declares SINT inputs — typing them float32 was
  // VUID-Input-08733 UB that rasterized nothing).
  void append_signature_chunk(std::vector<std::uint8_t>& blob,
                              const char tag[4],
                              const std::uint32_t* entries,
                              std::uint32_t count) {
    // 24 bytes per entry, widened: `count` is caller-supplied and the result
    // feeds `name_base` and every offset written into the chunk.
    if (!signature_count_ok("append_signature_chunk", count))
      return;
    const std::uint32_t entries_size = count * 24u;
    const std::uint32_t name_base = 8u + entries_size;  // relative to chunk data
    std::vector<EncodedSignatureEntry> encoded_entries;
    std::vector<std::uint32_t> name_offsets;
    encoded_entries.reserve(count);
    name_offsets.reserve(count);

    std::uint32_t names_size = 0u;
    for (std::uint32_t i = 0; i < count; ++i) {
      const std::uint32_t* e = entries + std::size_t(i) * kSigEntryWords;
      auto encoded = encode_signature_entry(tag, e[0], e[1]);
      encoded_entries.push_back(encoded);
      name_offsets.push_back(name_base + names_size);
      names_size += std::uint32_t(std::strlen(encoded.semantic_name) + 1u);
    }

    std::uint32_t data_len = name_base + names_size;
    data_len = (data_len + 3u) & ~3u;

    auto put32 = [&blob](std::uint32_t v) {
      blob.push_back(std::uint8_t(v));
      blob.push_back(std::uint8_t(v >> 8));
      blob.push_back(std::uint8_t(v >> 16));
      blob.push_back(std::uint8_t(v >> 24));
    };

    blob.insert(blob.end(), tag, tag + 4);
    put32(data_len);
    const std::size_t data_start = blob.size();
    put32(count);
    put32(8u);
    for (std::uint32_t i = 0; i < count; ++i) {
      const std::uint32_t* e = entries + std::size_t(i) * kSigEntryWords;
      const std::uint32_t sysval = e[0];
      const std::uint32_t reg = e[1];
      const std::uint32_t mask = e[2] & 0xFu;
      const auto encoded = encoded_entries.at(i);
      // UNKNOWN(0) component type: default to float32 (matches the previous
      // behaviour and the D3D convention for untyped registers).
      const std::uint32_t comptype = e[3] ? e[3] : 3u;
      if (e[4]) {
        char msg[96];
        std::snprintf(msg, sizeof(msg),
                      "shader signature entry reg=%u has stream=%u (unencoded)", reg, e[4]);
        umd_log(msg);
      }
      put32(name_offsets.at(i));
      put32(encoded.semantic_index);
      put32(encoded.system_value);
      put32(comptype);
      put32(reg);
      put32(mask | (mask << 8));  // mask | read/write mask, 2 pad bytes
      if (encoded.system_value != sysval || encoded.semantic_index != reg) {
        static std::atomic<std::uint32_t> s_tess_sig_remap_logs { 0u };
        const auto n = s_tess_sig_remap_logs.fetch_add(1u, std::memory_order_relaxed);
        if (n < 64u) {
          char msg[160];
          std::snprintf(msg, sizeof(msg),
            "shader patch signature remap: raw_sv=%u reg=%u -> sig_sv=%u sem=%s%u",
            sysval, reg, encoded.system_value, encoded.semantic_name, encoded.semantic_index);
          umd_log(msg);
        }
      }
    }
    for (const auto& e : encoded_entries) {
      const auto len = std::strlen(e.semantic_name) + 1u;
      blob.insert(blob.end(), e.semantic_name, e.semantic_name + len);
    }
    while ((blob.size() - data_start) < data_len)
      blob.push_back(0);
  }

  // One signature chunk to place ahead of the code chunk.
  struct SignatureChunk {
    const char* tag;                 // exactly four characters
    const std::uint32_t* entries;
    std::uint32_t count;
  };

  // Assemble a DXBC container: the 32-byte file header, the chunk offset table,
  // `N` signature chunks in order, then the code chunk, then the MD5 stamp.
  //
  // The container header validation, the code-tag selection, the offset table,
  // the file-size arithmetic and the MD5 stamping used to exist in three
  // verbatim copies -- one per wrapper -- differing only by which chunks went in
  // front. That divergence had already partially occurred, and it is the kind
  // where a wrong offset produces a container the shader compiler silently
  // misreads.
  //
  // `N` is a template parameter, not a runtime count: the offset table is then
  // a fixed `std::array<std::uint32_t, N + 1>` with no runtime bound to get
  // wrong, and each caller's chunk list is checked at compile time.
  //
  // NOT std::span: the TU is pinned to C++17 by umd/build.rs.
  template <std::size_t N>
  ShaderBytecode build_dxbc_container(const std::uint8_t* code, std::size_t len,
                                      const char* code_tag,
                                      const std::array<SignatureChunk, N>& sigs) {
    ShaderBytecode result = { };

    // Build every chunk into a scratch buffer first, recording where each one
    // starts relative to the chunk base.
    std::vector<std::uint8_t> chunks;
    std::array<std::uint32_t, N + 1> chunk_offsets = { };
    std::uint32_t chunk_count = 0;

    for (const auto& sig : sigs) {
      chunk_offsets.at(chunk_count++) = std::uint32_t(chunks.size());
      append_signature_chunk(chunks, sig.tag, sig.entries, sig.count);
    }

    chunk_offsets.at(chunk_count++) = std::uint32_t(chunks.size());
    chunks.insert(chunks.end(), code_tag, code_tag + 4);
    {
      std::uint32_t v = std::uint32_t(len);
      chunks.push_back(std::uint8_t(v));
      chunks.push_back(std::uint8_t(v >> 8));
      chunks.push_back(std::uint8_t(v >> 16));
      chunks.push_back(std::uint8_t(v >> 24));
    }
    chunks.insert(chunks.end(), code, code + len);

    const std::uint32_t file_header_size = 32u;
    const std::uint32_t offset_table_size = chunk_count * sizeof(std::uint32_t);
    const std::uint32_t chunk_base = file_header_size + offset_table_size;
    const std::uint32_t file_size = chunk_base + std::uint32_t(chunks.size());

    result.owned().resize(file_size);
    std::memcpy(&result.owned()[0], "DXBC", 4);
    write_le(result.owned(), 20u, std::uint32_t(1u));
    write_le(result.owned(), 24u, file_size);
    write_le(result.owned(), 28u, chunk_count);
    for (std::uint32_t i = 0; i < chunk_count; ++i)
      write_le(result.owned(), 32u + 4u * i, chunk_base + chunk_offsets.at(i));
    std::memcpy(&result.owned()[chunk_base], chunks.data(), chunks.size());

    auto digest = dxbc_spv::dxbc::hashDxbcBinary(result.owned().data(), result.owned().size());
    std::memcpy(&result.owned()[4], digest.data.data(), digest.data.size());

    // No `result.data = result.owned().data()` any more: the range is derived.
    return result;
  }

  // "SHEX" for SM5+, "SHDR" for SM4. Was re-derived at all three wrappers.
  const char* dxbc_code_tag(std::uint32_t version_token) {
    return ((version_token >> 4u) & 0xfu) >= 5u ? "SHEX" : "SHDR";
  }

  // Wrap a raw SM4/SM5 token stream in a DXBC container carrying REAL input/
  // output signature chunks from the >=11.1 DDI. Layout mirrors
  // prepare_shader_bytecode's code-only wrap, with two extra chunks.
  ShaderBytecode prepare_shader_bytecode_with_sigs(
      const std::uint8_t* code, std::size_t len,
      const std::uint32_t* in_entries, std::uint32_t n_in,
      const std::uint32_t* out_entries, std::uint32_t n_out) {
    ShaderBytecode result = { };
    if (!code || !len || len < 8 || (len & 3u))
      return result;

    const auto* dwords = reinterpret_cast<const std::uint32_t*>(code);
    if (std::size_t(dwords[1]) * sizeof(std::uint32_t) != len) {
      umd_log("raw shader bytecode dword count mismatch (sig wrap)");
      return result;
    }

    const std::array<SignatureChunk, 2> sigs = {{
      { "ISGN", in_entries,  n_in  },
      { "OSGN", out_entries, n_out },
    }};
    return build_dxbc_container(code, len, dxbc_code_tag(dwords[0]), sigs);
  }

  ShaderBytecode prepare_shader_bytecode_with_tess_sigs(
      const std::uint8_t* code, std::size_t len,
      const std::uint32_t* in_entries, std::uint32_t n_in,
      const std::uint32_t* out_entries, std::uint32_t n_out,
      const std::uint32_t* patch_entries, std::uint32_t n_patch) {
    ShaderBytecode result = { };
    if (!code || !len || len < 8 || (len & 3u))
      return result;

    const auto* dwords = reinterpret_cast<const std::uint32_t*>(code);
    if (std::size_t(dwords[1]) * sizeof(std::uint32_t) != len) {
      umd_log("raw shader bytecode dword count mismatch (tess sig wrap)");
      return result;
    }

    // The tess variant differs by exactly one list element.
    const std::array<SignatureChunk, 3> sigs = {{
      { "ISGN", in_entries,    n_in    },
      { "OSGN", out_entries,   n_out   },
      { "PCSG", patch_entries, n_patch },
    }};
    return build_dxbc_container(code, len, dxbc_code_tag(dwords[0]), sigs);
  }

  ShaderBytecode prepare_shader_bytecode(const std::uint8_t* code, std::size_t len) {
    ShaderBytecode result = { };

    if (!code || !len)
      return result;

    if (len >= 4 && std::memcmp(code, "DXBC", 4) == 0) {
      result.borrow(code, len);
      return result;
    }

    if (len < 8 || (len & 3u)) {
      umd_log("raw shader bytecode has invalid size");
      return result;
    }

    const auto* dwords = reinterpret_cast<const std::uint32_t*>(code);
    const std::uint32_t version_token = dwords[0];
    const std::uint32_t dword_count = dwords[1];

    // `dword_count < 2u` is absent from both signature wrappers. It is
    // REDUNDANT in all three -- `len >= 8` together with `dword_count * 4 ==
    // len` already implies `dword_count >= 2` -- so it stays here rather than
    // being propagated: adding it to the other two would be adding validation,
    // and removing it here would be removing a (harmless) check. Neither is
    // this tranche's business.
    if (dword_count < 2u || std::size_t(dword_count) * sizeof(std::uint32_t) != len) {
      umd_log("raw shader bytecode dword count mismatch");
      return result;
    }

    // No signature chunks: the code-only wrap is the same container with an
    // empty list, which is where the three copies converge.
    const std::array<SignatureChunk, 0> sigs = { };
    return build_dxbc_container(code, len, dxbc_code_tag(version_token), sigs);
  }

}  // namespace helios_bridge
