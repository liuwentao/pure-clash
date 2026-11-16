#Requires -Version 7.0
<#
.SYNOPSIS
构建 release 可执行文件并生成 Windows 安装包或便携 ZIP。

.PARAMETER NsisPath
可选的 makensis.exe 完整路径。为空时依次检查 PATH 和 NSIS 默认安装目录。

.PARAMETER PackageFormat
输出格式：Installer（默认）仅生成 NSIS 安装包，Portable 仅生成便携 ZIP，
All 同时生成两种产物。Portable 不需要安装 NSIS。

.OUTPUTS
在 dist 目录生成带 Cargo 包版本号的 Windows x64 安装程序和/或便携 ZIP。
ZIP 包含 pure-clash 根目录；首次运行时由程序生成 config、data 和 log 目录。
#>
[CmdletBinding()]
param(
    [string]$NsisPath,
    [ValidateSet("Installer", "Portable", "All")]
    [string]$PackageFormat = "Installer"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$manifestPath = Join-Path $projectRoot "Cargo.toml"
$nsisScript = Join-Path $PSScriptRoot "installer.nsi"

function Get-CanonicalUtf8TextSha256 {
    param([Parameter(Mandatory)][string]$LiteralPath)

    # Git 在 Windows 上可能把官方 LF 文本检出为 CRLF。许可证清单锚定上游
    # UTF-8/LF 原文，因此仅规范换行后计算哈希；非法 UTF-8 仍会立即失败。
    $bytes = [IO.File]::ReadAllBytes($LiteralPath)
    $utf8 = [Text.UTF8Encoding]::new($false, $true)
    $text = $utf8.GetString($bytes).Replace("`r`n", "`n").Replace("`r", "`n")
    $canonicalBytes = $utf8.GetBytes($text)
    return [Convert]::ToHexString(
        [Security.Cryptography.SHA256]::HashData($canonicalBytes)
    ).ToLowerInvariant()
}

Push-Location $projectRoot
try {
    & cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release 执行失败。"
    }

    $metadataJson = (& cargo metadata --no-deps --format-version 1 | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata 执行失败。"
    }

    $metadata = $metadataJson | ConvertFrom-Json
    $package = $metadata.packages |
        Where-Object { [IO.Path]::GetFullPath($_.manifest_path) -eq [IO.Path]::GetFullPath($manifestPath) } |
        Select-Object -First 1
    if (-not $package) {
        throw "无法从 cargo metadata 中定位当前包。"
    }

    $version = [string]$package.version
    $coreVersion = ($version -split "[-+]")[0]
    $versionParts = $coreVersion -split "\."
    if ($versionParts.Count -lt 3) {
        throw "Cargo 包版本必须至少包含 major.minor.patch。"
    }
    $numericVersion = "{0}.{1}.{2}.0" -f $versionParts[0], $versionParts[1], $versionParts[2]

    # 使用 Cargo 返回的实际 target 目录，兼容 CARGO_TARGET_DIR 和隔离构建。
    $cargoTargetDir = [IO.Path]::GetFullPath([string]$metadata.target_directory)
    $appExe = Join-Path $cargoTargetDir "release\pure-clash.exe"
    if (-not (Test-Path -LiteralPath $appExe -PathType Leaf)) {
        throw "release 可执行文件不存在：$appExe"
    }
    $appIcon = Join-Path $projectRoot "assets\windows\pure-clash.ico"
    if (-not (Test-Path -LiteralPath $appIcon -PathType Leaf)) {
        throw "Windows 应用图标不存在：$appIcon"
    }

    # 项目自身的 GPL-3.0 许可文本随安装包分发，与内核许可证分别履行义务。
    $projectLicense = Join-Path $projectRoot "LICENSE"
    if (-not (Test-Path -LiteralPath $projectLicense -PathType Leaf)) {
        throw "未找到项目 LICENSE 文件：$projectLicense"
    }

    # 仅携带下载清单与许可信息，三份数据库由应用首次设置时校验下载。
    $geodataRoot = Join-Path $projectRoot "geodata"
    $geodataManifest = Join-Path $geodataRoot "manifest.json"
    $geodataLicense = Join-Path $geodataRoot "LICENSE"
    $geodataNotice = Join-Path $geodataRoot "NOTICE.md"
    foreach ($requiredGeoFile in @($geodataManifest, $geodataLicense, $geodataNotice)) {
        if (-not (Test-Path -LiteralPath $requiredGeoFile -PathType Leaf)) {
            throw "内置 Geo 数据资源不存在：$requiredGeoFile"
        }
    }
    $geodataMetadata = Get-Content -Raw -LiteralPath $geodataManifest | ConvertFrom-Json
    if ($geodataMetadata.schema_version -ne 1 -or
        $geodataMetadata.source_repository -ne "https://github.com/MetaCubeX/meta-rules-dat" -or
        $geodataMetadata.source_branch -ne "release" -or
        [string]$geodataMetadata.source_commit -notmatch '^[0-9a-fA-F]{40}$') {
        throw "Geo 数据 manifest 元数据无效：$geodataManifest"
    }
    $expectedGeoNames = @("Country.mmdb", "GeoIP.dat", "GeoSite.dat")
    $actualGeoNames = @($geodataMetadata.files | ForEach-Object { [string]$_.name })
    if ($actualGeoNames.Count -ne $expectedGeoNames.Count -or
        @($actualGeoNames | Sort-Object -Unique).Count -ne $expectedGeoNames.Count -or
        @(Compare-Object ($expectedGeoNames | Sort-Object) ($actualGeoNames | Sort-Object)).Count -ne 0) {
        throw "Geo 数据 manifest 必须且只能包含三份基础数据。"
    }
    $geodataLicenseHash = Get-CanonicalUtf8TextSha256 -LiteralPath $geodataLicense
    if ($geodataLicenseHash -ne [string]$geodataMetadata.license_sha256) {
        throw "Geo 数据 LICENSE SHA-256 与 manifest 不一致。"
    }
    $geodataFiles = @($geodataManifest, $geodataLicense, $geodataNotice)

    $distDir = Join-Path $projectRoot "dist"
    New-Item -ItemType Directory -Force -Path $distDir | Out-Null
    $installerOutput = Join-Path $distDir "pure-clash-$version-windows-x64-setup.exe"
    $packagedFiles = @($appExe, $projectLicense) + $geodataFiles
    $installSizeBytes = ($packagedFiles |
        ForEach-Object { (Get-Item -LiteralPath $_).Length } |
        Measure-Object -Sum).Sum
    $installSizeKb = [int][Math]::Ceiling($installSizeBytes / 1KB)

    if ($PackageFormat -in @("Portable", "All")) {
        $portableOutput = Join-Path $distDir "pure-clash-$version-windows-x64-portable.zip"
        # 按已校验的发行文件白名单组装 ZIP，不打包 target/release 下的用户配置、
        # 日志或其他平台内核；根目录保持稳定，解压后可直接运行 pure-clash.exe。
        $zipEntries = [ordered]@{
            "pure-clash/pure-clash.exe" = $appExe
            "pure-clash/LICENSE" = $projectLicense
        }
        foreach ($source in $geodataFiles) {
            $zipEntries["pure-clash/geodata/$([IO.Path]::GetFileName($source))"] = $source
        }

        # 完整压缩成功后再替换最终文件，失败时不会留下看似可发布的半成品。
        $temporaryZip = "$portableOutput.$([Guid]::NewGuid().ToString('N')).tmp"
        try {
            $archive = [IO.Compression.ZipFile]::Open($temporaryZip, [IO.Compression.ZipArchiveMode]::Create)
            try {
                foreach ($entry in $zipEntries.GetEnumerator()) {
                    [IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                        $archive, $entry.Value, $entry.Key, [IO.Compression.CompressionLevel]::Optimal
                    ) | Out-Null
                }
            }
            finally {
                $archive.Dispose()
            }
            [IO.File]::Move($temporaryZip, $portableOutput, $true)
        }
        finally {
            if ([IO.File]::Exists($temporaryZip)) {
                [IO.File]::Delete($temporaryZip)
            }
        }
        Write-Host "便携包已生成：$portableOutput"
    }
    # 便携包复用全部资源校验，但不探测或调用 NSIS。
    if ($PackageFormat -eq "Portable") {
        return
    }

    # 优先尊重显式参数，其次检查 PATH，最后检查 NSIS 的常见安装目录。
    $nsisCandidates = @()
    if ($NsisPath) {
        $nsisCandidates += $NsisPath
    }
    $nsisCommand = Get-Command "makensis.exe" -ErrorAction SilentlyContinue
    if ($nsisCommand) {
        $nsisCandidates += $nsisCommand.Source
    }
    $nsisCandidates += Join-Path $env:ProgramFiles "NSIS\makensis.exe"
    $nsisCandidates += Join-Path ${env:ProgramFiles(x86)} "NSIS\makensis.exe"

    $makensis = $nsisCandidates |
        Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) } |
        Select-Object -First 1
    if (-not $makensis) {
        throw "未找到 makensis.exe。请安装 NSIS 3.x，或通过 -NsisPath 指定编译器路径。"
    }

    $nsisArguments = @(
        "/V3"
        "/WX"
        # 明确按 UTF-8 读取脚本，避免中文文案受 Windows 系统代码页影响。
        "/INPUTCHARSET"
        "UTF8"
        "/DAPP_VERSION=$version"
        "/DAPP_VERSION_NUMERIC=$numericVersion"
        "/DAPP_EXE_SOURCE=$appExe"
        "/DAPP_ICON_SOURCE=$appIcon"
        "/DGEODATA_ROOT=$geodataRoot"
        "/DPROJECT_LICENSE_SOURCE=$projectLicense"
        "/DINSTALLER_OUTPUT=$installerOutput"
        "/DINSTALL_SIZE_KB=$installSizeKb"
        $nsisScript
    )
    & $makensis @nsisArguments
    if ($LASTEXITCODE -ne 0) {
        throw "NSIS 安装包编译失败。"
    }
    if (-not (Test-Path -LiteralPath $installerOutput -PathType Leaf)) {
        throw "NSIS 未生成预期安装包：$installerOutput"
    }

    Write-Host "安装包已生成：$installerOutput"
}
finally {
    Pop-Location
}
