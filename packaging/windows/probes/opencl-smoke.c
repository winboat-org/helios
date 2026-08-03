#define CL_TARGET_OPENCL_VERSION 120
#include <CL/cl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void print_info(cl_platform_id platform, cl_device_id device) {
    char text[512] = {0};
    clGetPlatformInfo(platform, CL_PLATFORM_NAME, sizeof(text), text, NULL);
    printf("OpenCL platform: %s\n", text);
    memset(text, 0, sizeof(text));
    clGetDeviceInfo(device, CL_DEVICE_NAME, sizeof(text), text, NULL);
    printf("OpenCL device: %s\n", text);
}

int main(void) {
    cl_uint platform_count = 0;
    cl_int error = clGetPlatformIDs(0, NULL, &platform_count);
    if (error != CL_SUCCESS || platform_count == 0) {
        fprintf(stderr, "No OpenCL platform (error=%d).\n", error);
        return 1;
    }
    cl_platform_id *platforms = (cl_platform_id *)calloc(platform_count, sizeof(*platforms));
    if (!platforms) return 2;
    error = clGetPlatformIDs(platform_count, platforms, NULL);
    if (error != CL_SUCCESS) return 3;

    cl_platform_id platform = NULL;
    cl_device_id device = NULL;
    for (cl_uint index = 0; index < platform_count && !device; ++index) {
        if (clGetDeviceIDs(platforms[index], CL_DEVICE_TYPE_GPU, 1, &device, NULL) == CL_SUCCESS) {
            platform = platforms[index];
        }
    }
    free(platforms);
    if (!device) {
        fprintf(stderr, "No OpenCL GPU device was found.\n");
        return 4;
    }
    print_info(platform, device);

    cl_context context = clCreateContext(NULL, 1, &device, NULL, NULL, &error);
    if (!context || error != CL_SUCCESS) return 5;
    cl_command_queue queue = clCreateCommandQueue(context, device, 0, &error);
    if (!queue || error != CL_SUCCESS) return 6;
    const char *source = "__kernel void add1(__global int* x) { size_t i=get_global_id(0); x[i]+=1; }";
    cl_program program = clCreateProgramWithSource(context, 1, &source, NULL, &error);
    if (!program || error != CL_SUCCESS) return 7;
    error = clBuildProgram(program, 1, &device, "", NULL, NULL);
    if (error != CL_SUCCESS) {
        size_t size = 0;
        clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, 0, NULL, &size);
        char *log = (char *)calloc(size + 1, 1);
        if (log) {
            clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, size, log, NULL);
            fprintf(stderr, "OpenCL build failed (%d): %s\n", error, log);
            free(log);
        }
        return 8;
    }
    cl_kernel kernel = clCreateKernel(program, "add1", &error);
    if (!kernel || error != CL_SUCCESS) return 9;
    enum { COUNT = 256 };
    int values[COUNT];
    for (int index = 0; index < COUNT; ++index) values[index] = index;
    cl_mem buffer = clCreateBuffer(context, CL_MEM_READ_WRITE | CL_MEM_COPY_HOST_PTR, sizeof(values), values, &error);
    if (!buffer || error != CL_SUCCESS) return 10;
    error = clSetKernelArg(kernel, 0, sizeof(buffer), &buffer);
    size_t global = COUNT;
    if (error == CL_SUCCESS) error = clEnqueueNDRangeKernel(queue, kernel, 1, NULL, &global, NULL, 0, NULL, NULL);
    if (error == CL_SUCCESS) error = clEnqueueReadBuffer(queue, buffer, CL_TRUE, 0, sizeof(values), values, 0, NULL, NULL);
    int mismatches = 0;
    for (int index = 0; index < COUNT; ++index) if (values[index] != index + 1) ++mismatches;
    printf("OpenCL kernel result: %d mismatches across %d items.\n", mismatches, COUNT);
    clReleaseMemObject(buffer);
    clReleaseKernel(kernel);
    clReleaseProgram(program);
    clReleaseCommandQueue(queue);
    clReleaseContext(context);
    return (error == CL_SUCCESS && mismatches == 0) ? 0 : 11;
}
