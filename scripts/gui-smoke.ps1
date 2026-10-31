# NDM GUI smoke test — drives the REAL Windows GUI of ndm-app.exe
# through the Chrome DevTools Protocol (CDP) endpoint that
# WebView2 exposes when the app is started with
#   WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222
#
# Pure PowerShell + System.Net.WebSockets.ClientWebSocket: no
# external modules, no downloads. Works in Windows PowerShell 5.1
# and PowerShell 7.
#
# What it does:
#   1. Starts a local fixture HTTP server (python -m http.server)
#      over -FixtureDir and checks the fixture file answers.
#   2. Launches the app itself (with the CDP env var set) unless a
#      CDP endpoint is already live, then connects to the page
#      target over the websocket.
#   3. Asserts the UI booted (document title + #btnAdd present).
#   4. Screenshot 01-main.png.
#   5. Drives the real Add-URL flow: click #btnAdd, type the
#      fixture URL into #inUrl, set #inDir, click #btnStart.
#   6. Polls the DOM until the download reports completion (the
#      "Download complete" modal or a Complete row), then asserts
#      the file exists in -DownloadDir with the exact fixture size.
#   7. Closes the completion modal, screenshot 02-completed.png.
#
# Exit codes (kept distinct on purpose — CI tells these apart):
#   0  GUI SMOKE PASS
#   1  GUI SMOKE FAIL — a real assertion failed (details printed)
#   2  GUI UNVERIFIED — the CDP endpoint never appeared, so the
#      GUI layer could not be driven at all. This is reported
#      honestly; it is NOT a pass. (WebView2 may refuse the
#      debugging port in a non-interactive session.)

[CmdletBinding()]
param(
  [string]$AppExe = "$env:LOCALAPPDATA\Programs\Nill Download Manager\ndm-app.exe",
  [string]$FixtureDir = "$PSScriptRoot\..\gui-fixture",
  [string]$FixtureFile = "ndm-gui-fixture.bin",
  [int]$FixturePort = 8977,
  [string]$DownloadDir = "$env:USERPROFILE\Downloads",
  [string]$ShotsDir = "$PSScriptRoot\..\gui-shots",
  [int]$Port = 9222,
  [int]$CdpTimeoutSec = 45,
  [int]$DownloadTimeoutSec = 120,
  [string]$ExpectTitle = "Nill Download Manager"
)

$ErrorActionPreference = "Stop"
$script:ws = $null
$script:msgId = 0
$script:appProc = $null
$script:serverProc = $null
$script:exitCode = 1

function Log([string]$m) { Write-Host "GUI-SMOKE: $m" }

# ---------- CDP plumbing ----------

function Receive-CdpMessage {
  $ms = New-Object System.IO.MemoryStream
  $buf = New-Object byte[] 65536
  while ($true) {
    $cts = New-Object System.Threading.CancellationTokenSource
    $cts.CancelAfter(20000)
    try {
      $seg = New-Object System.ArraySegment[byte] -ArgumentList (, $buf)
      $r = $script:ws.ReceiveAsync($seg, $cts.Token).GetAwaiter().GetResult()
    } catch {
      throw "CDP receive failed or timed out: $($_.Exception.Message)"
    } finally {
      $cts.Dispose()
    }
    if ($r.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) {
      throw "CDP socket closed by the app"
    }
    $ms.Write($buf, 0, $r.Count)
    if ($r.EndOfMessage) { break }
  }
  return [System.Text.Encoding]::UTF8.GetString($ms.ToArray())
}

function Send-Cdp([string]$Method, $Params) {
  $script:msgId += 1
  $id = $script:msgId
  if ($null -eq $Params) { $Params = @{} }
  $payload = @{ id = $id; method = $Method; params = $Params } |
    ConvertTo-Json -Depth 20 -Compress
  $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
  $seg = New-Object System.ArraySegment[byte] -ArgumentList (, $bytes)
  $script:ws.SendAsync($seg, [System.Net.WebSockets.WebSocketMessageType]::Text,
    $true, [System.Threading.CancellationToken]::None).GetAwaiter().GetResult()
  while ($true) {
    $text = Receive-CdpMessage
    $obj = $text | ConvertFrom-Json
    if ($obj.id -eq $id) {
      if ($obj.error) { throw "CDP $Method error: $($obj.error.message)" }
      return $obj.result
    }
    # otherwise it is a CDP event notification — not ours, keep waiting
  }
}

