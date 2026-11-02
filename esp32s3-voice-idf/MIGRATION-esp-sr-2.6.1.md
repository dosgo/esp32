# 迁移记录：esp-sr 2.1.2 → 2.6.1、并去掉 C shim（2026-11-02）

## 结果

`D:\idf` 现在：

- 使用组件仓库的 **esp-sr 2.6.1**（官方组件，自动解析 esp-dl / dl_fft / esp-dsp / esp_new_jpeg）；
- 项目里**没有任何 C 源文件**，WakeNet 直接由 Rust 调用官方 C API；
- 编译通过：干净构建 ~3 分 40 秒，app 2.4 MB，`srmodels.bin` 291 KB（含 `wn9_xiaotexiaote_tts2`）。

| 组件 | 版本 |
|---|---|
| espressif/esp-sr | 2.6.1 |
| espressif/esp-dl | 3.3.14（传递依赖） |
| espressif/dl_fft | 0.8.0 |
| espressif/esp-dsp | 1.8.0 |
| espressif/esp_new_jpeg | 1.x（传递依赖） |
| espressif/cjson | 1.7.19 |
| espressif/esp_websocket_client | 1.2.0 |

## 架构：Rust 直接调官方 API

```
src/main.rs         业务逻辑
  └─ src/wakenet.rs  安全封装（unsafe 只在这一个文件里）
       └─ esp_idf_sys::esp_wn_* / esp_srmodel_*   ← bindgen 生成
            └─ managed_components/espressif__esp-sr（官方组件，未改动）
```

- `src/esp_sr_bindings.h`：**只 include 官方头文件**，用于让 esp-idf-sys 的 bindgen 生成绑定，不含任何实现；
- `src/wakenet.rs`：`WakeNet::new("xiaotexiaote")` → `esp_srmodel_init("model")` → `esp_srmodel_filter` →
  `esp_wn_handle_from_name` → `create(DET_MODE_95)`；`detect()` 比对 `WAKENET_DETECTED`；`Drop` 里
  `destroy` + `esp_srmodel_deinit`；
- 原来的 `components/esp_sr_shim/`（93 行 C）已删除，备份在
  `D:\idf\.migration-backup-20261102-1100\esp_sr_shim`。

## 配置文件改动

| 文件 | 改动 |
|---|---|
| `activate.ps1` | 保留 `ESP_IDF_SYS_EXTRA_COMPONENTS_FILE`（组件必须走这个 env 文件，见下） |
| `extra_components.toml` | `bindings_header` + 5 个 remote_component |
| `Cargo.toml` | 同步同样内容；不再有 `component_dirs` |
| `sdkconfig.defaults` | `CONFIG_MODEL_IN_FLASH=y`、`CONFIG_SR_WN_WN9_XIAOTEXIAOTE_TTS2=y` |
| `build.rs` | 链接顺序 workaround（-lstdc++） |
| `flash.bat` | 从构建目录取 `srmodels.bin`，写 `0x410000` |

## 两个必须注意的点

**1. `bindings_header` 必须用绝对路径**

esp-idf-sys 0.38 在 `cargo metadata` 失败时（它的 CWD 在 `D:\t\...build\esp-idf-sys-*` 里，向上找不到
`Cargo.toml`），拿不到根 crate 的 manifest 目录，于是 `manifest_dir` 为空：

```
Error: extra components C header file 'src/esp_sr_bindings.h' specified by crate '' does not exist
```

所以 `extra_components.toml` / `Cargo.toml` 里写的是 `D:\\idf\\src\\esp_sr_bindings.h`。
同理，组件也只能通过 `ESP_IDF_SYS_EXTRA_COMPONENTS_FILE`（env 文件）配置，写在 Cargo.toml 里的
`extra_components` 在这个项目里不会被读到。

**2. 模型必须显式选，否则 srmodels.bin 只有 4 字节**

`CONFIG_SR_WN_WN9_XIAOTEXIAOTE_TTS2=y`，否则 `movemodel.py` 不打包任何模型。

## 编译 / 烧录

```powershell
cd D:\idf
powershell -NoProfile -ExecutionPolicy Bypass -Command ". .\activate.ps1; cargo build --release -j 20"
D:\idf\flash.bat COM4
```

## 回滚

- 旧 C shim：`.migration-backup-20261102-1100\esp_sr_shim`
- 旧 2.1.2 组件：`D:\esp32\vendor\esp-sr-2.1.2`
- 迁移前整盘备份：`D:\idf-backup-20261102-1045`

## 2026-11-02 真机调试：`clean()` 会崩，已移除 reset

用 `assets/wake16k.raw` 直接喂 WakeNet 自测（临时加了一段 include_bytes! 的代码）：

```text
[idf-kws] model=wn9_xiaotexiaote_tts2 chunk=512 rate=16000
[selftest] fed 36 frames (18575 samples), hits=1     ← detect 正常，能命中
```

但紧接着调用 `esp_wn_iface_t::clean`：

```text
Guru Meditation Error: Core 0 panic'ed (LoadProhibited)
```

addr2line 定位到 `model_clean`（wakenet9_quantized.c）→ `dl_convq_queue_bzero`，即 esp-sr 2.6.1 的
clean 在 wn9 量化模型上解引用了 NULL 队列。这是官方库内部的问题，不是绑定写错。

处理：`src/wakenet.rs` 不再提供 `reset()`，需要清状态就直接 drop 整个 `WakeNet` 再 `new()`。
去掉该调用后，干净固件上电 30 秒无 panic，mic 正常出数。
