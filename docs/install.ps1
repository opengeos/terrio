# Installs the terrio command-line tool from its GitHub release, on Windows:
#
#     powershell -ExecutionPolicy ByPass -c "irm https://terrio.opengeos.org/install.ps1 | iex"
#
# Settings, from the environment:
#   TERRIO_VERSION         the release to install (0.4.0 or v0.4.0); the latest by default
#   TERRIO_INSTALL_DIR     where to put the binary; %USERPROFILE%\.local\bin by default
#   TERRIO_DOWNLOAD_URL    the directory the archives are downloaded from, for a mirror; the
#                          GitHub release by default
#   TERRIO_NO_MODIFY_PATH  set to leave the user's PATH as it is
#
# On Linux and macOS, use install.sh.

function Install-Terrio {
    $ErrorActionPreference = "Stop"
    # Invoke-WebRequest's progress bar slows downloads down many times in Windows PowerShell.
    $ProgressPreference = "SilentlyContinue"
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = "opengeos/terrio"
    $arch = $env:PROCESSOR_ARCHITECTURE
    try { $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() } catch {}
    switch ($arch) {
        { $_ -in "X64", "AMD64" } { $target = "x86_64-pc-windows-msvc" }
        "ARM64" { $target = "aarch64-pc-windows-msvc" }
        default { throw "No terrio binary for $arch; install it with pip install terrio or cargo install terrio-cli" }
    }

    $version = $env:TERRIO_VERSION
    if (-not $version) {
        try {
            $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
        } catch {
            throw "Could not find the latest release of $repo; set TERRIO_VERSION to the version to install"
        }
    }
    $version = "v" + $version.TrimStart("v")

    $installDir = $env:TERRIO_INSTALL_DIR
    if (-not $installDir) { $installDir = Join-Path $env:USERPROFILE ".local\bin" }
    $base = $env:TERRIO_DOWNLOAD_URL
    if (-not $base) { $base = "https://github.com/$repo/releases/download/$version" }
    $name = "terrio-cli-$version-$target"

    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "terrio-installer: downloading terrio $version for $target"
        $zip = Join-Path $tmp "$name.zip"
        try {
            Invoke-WebRequest "$base/$name.zip" -OutFile $zip -UseBasicParsing
            Invoke-WebRequest "$base/$name.zip.sha256" -OutFile "$zip.sha256" -UseBasicParsing
        } catch {
            throw "Could not download terrio $version for $target ($($_.Exception.Message)); install it with pip install terrio or cargo install terrio-cli"
        }
        $expected = ((Get-Content "$zip.sha256" -Raw).Trim() -split "\s+")[0]
        if ((Get-FileHash $zip -Algorithm SHA256).Hash -ne $expected.ToUpper()) {
            throw "Checksum mismatch for $name.zip"
        }

        Expand-Archive $zip -DestinationPath $tmp
        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        Copy-Item (Join-Path $tmp "$name\terrio.exe") (Join-Path $installDir "terrio.exe") -Force
        Write-Host "terrio-installer: installed $(Join-Path $installDir 'terrio.exe')"
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }

    if ($env:TERRIO_NO_MODIFY_PATH) {
        if (-not (($env:Path -split ";") -contains $installDir)) {
            Write-Host "terrio-installer: $installDir is not on your PATH"
        }
    } else {
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        if (-not (($userPath -split ";") -contains $installDir)) {
            [Environment]::SetEnvironmentVariable("Path", (@($installDir) + ($userPath -split ";" | Where-Object { $_ })) -join ";", "User")
            Write-Host "terrio-installer: added $installDir to your PATH"
        }
        # This session too (the script runs in the caller's, with irm | iex), which may predate
        # the user PATH having it.
        if (-not (($env:Path -split ";") -contains $installDir)) { $env:Path = "$installDir;$env:Path" }
    }
    & (Join-Path $installDir "terrio.exe") --version
}

Install-Terrio
