# ESP32-S3 Voice Assistant（ESP-IDF + std Rust + 官方 esp-sr）

独立工程，不修改原来的 no_std/esp-hal 固件。

## 当前状态：I2S 麦克风 + WakeNet 已跑通

串口实测：

```text
[es7210] ready (attempt 2)
[shim] selected wn9_xiaotexiaote_tts2
[shim] model=wn9_xiaotexiaote_tts2 chunk=512 rate=16000
[idf-kws] WakeNet chunk=512
[i2s] mic_ch=0 rms_l=11988 rms_r=8286
[idf-kws] DETECTED after 441 frames
```

- ESP-IDF v5.4.2；
- `esp-idf-svc 0.53`（std Rust，build-std）；
- 官方 esp-sr 2.1.2（源码和库版本一致）；
- I2S0 RX：BCLK=9 / DIN=11 / MCLK=8 / WS=10，16k/16bit；
- ES7210 麦克风 ADC：I2C SDA=42 / SCL=41，地址 0x40；
- 实时麦克风数据直接喂 WakeNet，检测到「小特小特」。

还没接 WiFi / WebSocket / 显示。

## 关键配置

之前崩溃的根因不是模型/阈值，而是 **PSRAM 没开**：

- `sdkconfig.defaults` 没被 `esp-idf-sys` 应用，生成的是单 app 分区、无 PSRAM；
- `flash_model_info()` 需要 PSRAM 分配，malloc 返回 NULL，`strncpy(NULL, ...)` 直接 `StoreProhibited`。

现在通过 `activate.ps1` 设置：

```powershell
$env:ESP_IDF_SDKCONFIG_DEFAULTS = 'D:\idf\sdkconfig.defaults'
```

并确认生成的 sdkconfig 包含：

```text
CONFIG_SPIRAM=y
CONFIG_PARTITION_TABLE_CUSTOM=y
```

## 目录

```text
D:\idf
├── activate.ps1
├── flash.bat
├── Cargo.toml
├── build.rs
├── sdkconfig.defaults
├── partitions.csv               # factory 4MB + model 4MB
├── components/
│   ├── esp-sr/                  # 官方 2.1.2
│   └── esp_sr_shim/             # C shim：Rust -> WakeNet
├── assets/wake16k.raw
└── src/main.rs
```

## 编译

```powershell
cd D:\idf
powershell -NoProfile -ExecutionPolicy Bypass -Command ". .\activate.ps1; cargo build --release -j 20"
```

输出在 `D:\t`。

## 烧录

```bat
D:\idf\flash.bat COM4
```

它会烧：

1. bootloader；
2. `partitions.csv`（factory + model）；
3. app；
4. `components/esp-sr/model/srmodels.bin` 到 `0x410000`。

## 下一步

1. 命中唤醒后进入会话状态，接 WebSocket + PCM 协议；
2. 接 TTS 播放（I2S TX + ES8311）；
3. 迁显示/按键/配网。

WakeNet + 实时麦克风已经验证可用，后面是应用层移植。
