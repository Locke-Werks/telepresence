<#
.SYNOPSIS
    Builds TelePresence for Windows and stages the installer payload.

.DESCRIPTION
    Two phases, because signing has to happen between them in CI:

      build     cargo build --release, CMake builds of the virtual camera and
                the setup helper. Our own binaries land in target\windows\own.
      assemble  Copies the (by now signed) binaries from own\ into
                target\windows\stage, runs windeployqt, and copies the
                GStreamer runtime: the core DLLs, only the plugins the app
                uses, and every DLL those import.

    Default is both. Locally, package.ps1 runs build, signs, then assemble.

.PARAMETER QtDir
    Qt kit root, e.g. C:\Qt\6.11.3\msvc2022_64. Defaults to QT_ROOT_DIR (set by
    install-qt-action), then the newest C:\Qt\6.*\msvc2022_64.

.PARAMETER GstRoot
    GStreamer MSVC root. Defaults to GSTREAMER_1_0_ROOT_MSVC_X86_64.
#>
param(
    [ValidateSet('build', 'assemble', 'all')]
    [string]$Phase = 'all',
    [string]$QtDir,
    [string]$GstRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Work = Join-Path $Repo 'target\windows'
$Own = Join-Path $Work 'own'
$Stage = Join-Path $Work 'stage'

# --- toolchain discovery ----------------------------------------------------

if (-not $QtDir) { $QtDir = $env:QT_ROOT_DIR }
if (-not $QtDir) {
    $QtDir = Get-ChildItem 'C:\Qt' -Directory -Filter '6.*' -ErrorAction SilentlyContinue |
        Sort-Object { [version]$_.Name } -Descending |
        ForEach-Object { Join-Path $_.FullName 'msvc2022_64' } |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $QtDir -or -not (Test-Path "$QtDir\bin\qmake.exe")) { throw "Qt kit not found (QtDir=$QtDir)" }

if (-not $GstRoot) { $GstRoot = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 }
if (-not $GstRoot -or -not (Test-Path "$GstRoot\bin\gstreamer-1.0-0.dll")) { throw "GStreamer MSVC runtime not found (GstRoot=$GstRoot)" }
$GstRoot = $GstRoot.TrimEnd('\')

$VsWhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$VsRoot = & $VsWhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $VsRoot) { throw 'Visual Studio with the C++ x64 tools not found' }

Write-Host "Qt:        $QtDir"
Write-Host "GStreamer: $GstRoot"
Write-Host "VS:        $VsRoot"

function Invoke-CMakeProject([string]$Source, [string]$Build) {
    # The VS generator finds MSVC itself, so no developer prompt is needed.
    cmake -S $Source -B $Build -G 'Visual Studio 17 2022' -A x64 | Out-Host
    cmake --build $Build --config Release --parallel | Out-Host
}

# --- build ------------------------------------------------------------------

if ($Phase -in 'build', 'all') {
    $env:QMAKE = "$QtDir\bin\qmake.exe"
    # gstreamer-rs resolves its libraries through pkg-config. The MSVC
    # distribution ships its own pkg-config.exe, which beats whatever else is
    # on PATH (Strawberry Perl's pkg-config.bat in particular).
    $env:PKG_CONFIG = "$GstRoot\bin\pkg-config.exe"
    $env:PKG_CONFIG_PATH = "$GstRoot\lib\pkgconfig"

    Push-Location $Repo
    try { cargo build --release --locked -p tp-app --bin telepresence | Out-Host }
    finally { Pop-Location }

    Invoke-CMakeProject (Join-Path $Repo 'drivers\windows\vcam') (Join-Path $Work 'build\vcam')
    Invoke-CMakeProject (Join-Path $PSScriptRoot 'helper') (Join-Path $Work 'build\helper')

    if (Test-Path $Own) { Remove-Item $Own -Recurse -Force }
    New-Item -ItemType Directory -Force "$Own\VirtualCamera" | Out-Null
    Copy-Item "$Repo\target\release\telepresence.exe" $Own
    Copy-Item "$Work\build\helper\Release\tp-setup-helper.exe" $Own
    Copy-Item "$Work\build\vcam\Release\tp-vcam.dll", "$Work\build\vcam\Release\tp-vcam-register.exe" "$Own\VirtualCamera"
    Write-Host "Own binaries in $Own"
}

# --- assemble ---------------------------------------------------------------

if ($Phase -in 'assemble', 'all') {
    if (-not (Test-Path "$Own\telepresence.exe")) { throw "nothing built in $Own; run -Phase build first" }
    if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
    Copy-Item $Own $Stage -Recurse

    # Qt. The compiler runtime is copied below for Qt and GStreamer together,
    # app-local, rather than as a vc_redist.exe nobody would run.
    & "$QtDir\bin\windeployqt.exe" --release --no-compiler-runtime --no-translations `
        --no-opengl-sw --qmldir (Join-Path $Repo 'crates\tp-app\qml') `
        "$Stage\telepresence.exe" | Out-Host

    # GStreamer plugins the app constructs, by plugin file name. Element to
    # plugin: tee/queue/capsfilter coreelements; appsrc/appsink app;
    # autovideosrc/autoaudiosrc/sinks autodetect; videoconvert/videoscale
    # videoconvertscale; videoflip videofilter; h264parse videoparsersbad;
    # avdec_h264 libav; jpegdec jpeg (MJPEG webcams); nvh264enc/dec nvcodec;
    # qsvh264enc qsv; d3d11h264dec and the preview sink d3d11; mfvideosrc and
    # mfh264enc mediafoundation; capture and playback wasapi2 (wasapi as the
    # fallback autodetect picks on older builds).
    $plugins = @(
        'coreelements', 'app', 'autodetect', 'videoconvertscale', 'videorate', 'videofilter',
        'videotestsrc', 'audioconvert', 'audioresample', 'audiotestsrc', 'opus', 'videoparsersbad',
        'libav', 'jpeg', 'nvcodec', 'qsv', 'd3d11', 'mediafoundation', 'wasapi2', 'wasapi',
        # GPL; present only in some GStreamer builds. Software fallback encoder.
        'x264'
    )
    $plugDir = Join-Path $Stage 'gstreamer-1.0'
    New-Item -ItemType Directory -Force $plugDir | Out-Null
    foreach ($p in $plugins) {
        $src = "$GstRoot\lib\gstreamer-1.0\gst$p.dll"
        if (Test-Path $src) { Copy-Item $src $plugDir }
        elseif ($p -eq 'x264') { Write-Warning 'gstx264.dll not in this GStreamer; x264enc fallback unavailable' }
        else { throw "required GStreamer plugin missing: $src" }
    }
    # Out-of-process registry scan: a plugin that crashes while loading takes
    # down the scanner instead of the app.
    Copy-Item "$GstRoot\libexec\gstreamer-1.0\gst-plugin-scanner.exe" $Stage

    # Dependency closure. Anything a staged binary imports that lives in the
    # GStreamer bin dir is copied beside the exe, repeated until nothing new
    # turns up. System DLLs and Qt's own are not in that dir, so they drop out.
    $dumpbin = Get-ChildItem "$VsRoot\VC\Tools\MSVC" -Directory | Sort-Object { [version]$_.Name } -Descending |
        ForEach-Object { "$($_.FullName)\bin\Hostx64\x64\dumpbin.exe" } | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $dumpbin) { throw 'dumpbin.exe not found' }
    $gstBin = @{}
    Get-ChildItem "$GstRoot\bin" -Filter *.dll | ForEach-Object { $gstBin[$_.Name.ToLowerInvariant()] = $_.FullName }

    $queue = [System.Collections.Generic.Queue[string]]::new()
    $seen = @{}
    Get-ChildItem $Stage -Recurse -Include *.exe, *.dll | ForEach-Object { $queue.Enqueue($_.FullName) }
    while ($queue.Count) {
        $file = $queue.Dequeue()
        $deps = & $dumpbin /nologo /dependents $file | ForEach-Object { $_.Trim() } | Where-Object { $_ -match '\.dll$' }
        foreach ($d in $deps) {
            $k = $d.ToLowerInvariant()
            if ($seen[$k] -or -not $gstBin[$k]) { continue }
            $seen[$k] = $true
            $dest = Join-Path $Stage $d
            if (-not (Test-Path $dest)) { Copy-Item $gstBin[$k] $dest }
            $queue.Enqueue($dest)
        }
    }
    Write-Host "GStreamer: $($plugins.Count) plugins requested, $($seen.Count) runtime DLLs"

    # Microsoft C/C++ runtime, app-local. Qt, GStreamer and the Rust binary are
    # all /MD builds; app-local copies are a supported deployment and save the
    # user a separate redistributable install.
    $crt = Get-ChildItem "$VsRoot\VC\Redist\MSVC" -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^\d+\.' } | Sort-Object { [version]$_.Name } -Descending |
        ForEach-Object { Get-ChildItem "$($_.FullName)\x64" -Directory -Filter 'Microsoft.VC*.CRT' -ErrorAction SilentlyContinue } |
        Select-Object -First 1
    if (-not $crt) { throw 'MSVC redistributable CRT directory not found' }
    Copy-Item "$($crt.FullName)\*.dll" $Stage -Force

    # GStreamer's LGPL obligations travel with its DLLs.
    $licenses = "$GstRoot\share\licenses"
    if (Test-Path $licenses) { Copy-Item $licenses "$Stage\licenses\gstreamer" -Recurse }
    Copy-Item (Join-Path $Repo 'LICENSE') $Stage -ErrorAction SilentlyContinue

    $size = (Get-ChildItem $Stage -Recurse -File | Measure-Object Length -Sum).Sum
    Write-Host ("Staged {0} files, {1:N0} MB in {2}" -f (Get-ChildItem $Stage -Recurse -File).Count, ($size / 1MB), $Stage)
}
