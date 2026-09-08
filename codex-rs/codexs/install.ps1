# codexs installer (Windows x86_64). Works in Windows PowerShell 5.1 and PowerShell 7.
#
#   irm https://raw.githubusercontent.com/meglinge/codex/codexs/codex-rs/codexs/install.ps1 | iex
#
# Environment overrides:
#   CODEXS_VERSION      release version to install, e.g. 0.153.4 (default: latest release)
#   CODEXS_INSTALL_DIR  where binaries + config live (default: $HOME\.codexs)
#   CODEXS_REPO         GitHub repo publishing the releases (default: meglinge/codex)
$ErrorActionPreference = 'Stop'

$Repo = if ($env:CODEXS_REPO) { $env:CODEXS_REPO } else { 'meglinge/codex' }
$Version = if ($env:CODEXS_VERSION) { $env:CODEXS_VERSION } else { 'latest' }
$InstallDir = if ($env:CODEXS_INSTALL_DIR) { $env:CODEXS_INSTALL_DIR } else { Join-Path $HOME '.codexs' }
$Target = 'x86_64-pc-windows-msvc'

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Green }

$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne 'AMD64') {
    throw "unsupported architecture '$arch': only x86_64 builds are published"
}
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch {}

if ($Version -eq 'latest') {
    $api = "https://api.github.com/repos/$Repo/releases/latest"
} else {
    $Version = $Version -replace '^codexs-v', '' -replace '^v', ''
    $api = "https://api.github.com/repos/$Repo/releases/tags/codexs-v$Version"
}

Write-Step "Resolving release ($($api.Split('/')[-1]))"
$release = Invoke-RestMethod -Uri $api -Headers @{ 'Accept' = 'application/vnd.github+json'; 'User-Agent' = 'codexs-installer' }
$asset = $release.assets | Where-Object { $_.name -like "codexs-*-$Target.zip" } | Select-Object -First 1
if (-not $asset) { throw "no $Target asset found in release $($release.tag_name)" }
$archive = $asset.name
$resolved = $archive -replace '^codexs-', '' -replace "-$Target\.zip$", ''

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("codexs-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    Write-Step "Downloading codexs $resolved ($archive)"
    $zip = Join-Path $tmp $archive
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zip -UseBasicParsing
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $src = Join-Path $tmp "codexs-$resolved-$Target"
    if (-not (Test-Path (Join-Path $src 'codexs.exe'))) { throw 'archive did not contain codexs.exe' }

    $binDir = Join-Path $InstallDir 'bin'
    Write-Step "Installing into $binDir"
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    # Helper binaries (codex-code-mode-host, sandbox helpers) must stay next to
    # codexs.exe; replace the whole bin directory so stale helpers never linger.
    $binNew = Join-Path $InstallDir 'bin.new'
    if (Test-Path $binNew) { Remove-Item -Recurse -Force $binNew }
    Copy-Item -Path $src -Destination $binNew -Recurse -Force
    if (Test-Path $binDir) {
        $binOld = Join-Path $InstallDir 'bin.old'
        if (Test-Path $binOld) { Remove-Item -Recurse -Force $binOld }
        try {
            Move-Item -Path $binDir -Destination $binOld -Force
        } catch {
            throw "could not replace $binDir - is codexs still running? ($($_.Exception.Message))"
        }
        Move-Item -Path $binNew -Destination $binDir -Force
        Remove-Item -Recurse -Force $binOld -ErrorAction SilentlyContinue
    } else {
        Move-Item -Path $binNew -Destination $binDir -Force
    }

    $config = Join-Path $InstallDir 'codexs.toml'
    if (-not (Test-Path $config)) {
        Copy-Item (Join-Path $binDir 'codexs.example.toml') $config
        Write-Step "Created $config from the example - edit accounts / api_keys before starting"
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @()
    if ($userPath) { $parts = $userPath -split ';' | Where-Object { $_ } }
    if ($parts -notcontains $binDir) {
        [Environment]::SetEnvironmentVariable('Path', (($parts + $binDir) -join ';'), 'User')
        Write-Step "Added $binDir to your user PATH (open a new terminal to pick it up)"
    }
    if (($env:Path -split ';') -notcontains $binDir) { $env:Path = "$binDir;$env:Path" }

    $installed = & (Join-Path $binDir 'codexs.exe') --version
    Write-Step "Installed: $installed"
    Write-Host ''
    Write-Host 'Next steps:'
    Write-Host '  One account per instance (credentials + proxy at startup, downstream needs none):'
    Write-Host '      codexs server --port 8790 --proxy socks5h://127.0.0.1:1080 --codex-home $HOME\.codex'
    Write-Host '      codexs server --help      # --access-token / --auth-file / --api-key ...'
    Write-Host '  Or the account pool from a config file:'
    Write-Host "      edit $config, then run:  codexs"
    Write-Host "      (config lookup: `$env:CODEXS_CONFIG, .\codexs.toml, $config)"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
