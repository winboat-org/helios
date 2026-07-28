// DXBC container synthesis: the surface `dxvk_bridge.cpp`'s shader creates call.
//
// T8/R1105. `ShaderBytecode` is a complete type here rather than a forward
// declaration because the three factories return it by value, so every caller
// needs its size and its move operations.
//
// ⚠ Like `bridge_common.h`, free of DXVK, COM, Vulkan and WDK includes.
#pragma once

#include <cstddef>
#include <cstdint>
#include <vector>

namespace helios_bridge {

// A shader blob that is either BORROWED from the caller's buffer or OWNED in
// a vector this object built.
//
// Pre-R819 `data` meant two different things depending on which factory
// produced the value -- a pointer into the caller's buffer with `owned`
// empty, or a pointer aliasing `owned.data()`. The struct was COPYABLE, and
// copying it in the second case left the copy's `data` pointing at the
// ORIGINAL vector's heap buffer: a dangling pointer as soon as the original
// died, handed to CreateVertexShader as the shader blob. It survived only
// because every use is a single local initialised by `auto x = factory(...)`
// and never copied or reassigned.
//
// Two changes make the invalid state unrepresentable rather than merely
// unreached: the type is move-only, and the range is DERIVED from the owner
// instead of stored beside it, so there is no `data` member that can alias a
// vector which is not this object's. Moving is safe and stays allowed --
// moving a std::vector transfers the heap buffer, so a derived pointer stays
// valid; it is copying that reallocates.
//
// No std::span: this TU is C++17 (see umd/build.rs).
class ShaderBytecode {
 public:
  ShaderBytecode() = default;
  ShaderBytecode(const ShaderBytecode&) = delete;
  ShaderBytecode& operator=(const ShaderBytecode&) = delete;
  // A user-declared copy constructor -- even a deleted one -- suppresses the
  // implicit move operations, so they are defaulted explicitly.
  ShaderBytecode(ShaderBytecode&&) = default;
  ShaderBytecode& operator=(ShaderBytecode&&) = default;

  /// Borrow the caller's buffer. Its lifetime must exceed this object's --
  /// the one precondition that stays a documented rule.
  void borrow(const std::uint8_t* p, std::size_t n) {
    borrowed_ = p;
    borrowed_len_ = n;
  }

  /// The owned buffer, built in place by the wrapping factories.
  std::vector<std::uint8_t>& owned() { return owned_; }

  const std::uint8_t* data() const {
    return owned_.empty() ? borrowed_ : owned_.data();
  }
  std::size_t len() const {
    return owned_.empty() ? borrowed_len_ : owned_.size();
  }

  explicit operator bool() const {
    return data() && len();
  }

 private:
  std::vector<std::uint8_t> owned_;
  const std::uint8_t* borrowed_ = nullptr;
  std::size_t borrowed_len_ = 0;
};

/// Wrap a raw SM4/SM5 token stream in a bare DXBC container (code chunk only).
ShaderBytecode prepare_shader_bytecode(const std::uint8_t* code, std::size_t len);

/// ... plus REAL ISGN/OSGN signature chunks from the >=11.1 DDI.
ShaderBytecode prepare_shader_bytecode_with_sigs(
    const std::uint8_t* code, std::size_t len,
    const std::uint32_t* in_entries, std::uint32_t n_in,
    const std::uint32_t* out_entries, std::uint32_t n_out);

/// ... plus a PCSG patch-constant chunk, for hull and domain shaders.
ShaderBytecode prepare_shader_bytecode_with_tess_sigs(
    const std::uint8_t* code, std::size_t len,
    const std::uint32_t* in_entries, std::uint32_t n_in,
    const std::uint32_t* out_entries, std::uint32_t n_out,
    const std::uint32_t* patch_entries, std::uint32_t n_patch);

/// Registry-gated (`HKLM\SOFTWARE\Helios!ShaderBytecodeDumpPath`) dump of one
/// blob. No-op when the value is absent, which is the production case.
void dump_shader_bytecode(const char* stage,
                          const char* form,
                          const void* data,
                          std::size_t len);

}  // namespace helios_bridge
