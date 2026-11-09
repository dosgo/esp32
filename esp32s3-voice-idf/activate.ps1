$projectDrive = Split-Path -Qualifier $PSScriptRoot
$managedTools = Join-Path $projectDrive '.embuild\espressif'
$managedIdf = Join-Path $managedTools 'esp-idf\v5.4.2'

# Honour a valid caller-supplied IDF first, otherwise use embuild's local copy.
if (-not $env:IDF_PATH -or -not (Test-Path (Join-Path $env:IDF_PATH 'export.ps1'))) {
    $env:IDF_PATH = $managedIdf
}
if (-not (Test-Path (Join-Path $env:IDF_PATH 'export.ps1'))) {
    throw "ESP-IDF v5.4.2 not found. Run cargo build once to let embuild install it."
}
if (-not $env:IDF_TOOLS_PATH -or -not (Test-Path $env:IDF_TOOLS_PATH)) {
    $env:IDF_TOOLS_PATH = $managedTools
}
# extra components / bindings are declared in extra_components.toml
$env:ESP_IDF_SYS_EXTRA_COMPONENTS_FILE = "$PSScriptRoot\extra_components.toml"
$env:ESP_IDF_SDKCONFIG_DEFAULTS = "$PSScriptRoot\sdkconfig.defaults"
$previousErrorAction = $ErrorActionPreference
$ErrorActionPreference = 'Stop'
try {
    . "$env:IDF_PATH\export.ps1"
} finally {
    $ErrorActionPreference = $previousErrorAction
}

