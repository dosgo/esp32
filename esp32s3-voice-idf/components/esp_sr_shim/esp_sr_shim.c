// Minimal C shim around the official esp-sr component.
//
// Rust std code calls these functions through extern "C"; the official
// component owns FreeRTOS, model partition, SPIFFS and the WakeNet C API.

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "esp_wn_iface.h"
#include "esp_wn_models.h"
#include "model_path.h"

static srmodel_list_t *g_models = NULL;
static const esp_wn_iface_t *g_iface = NULL;
static model_iface_data_t *g_data = NULL;
static int g_chunk = 0;

int sr_wake_init(const char *keyword)
{
    (void)keyword;
    printf("[shim] init start\n");
    fflush(stdout);
    g_models = esp_srmodel_init("model");
    if (g_models == NULL) {
        printf("[shim] esp_srmodel_init(model) failed\n");
        return -1;
    }
    printf("[shim] init done num=%d\n", g_models->num);
    fflush(stdout);

    char *name = NULL;
    for (int i = 0; i < g_models->num; i++) {
        if (g_models->model_name[i] != NULL &&
            (strstr(g_models->model_name[i], "xiaotexiaote") != NULL ||
             strstr(g_models->model_name[i], "小特小特") != NULL)) {
            name = g_models->model_name[i];
            break;
        }
    }
    printf("[shim] selected %s\n", name != NULL ? name : "(none)");
    fflush(stdout);
    if (name == NULL) {
        printf("[shim] no xiaotexiaote WakeNet model in partition\n");
        return -2;
    }

    g_iface = esp_wn_handle_from_name(name);
    if (g_iface == NULL) {
        printf("[shim] esp_wn_handle_from_name(%s) failed\n", name);
        return -3;
    }

    g_data = g_iface->create(name, DET_MODE_95);
    if (g_data == NULL) {
        printf("[shim] wakenet create(%s) failed\n", name);
        return -4;
    }

    g_chunk = g_iface->get_samp_chunksize(g_data);
    printf("[shim] model=%s chunk=%d rate=%d\n",
           name, g_chunk, g_iface->get_samp_rate(g_data));
    return g_chunk;
}

int sr_wake_detect(const int16_t *pcm)
{
    if (g_iface == NULL || g_data == NULL) {
        return -1;
    }
    return g_iface->detect(g_data, (int16_t *)pcm) == WAKENET_DETECTED ? 1 : 0;
}

void sr_wake_reset(void)
{
    if (g_iface != NULL && g_data != NULL) {
        g_iface->clean(g_data);
    }
}

void sr_wake_deinit(void)
{
    if (g_iface != NULL && g_data != NULL) {
        g_iface->destroy(g_data);
    }
    if (g_models != NULL) {
        esp_srmodel_deinit(g_models);
    }
    g_iface = NULL;
    g_data = NULL;
    g_models = NULL;
    g_chunk = 0;
}
