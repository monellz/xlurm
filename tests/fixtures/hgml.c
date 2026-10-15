/* Minimal HGML ABI fixture; no vendor headers or hardware required. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

struct process_info {
    unsigned int pid;
    unsigned long long used_gpu_memory;
    unsigned int gpu_instance_id, compute_instance_id;
};
static int marker(const char *name) {
    char path[4096];
    snprintf(path, sizeof(path), "%s/%s", getenv("TEST_ROOT"), name);
    return access(path, F_OK) == 0;
}
unsigned int hgmlInit_v2(void) { return marker("ppu-init-error") ? 999 : 0; }
unsigned int hgmlShutdown(void) { return 0; }
unsigned int hgmlDeviceGetCount_v2(unsigned int *count) {
    *count = marker("ppu-invalid-count") ? 65 : marker("ppu-zero-count") ? 0 : 2;
    return marker("ppu-count-error") ? 999 : 0;
}
unsigned int hgmlDeviceGetHandleByIndex_v2(unsigned int id, void **handle) {
    if (id > 1 || (id == 1 && marker("ppu-handle-error"))) return 2;
    *handle = id == 1 && marker("ppu-null-handle") ? NULL : (void *)(uintptr_t)(id + 1);
    return 0;
}
unsigned int hgmlDeviceGetUUID(void *handle, char *uuid, unsigned int length) {
    unsigned int id = (uintptr_t)handle - 1;
    if (id > 1 || length < 80 || (id == 1 && marker("ppu-uuid-error"))) return 2;
    snprintf(uuid, length, "GPU-ppu-%u", marker("ppu-duplicate-id") ? 0 : id);
    if (id == 1 && marker("ppu-uuid-changed")) snprintf(uuid, length, "GPU-replaced");
    if (id == 1 && marker("ppu-prefix")) snprintf(uuid, length, "PPU-ppu-%u", id);
    if (id == 1 && marker("ppu-invalid-uuid")) snprintf(uuid, length, "invalid");
    return 0;
}
unsigned int hgmlDeviceGetName(void *handle, char *name, unsigned int length) {
    unsigned int id = (uintptr_t)handle - 1;
    if (id > 1 || !length || (id == 1 && marker("ppu-name-error"))) return 2;
    snprintf(name, length, "PPU-ZW810E");
    if (marker("ppu-unterminated-name")) memset(name, 'x', length);
    if (marker("ppu-invalid-utf8")) name[0] = (char)0xff;
    return 0;
}
#ifndef OMIT_PROCESS_API
unsigned int hgmlDeviceGetComputeRunningProcesses_v2(void *handle, unsigned int *count, struct process_info *infos) {
    unsigned int id = (uintptr_t)handle - 1;
    if (id > 1 || (id == 1 && marker("ppu-probe-error"))) return 999;
    if (*count < 1 || (id == 1 && marker("ppu-insufficient-capacity"))) return 7;
    *count = id == 1 && marker("ppu-invalid-process-count") ? 4097 : id == 1 && marker("ppu-busy") ? 1 : 0;
    infos[0] = (struct process_info){.pid = marker("ppu-invalid-pid") ? 0 : 1234};
    return 0;
}
#endif
