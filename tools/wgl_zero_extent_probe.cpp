// Regression probe for WGL/Zink zero-area Win32 window transitions.
//
// Minecraft can create or retain its OpenGL context while the client area is
// transiently 120x0. WGL must use ordinary offscreen resources while there is
// no drawable area, then replace them with swapchain-backed resources when the
// window becomes drawable.

#define WIN32_LEAN_AND_MEAN
#include <GL/gl.h>
#include <windows.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static LRESULT CALLBACK window_proc(HWND hwnd, UINT message, WPARAM wparam,
                                    LPARAM lparam) {
  return DefWindowProcA(hwnd, message, wparam, lparam);
}

static void fail(const char *operation) {
  fprintf(stderr, "FAIL %s (Win32=%lu)\n", operation, GetLastError());
  ExitProcess(1);
}

static void pump_messages(void) {
  MSG message;
  while (PeekMessageA(&message, NULL, 0, 0, PM_REMOVE)) {
    TranslateMessage(&message);
    DispatchMessageA(&message);
  }
}

static void set_client_size(HWND hwnd, LONG width, LONG height) {
  if (!SetWindowPos(hwnd, NULL, 0, 0, width, height,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE))
    fail("SetWindowPos");
  pump_messages();

  RECT rect;
  if (!GetClientRect(hwnd, &rect) || rect.right - rect.left != width ||
      rect.bottom - rect.top != height)
    fail("set exact client size");
}

static void render_readback_and_swap(HDC dc, const char *phase, float red,
                                     float green, float blue,
                                     unsigned char expected_red,
                                     unsigned char expected_green,
                                     unsigned char expected_blue) {
  glClearColor(red, green, blue, 1.0f);
  glClear(GL_COLOR_BUFFER_BIT);

  unsigned char pixel[4] = {};
  glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
  const GLenum error = glGetError();
  if (error != GL_NO_ERROR) {
    fprintf(stderr, "FAIL %s render/readback (GL=0x%x)\n", phase,
            (unsigned)error);
    ExitProcess(1);
  }

  if (abs((int)pixel[0] - (int)expected_red) > 2 ||
      abs((int)pixel[1] - (int)expected_green) > 2 ||
      abs((int)pixel[2] - (int)expected_blue) > 2 || pixel[3] < 253) {
    fprintf(stderr, "FAIL %s color (RGBA=%u,%u,%u,%u)\n", phase, pixel[0],
            pixel[1], pixel[2], pixel[3]);
    ExitProcess(1);
  }

  if (!SwapBuffers(dc))
    fail(phase);
  printf("%s succeeded (RGBA=%u,%u,%u,%u)\n", phase, pixel[0], pixel[1],
         pixel[2], pixel[3]);
}

