/* End-to-end WGL -> Mesa/Zink -> CLVK sharing probe for Helios. */

#define WIN32_LEAN_AND_MEAN
#define CL_TARGET_OPENCL_VERSION 300
#define CL_USE_DEPRECATED_OPENCL_1_2_APIS
#include <windows.h>

#include <CL/cl.h>
#include <CL/cl_d3d11.h>
#include <CL/cl_gl.h>
#include <d3d11.h>
#include <GL/gl.h>

#include <array>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <vector>

namespace {

LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM wparam,
                             LPARAM lparam) {
    return DefWindowProcW(window, message, wparam, lparam);
}

template <typename T>
bool load_proc(HMODULE module, const char* name, T* out) {
    *out = reinterpret_cast<T>(GetProcAddress(module, name));
    if (*out == nullptr) {
        std::fprintf(stderr, "missing OpenCL function %s\n", name);
        return false;
    }
    return true;
}

} // namespace

int main(int argc, char** argv) {
    HINSTANCE instance = GetModuleHandleW(nullptr);
    WNDCLASSW window_class{};
    window_class.style = CS_OWNDC;
    window_class.lpfnWndProc = window_proc;
    window_class.hInstance = instance;
    window_class.lpszClassName = L"HeliosOpenClGlSharingSmoke";
    if (RegisterClassW(&window_class) == 0 &&
        GetLastError() != ERROR_CLASS_ALREADY_EXISTS) {
        std::fprintf(stderr, "RegisterClassW failed: %lu\n", GetLastError());
        return 2;
    }

    HWND window = CreateWindowW(window_class.lpszClassName, L"probe",
                                WS_OVERLAPPEDWINDOW, 0, 0, 64, 64, nullptr,
                                nullptr, instance, nullptr);
    HDC dc = window ? GetDC(window) : nullptr;
    PIXELFORMATDESCRIPTOR pfd{};
    pfd.nSize = sizeof(pfd);
    pfd.nVersion = 1;
    pfd.dwFlags = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
    pfd.iPixelType = PFD_TYPE_RGBA;
    pfd.cColorBits = 32;
    pfd.cAlphaBits = 8;
    pfd.iLayerType = PFD_MAIN_PLANE;
    int pixel_format = dc ? ChoosePixelFormat(dc, &pfd) : 0;
    HGLRC gl = nullptr;
    if (!window || !dc || pixel_format == 0 ||
        !SetPixelFormat(dc, pixel_format, &pfd) ||
        (gl = wglCreateContext(dc)) == nullptr || !wglMakeCurrent(dc, gl)) {
        std::fprintf(stderr, "WGL setup failed: %lu\n", GetLastError());
        return 3;
    }

    std::printf("GL_RENDERER=%s\n", glGetString(GL_RENDERER));
    bool resolve_case = false;
    bool d3d11_context_case = false;
    for (int argument = 1; argument < argc; ++argument) {
        resolve_case |= std::strcmp(argv[argument], "rgba16f") == 0;
        d3d11_context_case |=
            std::strcmp(argv[argument], "d3d11-context") == 0;
    }
    const size_t width = resolve_case ? 1920 : 4;
    const size_t height = resolve_case ? 1080 : 4;
    const size_t channel_size = resolve_case ? 2 : 1;
    std::vector<unsigned char> pixels(width * height * 4 * channel_size);
    if (resolve_case) {
        constexpr std::array<uint16_t, 8> half_values = {
            0x0000, 0x3c00, 0xbc00, 0x3800,
            0x4000, 0x3400, 0x4200, 0x4400,
        };
        for (size_t i = 0; i < pixels.size() / sizeof(uint16_t); i++) {
            const uint16_t value = half_values[i % half_values.size()];
            std::memcpy(pixels.data() + i * sizeof(value), &value,
                        sizeof(value));
        }
    } else {
        for (size_t i = 0; i < pixels.size(); i++) {
            pixels[i] = static_cast<unsigned char>((i * 37 + 11) & 0xff);
        }
    }
    GLuint texture = 0;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0,
                 resolve_case ? 0x881a /* GL_RGBA16F */
                              : 0x8058 /* GL_RGBA8 */,
                 static_cast<GLsizei>(width), static_cast<GLsizei>(height), 0,
                 GL_RGBA,
                 resolve_case ? 0x140b /* GL_HALF_FLOAT */ : GL_UNSIGNED_BYTE,
                 pixels.data());
    glFinish();
    if (glGetError() != GL_NO_ERROR) {
        std::fprintf(stderr, "GL texture setup failed\n");
        return 4;
    }
    std::printf("GL_TEXTURE case=%s extent=%zux%zu bytes=%zu\n",
                resolve_case ? "rgba16f-1920x1080" : "rgba8-4x4", width,
                height, pixels.size());

    HMODULE opencl = LoadLibraryW(L"OpenCL.dll");
    if (!opencl) {
        std::fprintf(stderr, "LoadLibrary(OpenCL.dll) failed: %lu\n",
                     GetLastError());
        return 5;
    }

    decltype(&clGetPlatformIDs) get_platform_ids{};
    decltype(&clGetDeviceIDs) get_device_ids{};
    decltype(&clGetDeviceInfo) get_device_info{};
    decltype(&clCreateContext) create_context{};
    decltype(&clCreateCommandQueue) create_queue{};
    decltype(&clCreateFromGLTexture) create_from_gl_texture{};
    decltype(&clEnqueueAcquireGLObjects) acquire_gl{};
    decltype(&clEnqueueReleaseGLObjects) release_gl{};
    decltype(&clEnqueueReadImage) read_image{};
    decltype(&clFinish) finish{};
    decltype(&clReleaseMemObject) release_mem{};
    decltype(&clReleaseCommandQueue) release_queue{};
    decltype(&clReleaseContext) release_context{};
    if (!load_proc(opencl, "clGetPlatformIDs", &get_platform_ids) ||
        !load_proc(opencl, "clGetDeviceIDs", &get_device_ids) ||
        !load_proc(opencl, "clGetDeviceInfo", &get_device_info) ||
        !load_proc(opencl, "clCreateContext", &create_context) ||
        !load_proc(opencl, "clCreateCommandQueue", &create_queue) ||
        !load_proc(opencl, "clCreateFromGLTexture", &create_from_gl_texture) ||
        !load_proc(opencl, "clEnqueueAcquireGLObjects", &acquire_gl) ||
        !load_proc(opencl, "clEnqueueReleaseGLObjects", &release_gl) ||
        !load_proc(opencl, "clEnqueueReadImage", &read_image) ||
        !load_proc(opencl, "clFinish", &finish) ||
        !load_proc(opencl, "clReleaseMemObject", &release_mem) ||
        !load_proc(opencl, "clReleaseCommandQueue", &release_queue) ||
        !load_proc(opencl, "clReleaseContext", &release_context)) {
        return 6;
    }

    cl_uint platform_count = 0;
    cl_int error = get_platform_ids(0, nullptr, &platform_count);
    std::vector<cl_platform_id> platforms(platform_count);
    if (error != CL_SUCCESS || platform_count == 0 ||
        get_platform_ids(platform_count, platforms.data(), nullptr) !=
            CL_SUCCESS) {
        std::fprintf(stderr, "OpenCL platform enumeration failed: %d\n",
                     error);
        return 7;
    }
    cl_platform_id platform = platforms[0];
    cl_device_id device = nullptr;
    error = get_device_ids(platform, CL_DEVICE_TYPE_GPU, 1, &device, nullptr);
    if (error != CL_SUCCESS) {
        std::fprintf(stderr, "OpenCL GPU enumeration failed: %d\n", error);
        return 8;
    }
    std::array<char, 512> device_name{};
    get_device_info(device, CL_DEVICE_NAME, device_name.size(),
                    device_name.data(), nullptr);
    std::printf("CL_DEVICE=%s\n", device_name.data());

    ID3D11Device* d3d11_device = nullptr;
    ID3D11DeviceContext* d3d11_device_context = nullptr;
    if (d3d11_context_case) {
        D3D_FEATURE_LEVEL feature_level{};
        const HRESULT result = D3D11CreateDevice(
            nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, nullptr, 0,
            D3D11_SDK_VERSION, &d3d11_device, &feature_level,
            &d3d11_device_context);
        if (FAILED(result)) {
            std::fprintf(stderr, "D3D11CreateDevice failed: 0x%08lx\n",
                         static_cast<unsigned long>(result));
            return 9;
        }
        std::printf("D3D11_CONTEXT feature_level=0x%x\n",
                    static_cast<unsigned int>(feature_level));
    }

    auto cleanup_graphics = [&] {
        if (d3d11_device_context != nullptr) {
            d3d11_device_context->Release();
        }
        if (d3d11_device != nullptr) {
            d3d11_device->Release();
        }
        glDeleteTextures(1, &texture);
        wglMakeCurrent(nullptr, nullptr);
        wglDeleteContext(gl);
        ReleaseDC(window, dc);
        DestroyWindow(window);
        UnregisterClassW(window_class.lpszClassName, instance);
        FreeLibrary(opencl);
    };

    std::vector<cl_context_properties> properties = {
        CL_CONTEXT_PLATFORM,
        reinterpret_cast<cl_context_properties>(platform),
        CL_GL_CONTEXT_KHR,
        reinterpret_cast<cl_context_properties>(gl),
        CL_WGL_HDC_KHR,
        reinterpret_cast<cl_context_properties>(dc),
    };
    if (d3d11_context_case) {
        properties.push_back(CL_CONTEXT_D3D11_DEVICE_KHR);
        properties.push_back(
            reinterpret_cast<cl_context_properties>(d3d11_device));
    }
    properties.push_back(0);
    cl_context context = create_context(properties.data(), 1, &device, nullptr,
                                        nullptr, &error);
    std::printf("clCreateContext result=%p error=%d\n", context, error);
    if (d3d11_context_case) {
        std::printf("mixed D3D11/OpenGL context accepted=%d\n",
                    context != nullptr && error == CL_SUCCESS);
    }
    if (!context || error != CL_SUCCESS) {
        cleanup_graphics();
        return 10;
    }
    cl_command_queue queue = create_queue(context, device, 0, &error);
    if (!queue || error != CL_SUCCESS) {
        std::fprintf(stderr, "clCreateCommandQueue failed: %d\n", error);
        return 11;
    }

    cl_mem image = create_from_gl_texture(context, CL_MEM_READ_WRITE,
                                          GL_TEXTURE_2D, 0, texture, &error);
    std::printf("clCreateFromGLTexture result=%p error=%d\n", image, error);
    if (!image || error != CL_SUCCESS) {
        return 12;
    }

    error = acquire_gl(queue, 1, &image, 0, nullptr, nullptr);
    std::printf("clEnqueueAcquireGLObjects error=%d\n", error);
    if (error != CL_SUCCESS) {
        return 13;
    }
    const size_t origin[3] = {0, 0, 0};
    const size_t region[3] = {width, height, 1};
    std::vector<unsigned char> readback(pixels.size());
    error = read_image(queue, image, CL_TRUE, origin, region, 0, 0,
                       readback.data(), 0, nullptr, nullptr);
    std::printf("clEnqueueReadImage error=%d match=%d\n", error,
                readback == pixels);
    if (error != CL_SUCCESS || readback != pixels) {
        return 14;
    }
    error = release_gl(queue, 1, &image, 0, nullptr, nullptr);
    if (error == CL_SUCCESS) {
        error = finish(queue);
    }
    std::printf("clEnqueueReleaseGLObjects/clFinish error=%d\n", error);

    release_mem(image);
    release_queue(queue);
    release_context(context);
    cleanup_graphics();
    return error == CL_SUCCESS ? 0 : 15;
}
