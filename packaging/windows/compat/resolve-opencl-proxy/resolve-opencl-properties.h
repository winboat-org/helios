#pragma once

#include <stddef.h>
#include <stdint.h>

/* Keep this helper independent of an OpenCL SDK so the compatibility job can
 * build from a shallow Helios checkout. These values and the property scalar
 * ABI are fixed by the Khronos headers on 64-bit Windows. */
typedef intptr_t helios_cl_context_property;

#define HELIOS_CL_GL_CONTEXT_KHR ((helios_cl_context_property)0x2008)
#define HELIOS_CL_WGL_HDC_KHR ((helios_cl_context_property)0x200B)
#define HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR                                    \
  ((helios_cl_context_property)0x401D)
#define HELIOS_RESOLVE_MAX_CONTEXT_PROPERTY_PAIRS 64U
#define HELIOS_RESOLVE_FILTER_CAPACITY                                        \
  (HELIOS_RESOLVE_MAX_CONTEXT_PROPERTY_PAIRS * 2U + 1U)

enum helios_resolve_filter_result {
  HELIOS_RESOLVE_PROPERTIES_UNCHANGED = 0,
  HELIOS_RESOLVE_PROPERTIES_FILTERED = 1,
  HELIOS_RESOLVE_PROPERTIES_UNTERMINATED = 2
};

/* Resolve 21.0.4 asks for a context containing GL, WGL, and D3D11 sharing
 * properties together. OpenCL requires that combination to fail. The app-local
 * compatibility boundary removes only the non-null D3D11 property from that
 * exact mixed shape; D3D11-only and GL-only contexts remain untouched.
 *
 * Read the value only after checking the single-scalar terminator. Besides
 * matching the property-list ABI, this is important when the terminator is the
 * last readable scalar on a page. */
static enum helios_resolve_filter_result helios_prepare_resolve_properties(
    int enabled, const helios_cl_context_property *properties,
    helios_cl_context_property *filtered, size_t filtered_capacity,
    const helios_cl_context_property **forwarded) {
  size_t pair;
  size_t pair_count = 0;
  unsigned int gl_context_count = 0;
  unsigned int wgl_hdc_count = 0;
  unsigned int d3d11_device_count = 0;
  int has_nonnull_gl_context = 0;
  int has_nonnull_wgl_hdc = 0;
  int has_nonnull_d3d11_device = 0;

  if (forwarded == NULL) {
    return HELIOS_RESOLVE_PROPERTIES_UNTERMINATED;
  }
  *forwarded = properties;
  if (!enabled || properties == NULL) {
    return HELIOS_RESOLVE_PROPERTIES_UNCHANGED;
  }

  for (pair = 0; pair <= HELIOS_RESOLVE_MAX_CONTEXT_PROPERTY_PAIRS; ++pair) {
    helios_cl_context_property key = properties[pair * 2U];
    helios_cl_context_property value;
    if (key == 0) {
      pair_count = pair;
      break;
    }
    if (pair == HELIOS_RESOLVE_MAX_CONTEXT_PROPERTY_PAIRS) {
      return HELIOS_RESOLVE_PROPERTIES_UNTERMINATED;
    }
    value = properties[pair * 2U + 1U];
    if (key == HELIOS_CL_GL_CONTEXT_KHR) {
      ++gl_context_count;
      has_nonnull_gl_context = value != 0;
    } else if (key == HELIOS_CL_WGL_HDC_KHR) {
      ++wgl_hdc_count;
      has_nonnull_wgl_hdc = value != 0;
    } else if (key == HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR) {
      ++d3d11_device_count;
      has_nonnull_d3d11_device = value != 0;
    }
  }

  if (gl_context_count != 1U || wgl_hdc_count != 1U ||
      d3d11_device_count != 1U || !has_nonnull_gl_context ||
      !has_nonnull_wgl_hdc || !has_nonnull_d3d11_device) {
    return HELIOS_RESOLVE_PROPERTIES_UNCHANGED;
  }
  if (filtered == NULL || filtered_capacity < pair_count * 2U + 1U) {
    return HELIOS_RESOLVE_PROPERTIES_UNTERMINATED;
  }

  {
    size_t destination = 0;
    for (pair = 0; pair < pair_count; ++pair) {
      helios_cl_context_property key = properties[pair * 2U];
      helios_cl_context_property value = properties[pair * 2U + 1U];
      if (key == HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR) {
        continue;
      }
      filtered[destination++] = key;
      filtered[destination++] = value;
    }
    filtered[destination] = 0;
  }
  *forwarded = filtered;
  return HELIOS_RESOLVE_PROPERTIES_FILTERED;
}