function Invoke-Js([string]$Expr) {
  $res = Send-Cdp "Runtime.evaluate" @{
    expression    = $Expr
    returnByValue = $true
    awaitPromise  = $true
  }
  if ($res.exceptionDetails) {
    $msg = ""
    if ($res.exceptionDetails.exception) { $msg = $res.exceptionDetails.exception.description }
    throw "page JS threw: $msg"
  }
  return $res.result.value
}

function Save-Shot([string]$Path) {
  $res = Send-Cdp "Page.captureScreenshot" @{ format = "png" }
  [System.IO.File]::WriteAllBytes($Path, [Convert]::FromBase64String($res.data))
  Log "screenshot saved: $Path"
}

# ---------- main ----------

try {
  if (-not (Test-Path $AppExe)) { throw "app exe not found: $AppExe" }
  $fixturePath = Join-Path $FixtureDir $FixtureFile
  if (-not (Test-Path $fixturePath)) { throw "fixture file not found: $fixturePath" }
  $expectedSize = (Get-Item $fixturePath).Length
  New-Item -ItemType Directory -Force $ShotsDir | Out-Null
  New-Item -ItemType Directory -Force $DownloadDir | Out-Null
  $targetPath = Join-Path $DownloadDir $FixtureFile
  if (Test-Path $targetPath) { Remove-Item $targetPath -Force }

  # --- fixture server (this script owns it end-to-end) ---
  $py = Get-Command python -ErrorAction SilentlyContinue
  if (-not $py) { $py = Get-Command py -ErrorAction SilentlyContinue }
  if (-not $py) { throw "no python found to serve the fixture" }
  $script:serverProc = Start-Process $py.Source -PassThru -WindowStyle Hidden `
    -ArgumentList "-m", "http.server", "$FixturePort", "--directory", "`"$FixtureDir`""
  $fixtureUrl = "http://127.0.0.1:$FixturePort/$FixtureFile"
  $served = $false
  foreach ($i in 1..30) {
    try {
      $h = Invoke-WebRequest -UseBasicParsing -Method Head -Uri $fixtureUrl -TimeoutSec 2
      if ($h.StatusCode -eq 200) { $served = $true; break }
    } catch { Start-Sleep -Milliseconds 500 }
  }
  if (-not $served) { throw "fixture server did not answer at $fixtureUrl" }
  Log "fixture serving: $fixtureUrl ($expectedSize bytes)"

  # --- launch the app with the debugging port, if needed ---
  function Test-CdpUp {
    try {
      $r = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/json/list" -TimeoutSec 2
      return ($r.StatusCode -eq 200)
    } catch { return $false }
  }
  if (-not (Test-CdpUp)) {
    # a leftover instance without debugging would swallow the new
    # launch via the single-instance handover — clear it first
    Get-Process ndm-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$Port"
    $script:appProc = Start-Process $AppExe -PassThru
    Log "launched app (pid $($script:appProc.Id)) with CDP port $Port"
  } else {
    Log "CDP endpoint already live — attaching to the running app"
  }

  # --- wait for the page target ---
  $wsUrl = $null
  $deadline = (Get-Date).AddSeconds($CdpTimeoutSec)
  while ((Get-Date) -lt $deadline) {
    try {
      $list = (Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/json/list" -TimeoutSec 2).Content |
        ConvertFrom-Json
      foreach ($t in $list) {
        if ($t.type -eq "page" -and $t.webSocketDebuggerUrl) { $wsUrl = $t.webSocketDebuggerUrl; break }
      }
      if ($wsUrl) { break }
    } catch { }
    Start-Sleep -Seconds 1
  }
  if (-not $wsUrl) {
    Log "GUI UNVERIFIED: CDP endpoint never appeared on port $Port within ${CdpTimeoutSec}s (WebView2 may refuse the debugging port in this session) — the GUI layer remains unverified on Windows"
    # NB: a bare `return` here would exit the script with code 0
    # and fake a pass — throw the marker instead; the catch block
    # maps it to the distinct exit code 2.
    throw "CDP_UNVERIFIED_MARKER"
  }

  # --- connect ---
  $script:ws = New-Object System.Net.WebSockets.ClientWebSocket
  $script:ws.ConnectAsync([Uri]$wsUrl, [System.Threading.CancellationToken]::None).GetAwaiter().GetResult()
  Send-Cdp "Page.enable" @{} | Out-Null
  Send-Cdp "Runtime.enable" @{} | Out-Null
  Start-Sleep -Seconds 2

  # --- boot assertion ---
  $boot = Invoke-Js "JSON.stringify({ title: document.title, add: !!document.getElementById('btnAdd') })" | ConvertFrom-Json
  Log "page title: '$($boot.title)' (Add URL button present: $($boot.add))"
  if (-not $boot.add) { throw "app UI did not boot: #btnAdd missing from the DOM" }
  if ($boot.title -notlike "*$ExpectTitle*") { throw "unexpected page title: '$($boot.title)'" }
  Save-Shot (Join-Path $ShotsDir "01-main.png")

  # --- drive the Add-URL flow ---
  Invoke-Js "document.getElementById('btnAdd').click(); 'clicked'" | Out-Null
  $opened = $false
  foreach ($i in 1..20) {
    $cls = Invoke-Js "document.getElementById('modal').className"
    if ($cls -notlike "*hidden*") { $opened = $true; break }
    Start-Sleep -Milliseconds 500
  }
  if (-not $opened) {
    $visible = Invoke-Js "[...document.querySelectorAll('.modal:not(.hidden)')].map(m => m.id).join(',')"
    throw "Add-URL dialog did not open (visible modals: $visible)"
  }
  $urlJs = ConvertTo-Json $fixtureUrl
  $dirJs = ConvertTo-Json $DownloadDir
  Invoke-Js @"
(function () {
  var u = document.getElementById('inUrl');
  u.focus(); u.value = $urlJs;
  u.dispatchEvent(new Event('input', { bubbles: true }));
  u.dispatchEvent(new Event('change', { bubbles: true }));
  var d = document.getElementById('inDir');
  if (d) {
    d.value = $dirJs;
    d.dispatchEvent(new Event('input', { bubbles: true }));
    d.dispatchEvent(new Event('change', { bubbles: true }));
  }
  return document.getElementById('inUrl').value;
})()
"@ | Out-Null
  Start-Sleep -Seconds 2  # let the dialog's probe settle
  Invoke-Js "document.getElementById('btnStart').click(); 'started'" | Out-Null
  Log "download started from the GUI: $fixtureUrl -> $DownloadDir"

  # --- poll for completion ---
  $nameJs = ConvertTo-Json $FixtureFile
  $done = $false
  $deadline = (Get-Date).AddSeconds($DownloadTimeoutSec)
  while ((Get-Date) -lt $deadline) {
    $state = Invoke-Js @"
JSON.stringify({
  cmodal: (function () { var c = document.getElementById('cmodal'); return !!c && !c.classList.contains('hidden'); })(),
  rowComplete: (function () {
    var t = document.body.innerText || '';
    return t.indexOf($nameJs) >= 0 && t.indexOf('Complete') >= 0;
  })()
})
"@ | ConvertFrom-Json
    if (($state.cmodal -or $state.rowComplete) -and (Test-Path $targetPath)) { $done = $true; break }
    Start-Sleep -Seconds 2
  }
  if (-not $done) { throw "download did not complete within ${DownloadTimeoutSec}s (file on disk: $(Test-Path $targetPath))" }
  $actualSize = (Get-Item $targetPath).Length
  if ($actualSize -ne $expectedSize) { throw "downloaded size $actualSize != fixture size $expectedSize" }
  Log "download completed through the GUI; file verified on disk ($actualSize bytes)"

  # --- close the completion modal, screenshot the finished list ---
  Invoke-Js "(function () { var c = document.getElementById('cmodal'); if (c && !c.classList.contains('hidden')) { document.getElementById('cModalClose').click(); } return 'ok'; })()" | Out-Null
  Start-Sleep -Seconds 1
  Save-Shot (Join-Path $ShotsDir "02-completed.png")

  Log "GUI SMOKE PASS"
  $script:exitCode = 0
} catch {
  if ($_.Exception.Message -eq "CDP_UNVERIFIED_MARKER") {
    # the UNVERIFIED line was already printed where it was thrown
    $script:exitCode = 2
  } else {
    Log "GUI SMOKE FAIL: $($_.Exception.Message)"
    if ($script:ws -and $script:ws.State -eq [System.Net.WebSockets.WebSocketState]::Open) {
      try { Save-Shot (Join-Path $ShotsDir "99-failure.png") } catch { }
    }
    $script:exitCode = 1
  }
} finally {
  if ($script:ws) { try { $script:ws.Dispose() } catch { } }
  if ($script:appProc) {
    try { Stop-Process -Id $script:appProc.Id -Force -ErrorAction SilentlyContinue } catch { }
  }
  if ($script:serverProc) {
    try { Stop-Process -Id $script:serverProc.Id -Force -ErrorAction SilentlyContinue } catch { }
  }
}

exit $script:exitCode
