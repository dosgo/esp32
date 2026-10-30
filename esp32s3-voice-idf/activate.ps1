$env:IDF_PATH = 'D:\e'
$env:IDF_TOOLS_PATH = 'D:\esp32\toolchain\espressif\idf-tools'
$env:ESP_IDF_SYS_EXTRA_COMPONENTS_FILE = "$PSScriptRoot\extra_components.toml"
$env:ESP_IDF_SDKCONFIG_DEFAULTS = "$PSScriptRoot\sdkconfig.defaults"
$py = 'C:\Users\DosGo\AppData\Local\Programs\DeepSeek Harness\resources\runtime\primary-runtime\dependencies\python'
$env:PATH = "$py;$env:PATH"
. "$env:IDF_PATH\export.ps1"

