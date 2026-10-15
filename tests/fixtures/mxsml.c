/* Minimal MXSML ABI fixture; no vendor headers or hardware required. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

struct process_info {
    unsigned int compute_instance_id, gpu_instance_id, pid;
    unsigned long long used_gpu_memory;
};
struct unavailable_reason { int code; char reason[64]; };
static int marker(const char *name) {
    char path[4096];
    snprintf(path, sizeof(path), "%s/%s", getenv("TEST_ROOT"), name);
    return access(path, F_OK) == 0;
}
static void query(const char *operation, unsigned int id) {
    char path[4096];
    snprintf(path, sizeof(path), "%s/mxsml-queries", getenv("TEST_ROOT"));
    FILE *file = fopen(path, "a");
    if (file) { fprintf(file, "%s %u\n", operation, id); fclose(file); }
}
unsigned int mxSmlExInit(void) { return marker("mx-init-error") ? 999 : 0; }
unsigned int mxSmlExShutdown(void) { return 0; }
unsigned int mxSmlExDeviceGetCount(unsigned int *count) {
    *count = marker("mx-invalid-count") ? 65 : marker("mx-zero-count") ? 0 : 2;
    return marker("mx-count-error") ? 999 : 0;
}
unsigned int mxSmlExGetDeviceHandleByIndex(unsigned int id, void **handle) {
    if (id > 1 || (id == 1 && marker("mx-handle-error"))) return 2;
    *handle = id == 1 && marker("mx-null-handle") ? NULL : (void *)(uintptr_t)(id + 1);
    return 0;
}
unsigned int mxSmlExDeviceGetUUID(void *handle, char *uuid, unsigned int length) {
    unsigned int id = (uintptr_t)handle - 1;
    if (id > 1 || length < 80 || (id == 1 && marker("mx-uuid-error"))) return 2;
    snprintf(uuid, length, "GPU-metax-%u", marker("mx-duplicate-id") ? 0 : id);
    if (id == 1 && marker("mx-uuid-changed")) snprintf(uuid, length, "GPU-replaced");
    if (id == 1 && marker("mx-invalid-uuid")) snprintf(uuid, length, "invalid");
    return 0;
}
unsigned int mxSmlExDeviceGetName(void *handle, char *name, unsigned int length) {
    unsigned int id = (uintptr_t)handle - 1;
    if (id > 1 || !length || (id == 1 && marker("mx-name-error"))) return 2;
    snprintf(name, length, "MXC550");
    if (marker("mx-unterminated-name")) memset(name, 'x', length);
    if (marker("mx-invalid-utf8")) name[0] = (char)0xff;
    return 0;
}
unsigned int mxSmlGetDeviceUnavailableReason(unsigned int id, struct unavailable_reason *reason) {
    query("availability", id);
    if (id > 1 || (id == 1 && marker("mx-availability-error"))) return 999;
    if (id == 1 && marker("mx-missing-availability")) return 0;
    reason->code = id == 1 && marker("mx-unhealthy") ? 1 : 0;
    return 0;
}
#ifndef OMIT_PROCESS_API
unsigned int mxSmlExDeviceGetComputeRunningProcesses(void *handle, unsigned int *count, struct process_info *infos) {
    unsigned int id = (uintptr_t)handle - 1;
    query("process", id);
    if (id > 1 || (id == 1 && marker("mx-probe-error"))) return 999;
    if (*count < 1 || (id == 1 && marker("mx-insufficient-capacity"))) return 7;
    *count = id == 1 && marker("mx-invalid-process-count") ? 4097 : id == 1 && marker("mx-busy") ? 1 : 0;
    infos[0] = (struct process_info){.pid = marker("mx-invalid-pid") ? 0 : 1234};
    return 0;
}
#endif
