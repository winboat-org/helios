#define WIN32_LEAN_AND_MEAN
#include <windows.h>

#include <stdio.h>
#include <string.h>

#include "resolve-opencl-properties.h"

#define TEST_CL_CONTEXT_PLATFORM ((helios_cl_context_property)0x1084)

static int failures;

static void expect(int condition, const char *message) {
  if (!condition) {
    fprintf(stderr, "FAIL: %s\n", message);
    ++failures;
  }
}

static void test_mixed_context_is_narrowed(void) {
  const helios_cl_context_property input[] = {
      TEST_CL_CONTEXT_PLATFORM,
      0x11,
      HELIOS_CL_GL_CONTEXT_KHR,
      0x22,
      HELIOS_CL_WGL_HDC_KHR,
      0x33,
      HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR,
      0x44,
      0};
  const helios_cl_context_property expected[] = {
      TEST_CL_CONTEXT_PLATFORM, 0x11, HELIOS_CL_GL_CONTEXT_KHR, 0x22,
      HELIOS_CL_WGL_HDC_KHR, 0x33, 0};
  helios_cl_context_property output[HELIOS_RESOLVE_FILTER_CAPACITY];
  const helios_cl_context_property *forwarded = NULL;
  enum helios_resolve_filter_result result = helios_prepare_resolve_properties(
      1, input, output, HELIOS_RESOLVE_FILTER_CAPACITY, &forwarded);

  expect(result == HELIOS_RESOLVE_PROPERTIES_FILTERED,
         "mixed GL/WGL/D3D11 list must be filtered");
  expect(forwarded == output, "filtered list must use caller storage");
  expect(memcmp(output, expected, sizeof(expected)) == 0,
         "only the D3D11 property must be removed");
}

static void test_other_shapes_are_unchanged(void) {
  const helios_cl_context_property d3d_only[] = {
      TEST_CL_CONTEXT_PLATFORM, 0x11, HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR,
      0x44, 0};
  const helios_cl_context_property gl_without_wgl[] = {
      HELIOS_CL_GL_CONTEXT_KHR, 0x22, HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR,
      0x44, 0};
  const helios_cl_context_property null_d3d[] = {
      HELIOS_CL_GL_CONTEXT_KHR, 0x22, HELIOS_CL_WGL_HDC_KHR, 0x33,
      HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR, 0, 0};
  const helios_cl_context_property duplicate_d3d[] = {
      HELIOS_CL_GL_CONTEXT_KHR,
      0x22,
      HELIOS_CL_WGL_HDC_KHR,
      0x33,
      HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR,
      0x44,
      HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR,
      0x55,
      0};
  const helios_cl_context_property duplicate_null_d3d[] = {
      HELIOS_CL_GL_CONTEXT_KHR,
      0x22,
      HELIOS_CL_WGL_HDC_KHR,
      0x33,
      HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR,
      0x44,
      HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR,
      0,
      0};
  helios_cl_context_property output[HELIOS_RESOLVE_FILTER_CAPACITY];
  const helios_cl_context_property *forwarded = NULL;

#define EXPECT_UNCHANGED(list, enabled, message)                               \
  do {                                                                         \
    enum helios_resolve_filter_result result =                                \
        helios_prepare_resolve_properties((enabled), (list), output,           \
                                          HELIOS_RESOLVE_FILTER_CAPACITY,      \
                                          &forwarded);                         \
    expect(result == HELIOS_RESOLVE_PROPERTIES_UNCHANGED, (message));          \
    expect(forwarded == (list), "unchanged list must retain its identity");   \
  } while (0)

  EXPECT_UNCHANGED(d3d_only, 1, "D3D11-only context must remain unchanged");
  EXPECT_UNCHANGED(gl_without_wgl, 1,
                   "GL/D3D11 list without WGL binding must remain unchanged");
  EXPECT_UNCHANGED(null_d3d, 1,
                   "explicit-null D3D11 property must remain unchanged");
  EXPECT_UNCHANGED(duplicate_d3d, 1,
                   "duplicate D3D11 properties must remain invalid upstream");
  EXPECT_UNCHANGED(duplicate_null_d3d, 1,
                   "nonnull plus null duplicate D3D11 must remain unchanged");
  EXPECT_UNCHANGED(d3d_only, 0, "disabled compatibility must be a no-op");
#undef EXPECT_UNCHANGED
}

