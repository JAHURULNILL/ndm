# WebView2 runtime setup for older Windows — run silently by the
# NDM installer (scripts\installer.nsi extracts this file to
# $PLUGINSDIR and executes it during the Install section).
#
# Why this exists: the app window is drawn by Microsoft's WebView2
# runtime. It ships in-box on Windows 10/11 but was never part of
# Windows 7/8/8.1, and Microsoft has retired every download of the
# last build that still runs there (109.0.1518.78, the final
# Windows 7/8.1-compatible release). A pinned copy of that genuine,
# Microsoft-signed installer is therefore self-hosted on this
# project's GitHub (release tag "webview2-runtime-v109") and this
# script fetches it from there.
#
# Contract (the installer's invisibility rule): fully silent, no
# prompts, no windows; always exits 0; never blocks forever. If
# the runtime is already present this returns immediately. Any
# failure at all — offline, timeout, hash mismatch — skips the
# step silently and lets the NDM install complete.
#
# Compatibility notes: only .NET APIs that exist since .NET 2.0 /
# PowerShell 2.0 are used (no Invoke-WebRequest, no Get-FileHash —
# both are newer than Windows 8.0's in-box PowerShell 3.0 / 2.0).

$ErrorActionPreference = 'SilentlyContinue'

$wv2Guid          = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
$wv2Url           = 'https://github.com/JAHURULNILL/ndm/releases/download/webview2-runtime-v109/MicrosoftEdgeWebView2RuntimeInstallerX64.exe'
$wv2Sha256        = 'f2cc49863e221402d4664d9fa944a47f22543da2dcb363438c3e598789b9fd5b'
$downloadDeadline = 900    # seconds, whole-transfer cap
$installTimeout   = 600000 # milliseconds

function Test-WebView2Present {
    $keys = @(
        "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\$wv2Guid",
        "HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\$wv2Guid",
        "HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\$wv2Guid"
    )
    foreach ($key in $keys) {
        $pv = (Get-ItemProperty -Path $key -Name 'pv').pv
        if ($pv -and $pv -ne '0.0.0.0') { return $true }
    }
    return $false
}

try {
    if (Test-WebView2Present) { exit 0 }

    # GitHub refuses TLS below 1.2; the pre-.NET-4.6 default is
    # lower, so switch 1.2 on. Numeric on purpose: the Tls12 enum
    # member does not exist on the oldest frameworks, the value
    # itself is understood wherever the OS supports it.
    [System.Net.ServicePointManager]::SecurityProtocol =
        [System.Net.ServicePointManager]::SecurityProtocol -bor 3072

    $target = Join-Path $env:TEMP 'MicrosoftEdgeWebView2RuntimeInstallerX64.exe'
    Remove-Item $target -Force

    # Download with explicit timeouts and a hard overall deadline,
    # so a dead or trickling link can never wedge the installer.
    $request = [System.Net.HttpWebRequest]::Create($wv2Url)
    $request.UserAgent = 'NillDownloadManager-Setup'
    $request.Timeout = 60000
    $request.ReadWriteTimeout = 300000
    $request.AllowAutoRedirect = $true
    $response = $request.GetResponse()
    $inStream = $response.GetResponseStream()
    $outStream = [System.IO.File]::Create($target)
    $buffer = New-Object byte[] 65536
    $deadline = (Get-Date).AddSeconds($downloadDeadline)
    try {
        while ($true) {
            $n = $inStream.Read($buffer, 0, $buffer.Length)
            if ($n -le 0) { break }
            $outStream.Write($buffer, 0, $n)
            if ((Get-Date) -gt $deadline) { throw 'download deadline exceeded' }
        }
    } finally {
        $outStream.Close()
        $inStream.Close()
        $response.Close()
    }

    # Verify before trusting anything downloaded: SHA-256 via .NET
    # directly (Get-FileHash only exists on PowerShell 4+).
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $stream = [System.IO.File]::OpenRead($target)
    try { $hashBytes = $sha.ComputeHash($stream) }
    finally { $stream.Close(); $sha.Dispose() }
    $actual = ([System.BitConverter]::ToString($hashBytes)).Replace('-', '').ToLowerInvariant()
    if ($actual -ne $wv2Sha256) {
        Remove-Item $target -Force
        exit 0
    }

    # Install quietly with a bounded wait; tidy up either way.
    $proc = Start-Process -FilePath $target -ArgumentList '/silent', '/install' -PassThru
    if ($proc) {
        if (-not $proc.WaitForExit($installTimeout)) {
            Stop-Process -Id $proc.Id -Force
        }
    }
    Remove-Item $target -Force
} catch {
    # Stay silent: the NDM install completes regardless.
}
exit 0
