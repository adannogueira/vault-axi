$ErrorActionPreference = "Stop"

$Repo = if ($env:VAULT_REPO) { $env:VAULT_REPO } else { "adannogueira/vault-axi" }
$InstallDir = if ($env:VAULT_INSTALL_DIR) { $env:VAULT_INSTALL_DIR } else { Join-Path $HOME ".local\bin" }
$Version = if ($env:VAULT_VERSION) { $env:VAULT_VERSION } else { "latest" }
$AssetName = "vault-windows-x86_64.exe"

if ($Version -eq "latest") {
    $BaseUrl = "https://github.com/$Repo/releases/latest/download"
} else {
    $BaseUrl = "https://github.com/$Repo/releases/download/$Version"
}

$BinUrl = "$BaseUrl/$AssetName"
$ShaUrl = "$BaseUrl/$AssetName.sha256"

Write-Host "==> Downloading $AssetName from $Repo ($Version)..." -ForegroundColor Cyan

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null

try {
    $TmpBin = Join-Path $TmpDir $AssetName
    $TmpSha = Join-Path $TmpDir "$AssetName.sha256"

    Invoke-WebRequest -Uri $BinUrl -OutFile $TmpBin -UseBasicParsing
    try {
        Invoke-WebRequest -Uri $ShaUrl -OutFile $TmpSha -UseBasicParsing
    } catch {
        # Checksum file is optional
    }

    if (Test-Path $TmpSha) {
        $ExpectedHash = ((Get-Content $TmpSha -Raw).Trim() -split "\s+")[0].ToLowerInvariant()
        $ActualHash = (Get-FileHash -Path $TmpBin -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($ExpectedHash -and ($ExpectedHash -ne $ActualHash)) {
            throw "SHA-256 checksum mismatch! Expected $ExpectedHash, got $ActualHash"
        }
        Write-Host "==> Verified SHA-256 checksum." -ForegroundColor Cyan
    }

    if (-not (Test-Path $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }

    $DestPath = Join-Path $InstallDir "vault.exe"
    Move-Item -Path $TmpBin -Destination $DestPath -Force
    Write-Host "==> Installed vault to $DestPath" -ForegroundColor Green

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathEntries = if ($UserPath) { $UserPath -split ";" } else { @() }
    if ($PathEntries -notcontains $InstallDir) {
        $NewUserPath = if ($UserPath) { "$InstallDir;$UserPath" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
        $env:Path = "$InstallDir;$env:Path"
        Write-Host "==> Added $InstallDir to your User PATH." -ForegroundColor Cyan
    }

    Write-Host "`nGet started:"
    Write-Host "  vault register --email you@example.com --password <pw>"
    Write-Host "  vault --help"
} finally {
    Remove-Item -Path $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