static void test_order_and_maximum_length(void) {
  helios_cl_context_property input[HELIOS_RESOLVE_FILTER_CAPACITY];
  helios_cl_context_property output[HELIOS_RESOLVE_FILTER_CAPACITY];
  helios_cl_context_property unterminated[HELIOS_RESOLVE_FILTER_CAPACITY];
  const helios_cl_context_property *forwarded = NULL;
  enum helios_resolve_filter_result result;
  size_t pair;

  for (pair = 0; pair < HELIOS_RESOLVE_MAX_CONTEXT_PROPERTY_PAIRS; ++pair) {
    input[pair * 2U] = (helios_cl_context_property)(0x5000U + pair);
    input[pair * 2U + 1U] = (helios_cl_context_property)(0x6000U + pair);
  }
  input[7U * 2U] = HELIOS_CL_WGL_HDC_KHR;
  input[7U * 2U + 1U] = 0x33;
  input[31U * 2U] = HELIOS_CL_CONTEXT_D3D11_DEVICE_KHR;
  input[31U * 2U + 1U] = 0x44;
  input[63U * 2U] = HELIOS_CL_GL_CONTEXT_KHR;
  input[63U * 2U + 1U] = 0x22;
  input[HELIOS_RESOLVE_MAX_CONTEXT_PROPERTY_PAIRS * 2U] = 0;

  result = helios_prepare_resolve_properties(
      1, input, output, HELIOS_RESOLVE_FILTER_CAPACITY, &forwarded);
  expect(result == HELIOS_RESOLVE_PROPERTIES_FILTERED,
         "64-pair list with terminator must be accepted");
  expect(output[7U * 2U] == HELIOS_CL_WGL_HDC_KHR,
         "properties before D3D11 must retain order");
  expect(output[31U * 2U] == (helios_cl_context_property)(0x5020U),
         "properties after D3D11 must shift without reordering");
  expect(output[62U * 2U] == HELIOS_CL_GL_CONTEXT_KHR &&
             output[62U * 2U + 2U] == 0,
         "last property and terminator must remain intact");

  for (pair = 0; pair < HELIOS_RESOLVE_FILTER_CAPACITY; ++pair) {
    unterminated[pair] = 1;
  }
  result = helios_prepare_resolve_properties(
      1, unterminated, output, HELIOS_RESOLVE_FILTER_CAPACITY, &forwarded);
  expect(result == HELIOS_RESOLVE_PROPERTIES_UNTERMINATED,
         "unterminated maximum-length list must fall back unchanged");
  expect(forwarded == unterminated,
         "unterminated list must retain its identity");
}

static void test_single_scalar_terminator_does_not_overread(void) {
  SYSTEM_INFO info;
  unsigned char *allocation;
  DWORD old_protection;
  helios_cl_context_property *terminator;
  helios_cl_context_property output[HELIOS_RESOLVE_FILTER_CAPACITY];
  const helios_cl_context_property *forwarded = NULL;
  enum helios_resolve_filter_result result;

  GetSystemInfo(&info);
  allocation = (unsigned char *)VirtualAlloc(
      NULL, (size_t)info.dwPageSize * 2U, MEM_RESERVE | MEM_COMMIT,
      PAGE_READWRITE);
  expect(allocation != NULL, "guard-page allocation must succeed");
  if (allocation == NULL) {
    return;
  }
  expect(VirtualProtect(allocation + info.dwPageSize, info.dwPageSize,
                        PAGE_NOACCESS, &old_protection),
         "guard page must be protected");
  terminator = (helios_cl_context_property *)(allocation + info.dwPageSize -
                                               sizeof(*terminator));
  *terminator = 0;

  result = helios_prepare_resolve_properties(
      1, terminator, output, HELIOS_RESOLVE_FILTER_CAPACITY, &forwarded);
  expect(result == HELIOS_RESOLVE_PROPERTIES_UNCHANGED,
         "single-scalar terminator must be accepted");
  expect(forwarded == terminator,
         "terminator-only list must retain its identity");
  VirtualFree(allocation, 0, MEM_RELEASE);
}

int main(void) {
  test_mixed_context_is_narrowed();
  test_other_shapes_are_unchanged();
  test_order_and_maximum_length();
  test_single_scalar_terminator_does_not_overread();
  if (failures != 0) {
    fprintf(stderr, "%d Resolve OpenCL property-filter test(s) failed.\n",
            failures);
    return 1;
  }
  puts("Resolve OpenCL property-filter tests passed.");
  return 0;
}
