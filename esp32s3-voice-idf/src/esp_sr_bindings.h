// Bindings for the official esp-sr component.
//
// This header only *includes* the official esp-sr public headers so that
// esp-idf-sys/bindgen can generate Rust bindings for them. No C code of the
// component is modified or wrapped here.
#pragma once

#if defined(ESP_IDF_COMP_ESPRESSIF__ESP_SR_ENABLED)
#include "esp_vad.h"
#include "esp_wn_iface.h"
#include "esp_wn_models.h"
#include "model_path.h"
#endif