int main(int argc, char **argv) {
  if (argc > 2) {
    fprintf(stderr, "usage: %s [expected WGL ICD path]\n", argv[0]);
    return 2;
  }

  HINSTANCE hinstance = GetModuleHandleA(NULL);
  WNDCLASSA window_class = {};
  window_class.lpfnWndProc = window_proc;
  window_class.hInstance = hinstance;
  window_class.lpszClassName = "HeliosWglZeroExtentProbe";
  if (!RegisterClassA(&window_class) &&
      GetLastError() != ERROR_CLASS_ALREADY_EXISTS)
    fail("RegisterClassA");

  HWND hwnd = CreateWindowExA(0, window_class.lpszClassName,
                              "helios WGL zero-extent probe", WS_POPUP, 0, 0,
                              120, 0, NULL, NULL, hinstance, NULL);
  if (!hwnd)
    fail("CreateWindowExA");

  RECT rect;
  if (!GetClientRect(hwnd, &rect))
    fail("GetClientRect");
  printf("zero-area client: %ldx%ld\n", rect.right - rect.left,
         rect.bottom - rect.top);
  if (rect.right - rect.left <= 0 || rect.bottom - rect.top != 0)
    fail("create 120x0 client area");

  HDC dc = GetDC(hwnd);
  if (!dc)
    fail("GetDC");

  PIXELFORMATDESCRIPTOR descriptor = {};
  descriptor.nSize = sizeof(descriptor);
  descriptor.nVersion = 1;
  descriptor.dwFlags =
      PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
  descriptor.iPixelType = PFD_TYPE_RGBA;
  descriptor.cColorBits = 32;
  descriptor.cDepthBits = 24;
  descriptor.cStencilBits = 8;
  descriptor.iLayerType = PFD_MAIN_PLANE;
  const int format = ChoosePixelFormat(dc, &descriptor);
  if (!format || !SetPixelFormat(dc, format, &descriptor))
    fail("set pixel format");

  HGLRC context = wglCreateContext(dc);
  if (!context || !wglMakeCurrent(dc, context))
    fail("create/make-current WGL context");

  HMODULE opengl = GetModuleHandleA("opengl32.dll");
  HMODULE wgl = GetModuleHandleA("libgallium_wgl.dll");
  char executable_path[MAX_PATH] = {};
  char opengl_path[MAX_PATH] = {};
  char wgl_path[MAX_PATH] = {};
  if (!opengl || !wgl ||
      !GetModuleFileNameA(NULL, executable_path, sizeof(executable_path)) ||
      !GetModuleFileNameA(opengl, opengl_path, sizeof(opengl_path)) ||
      !GetModuleFileNameA(wgl, wgl_path, sizeof(wgl_path)))
    fail("resolve graphics module paths");

  char *executable_name = strrchr(executable_path, '\\');
  char *opengl_name = strrchr(opengl_path, '\\');
  if (!executable_name || !opengl_name)
    fail("parse module paths");
  *executable_name = '\0';
  *opengl_name = '\0';
  if (argc == 1 && _stricmp(executable_path, opengl_path) != 0)
    fail("load app-local OpenGL DLL");
  if (argc == 2 && _stricmp(argv[1], wgl_path) != 0) {
    fprintf(stderr, "FAIL loaded WGL ICD %s, expected %s\n", wgl_path, argv[1]);
    return 1;
  }

  const char *renderer = (const char *)glGetString(GL_RENDERER);
  if (!renderer || !strstr(renderer, "zink"))
    fail("select Zink renderer");
  printf("OpenGL DLL directory: %s\n", opengl_path);
  printf("WGL ICD: %s\n", wgl_path);
  printf("renderer: %s\n", renderer);

  /* Validate the initial context-creation edge that triggered Minecraft. */
  glViewport(0, 0, 1, 1);
  render_readback_and_swap(dc, "initial 120x0 offscreen render", 0.25f, 0.5f,
                           0.75f, 64, 128, 191);

  set_client_size(hwnd, 320, 240);
  ShowWindow(hwnd, SW_SHOW);
  pump_messages();
  glViewport(0, 0, 320, 240);
  render_readback_and_swap(dc, "initial 320x240 drawable render", 0.1f, 0.2f,
                           0.3f, 26, 51, 77);

  /* Exercise drawable -> offscreen -> same-size drawable transitions. The
   * preserved framebuffer dimensions remain 320x240 throughout, so passing
   * requires backing resources to be recreated for state, not just size.
   */
  for (unsigned cycle = 0; cycle < 2; cycle++) {
    set_client_size(hwnd, 120, 0);
    glViewport(0, 0, 1, 1);
    render_readback_and_swap(dc, "transition 120x0 offscreen render", 0.4f,
                             0.3f, 0.2f, 102, 77, 51);

    set_client_size(hwnd, 320, 240);
    glViewport(0, 0, 320, 240);
    render_readback_and_swap(dc, "same-size 320x240 drawable restore", 0.2f,
                             0.4f, 0.6f, 51, 102, 153);
  }

  glFinish();
  if (glGetError() != GL_NO_ERROR)
    fail("finish restored rendering");

  wglMakeCurrent(NULL, NULL);
  wglDeleteContext(context);
  ReleaseDC(hwnd, dc);
  DestroyWindow(hwnd);
  printf("PASS\n");
  return 0;
}
