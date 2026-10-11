/* Minimal driver ABI fixture. No Ascend headers or hardware required. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

struct dcmi_chip_info_v2 {
    unsigned char chip_type[32], chip_name[32], chip_ver[32];
    unsigned int aicore_cnt;
    unsigned char npu_name[32];
};
struct dcmi_proc_mem_info {
    int proc_id;
    unsigned long proc_mem_usage;
};

static int marker(const char *name) {
    char path[4096];
    snprintf(path, sizeof(path), "%s/%s", getenv("TEST_ROOT"), name);
    return access(path, F_OK) == 0;
}
static void query(const char *operation, int id) {
    char path[4096];
    snprintf(path, sizeof(path), "%s/dcmi-queries", getenv("TEST_ROOT"));
    FILE *file = fopen(path, "a");
    if (file) {
        fprintf(file, "%s %d\n", operation, id);
        fclose(file);
    }
}
int dcmiv2_init(void) {
    query("init", -1);
    return marker("dcmi-init-error") ? -1 : 0;
}
int dcmiv2_get_device_list(int *ids, int *count, int length) {
    if (length < 2) return -1;
    *count = marker("dcmi-device-count-error") ? 65 : 2;
    ids[0] = 3;
    ids[1] = marker("dcmi-duplicate-id") ? 3 : marker("dcmi-invalid-id") ? -1 : 2;
    return 0;
}
int dcmiv2_get_device_chip_info(int id, struct dcmi_chip_info_v2 *info) {
    if (id < 2 || id > 3 || marker("dcmi-chip-info-error")) return -1;
    memset(info, 0, sizeof(*info));
    memcpy(info->chip_name, "Ascend950PR", 11);
    memcpy(info->npu_name, "9579", 5);
    return 0;
}
int dcmiv2_get_device_health(int id, unsigned int *health) {
    query("health", id);
    if (id < 2 || id > 3 || marker("npu-health-error")) return -1;
    *health = marker("dcmi-missing-device") ? ~0U :
              marker("npu-health") || (id == 2 && marker("npu-chip0-unhealthy")) ? 2 : 0;
    return 0;
}
#ifndef OMIT_PROCESS_API
int dcmiv2_get_device_resource_info(int id,
                                struct dcmi_proc_mem_info *processes, int *count) {
    query("process", id);
    if (id < 2 || id > 3 || marker("probe-error")) return -1;
    *count = marker("dcmi-invalid-process-count") ? 33 : marker("npu-busy") ? 1 : 0;
    if (*count == 1) {
        processes[0].proc_id = 123;
        processes[0].proc_mem_usage = 1024;
    }
    return 0;
}
#endif
