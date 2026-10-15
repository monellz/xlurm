/* Minimal HCU SMI C ABI fixture, independent of vendor headers/hardware. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

struct process_info {
    uint32_t pid, pasid;
    uint64_t vram, sdma;
    uint32_t cu;
};
static int marker(const char *name) {
    char path[4096];
    snprintf(path, sizeof(path), "%s/%s", getenv("TEST_ROOT"), name);
    return access(path, F_OK) == 0;
}
int rsmi_init(uint64_t flags) {
    return flags || marker("hcu-init-error") ? 1 : 0;
}
int rsmi_shut_down(void) { return 0; }
int rsmi_num_monitor_devices(uint32_t *count) {
    *count = marker("hcu-count-error") ? 65 : 2;
    return 0;
}
int rsmi_dev_unique_id_get(uint32_t id, uint64_t *uid) {
    *uid = marker("hcu-duplicate-id") ? 1 : id + 1;
    return marker("hcu-discovery-error") ? 1 : 0;
}
#ifndef OMIT_PROCESS_API
int rsmi_compute_process_info_get(struct process_info *procs, uint32_t *count) {
    if (marker("hcu-probe-error")) return 1;
    if (*count < 1) return 11;
    *count = marker("hcu-invalid-process-count") ? 4097 : marker("hcu-busy") ? 1 : 0;
    procs[0] = (struct process_info){.pid = 1234};
    return 0;
}
#endif
int rsmi_compute_process_gpus_get(uint32_t pid, uint32_t *ids, uint32_t *count) {
    if (pid != 1234 || marker("hcu-process-gpus-error")) return 1;
    if (*count < 1) return 11;
    *count = marker("hcu-invalid-gpu-count") ? 65 : 1;
    ids[0] = marker("hcu-invalid-gpu-id") ? 2 : 1;
    return 0;
}
