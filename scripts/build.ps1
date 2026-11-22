<#
.SYNOPSIS
    Build Rustboard in release mode and sign the executable.
.DESCRIPTION
    Runs `cargo build --release` and signs target\release\rustboard.exe with the
    local "CN=Rustboard Development" certificate from Cert:\CurrentUser\My.

    The executable must not be running while it is signed. If no development
    certificate exists yet, pass -CreateCertificate once to create one and trust
    it for the current user.
.EXAMPLE
    .\scripts\build.ps1
.EXAMPLE
    .\scripts\build.ps1 -CreateCertificate
#>
[CmdletBinding()]
param(
    [switch]$CreateCertificate
)

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $repo 'Cargo.toml'
$targetDir = if ($env:CARGO_TARGET_DIR) {
    [System.IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
} else {
    Join-Path $repo 'target'
}
$binaries = @('rustboard.exe', 'rustboard-mcp.exe', 'todo.exe')
$exes = $binaries | ForEach-Object { Join-Path $targetDir "release\$_" }
$certSubject = 'CN=Rustboard Development, O=Rustboard'
$certFolder = Join-Path $env:USERPROFILE '.rustboard\signing'

function Find-Cargo {
    $command = Get-Command cargo -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $candidates = @(Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe')
    $toolchains = Join-Path $env:USERPROFILE '.rustup\toolchains'
    if (Test-Path $toolchains) {
        $candidates += Get-ChildItem $toolchains -Directory |
            ForEach-Object { Join-Path $_.FullName 'bin\cargo.exe' }
    }
    foreach ($candidate in $candidates) {
        if (Test-Path $candidate) {
            return $candidate
        }
    }
    throw 'cargo.exe not found. Install Rust, or add the toolchain bin folder to PATH.'
}

function Get-DevCertificate {
    Get-ChildItem Cert:\CurrentUser\My |
        Where-Object { $_.Subject -like 'CN=Rustboard Development*' -and $_.HasPrivateKey } |
        Sort-Object NotAfter -Descending |
        Select-Object -First 1
}

function New-DevCertificate {
    New-Item -ItemType Directory -Force -Path $certFolder | Out-Null

    $cert = New-SelfSignedCertificate -Type CodeSigningCert `
        -Subject $certSubject `
        -CertStoreLocation 'Cert:\CurrentUser\My' `
        -KeyExportPolicy Exportable `
        -KeyUsage DigitalSignature `
        -NotAfter (Get-Date).AddYears(5)

    $publicPath = Join-Path $certFolder 'rustboard-dev.cer'
    Remove-Item $publicPath -ErrorAction SilentlyContinue
    Export-Certificate -Cert $cert -FilePath $publicPath | Out-Null

    $public = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 $publicPath
    foreach ($storeName in 'Root', 'TrustedPublisher') {
        $store = New-Object System.Security.Cryptography.X509Certificates.X509Store(
            $storeName,
            [System.Security.Cryptography.X509Certificates.StoreLocation]::CurrentUser)
        $store.Open([System.Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite)
        try {
            $store.Add($public)
        } finally {
            $store.Close()
        }
    }

    Write-Host "Created development certificate $($cert.Thumbprint) and trusted it for the current user."
    return $cert
}

if (Get-Process rustboard -ErrorAction SilentlyContinue) {
    throw 'Rustboard is running. Close it before building so the executable can be replaced and signed.'
}

$cargo = Find-Cargo
$cargoDir = Split-Path -Parent $cargo
if ($env:PATH -notlike "*$cargoDir*") {
    # Build scripts spawn rustc by name, so the toolchain folder must be on PATH
    # even when cargo itself was found by full path.
    $env:PATH = "$cargoDir;$env:PATH"
}
Write-Host "cargo:  $cargo"
Write-Host "target: $targetDir"

& $cargo build --release --bins --manifest-path $manifest
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE."
}
foreach ($exe in $exes) {
    if (-not (Test-Path $exe)) {
        throw "Build finished but $exe was not produced."
    }
}

$cert = Get-DevCertificate
if (-not $cert) {
    if (-not $CreateCertificate) {
        throw 'No Rustboard development certificate found. Run .\scripts\build.ps1 -CreateCertificate once.'
    }
    $cert = New-DevCertificate
}

foreach ($exe in $exes) {
    $result = Set-AuthenticodeSignature -FilePath $exe -Certificate $cert
    if ($result.Status -ne 'Valid') {
        throw "Signing failed for $exe: $($result.Status) - $($result.StatusMessage)"
    }

    $verified = Get-AuthenticodeSignature $exe
    if ($verified.Status -ne 'Valid') {
        throw "The signature for $exe did not verify: $($verified.Status) - $($verified.StatusMessage)"
    }

    Write-Host ''
    Write-Host "Built and signed: $exe"
    Write-Host "Publisher: $($verified.SignerCertificate.Subject)"
    Write-Host "Status:    $($verified.Status)"
}
