; Nill Download Manager — per-user NSIS installer.
;
; Packages the assembled portable tree (the exact folder the
; portable ZIP wraps) so the installed layout is identical to the
; portable one, which is what the app's tool discovery expects:
;
;   <InstallDir>\ndm-app.exe
;   <InstallDir>\ndm-host.exe          (beside the app)
;   <InstallDir>\extension\*           (browser extension)
;   <InstallDir>\installed.flag        (install-flavour marker —
;                                       the app's self-update reads
;                                       this exact file name)
;   <InstallDir>\uninstall.exe
;   <InstallDir>\ucrtbase.dll + api-ms-win-*.dll
;                                       (app-local UCRT — below)
;
; The video engine (yt-dlp, deno, ffmpeg) is deliberately NOT in
; the package — that is what keeps this installer IDM-sized. The
; app fetches it once, silently in the background on first launch,
; into %APPDATA%\com.ndm.downloadmanager\components (hash-verified
; against pins in the app), exactly as the Linux build does. An
; install over a pre-slim version leaves that version's bundled
; <InstallDir>\components folder untouched on disk, and the app's
; tool discovery still finds the engine there — upgraders never
; re-download it.
;
; Per-user install (RequestExecutionLevel user, HKCU only): no
; admin prompt, installs under %LOCALAPPDATA%\Programs. Silent
; mode (/S) is fully functional — the app's self-update flow and
; CI both drive it that way; in silent mode the app is relaunched
; at the end (see .onInstSuccess), in interactive mode the finish
; page offers the standard "Run Nill Download Manager" checkbox.
;
; Build (CI, from the repo root):
;   makensis /DVERSION=<x.y.z> /DSRCROOT=<assembled portable dir> \
;            /DOUTFILE=<NDM_<x.y.z>_x64-setup.exe> scripts\installer.nsi
;
; WebView2 on older Windows: the app window needs Microsoft's
; WebView2 runtime — in-box on Windows 10/11, absent on 7/8/8.1,
; and Microsoft has retired every download of the last build that
; still runs there (109.0.1518.78). The Install section therefore
; runs scripts\webview2-setup.ps1, which silently fetches a
; pinned, hash-verified copy of that genuine installer from this
; project's own GitHub (release tag webview2-runtime-v109) and
; installs it in the background. Nothing is shown to the user in
; any mode; machines that already have the runtime skip the step
; instantly, and any failure leaves the NDM install to complete.

!ifndef VERSION
  !define VERSION "2.0.5"
!endif
!ifndef SRCROOT
  !define SRCROOT "NDM-Windows-Portable"
!endif
!ifndef OUTFILE
  !define OUTFILE "NDM_${VERSION}_x64-setup.exe"
!endif
!ifndef ICON
  !define ICON "..\src-tauri\icons\icon.ico"
!endif
!define APPNAME "Nill Download Manager"
!define REGKEY "Software\Nill Download Manager"
!define UNREGKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Nill Download Manager"

Unicode True
Name "${APPNAME}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\${APPNAME}"
InstallDirRegKey HKCU "${REGKEY}" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma
Icon "${ICON}"
UninstallIcon "${ICON}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APPNAME}"
VIAddVersionKey "FileDescription" "${APPNAME} Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "CompanyName" "Nill"
VIAddVersionKey "LegalCopyright" ""

!include "LogicLib.nsh"
!include "MUI2.nsh"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_FINISHPAGE_RUN "$INSTDIR\ndm-app.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Run ${APPNAME}"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_WELCOME
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH
!insertmacro MUI_LANGUAGE "English"

; In silent mode there is no finish page, so the "Run" checkbox
; never fires: relaunch here instead. That is what the app's
; self-update expects — it starts this installer with /S and
; exits; when setup succeeds, the new copy comes back up on its
; own. A short sleep first shrinks the race with the old process
; still holding the single-instance lock while it shuts down.
Function .onInstSuccess
  ${If} ${Silent}
    Sleep 2000
    Exec '"$INSTDIR\ndm-app.exe"'
  ${EndIf}
FunctionEnd

Section "Install"
  SetOutPath "$INSTDIR"

  ; Replace the binaries rename-aside first: renaming a running
  ; .exe is allowed on Windows, so an instance that is still
  ; shutting down (self-update hands over exactly like that)
  ; cannot lock the new copy out. The .old files are best-effort
  ; deleted below and by the app on its next start.
  ${If} ${FileExists} "$INSTDIR\ndm-app.exe"
    Delete "$INSTDIR\ndm-app.exe.old"
    Rename "$INSTDIR\ndm-app.exe" "$INSTDIR\ndm-app.exe.old"
  ${EndIf}
  File "${SRCROOT}\ndm-app.exe"
  ${If} ${FileExists} "$INSTDIR\ndm-host.exe"
    Delete "$INSTDIR\ndm-host.exe.old"
    Rename "$INSTDIR\ndm-host.exe" "$INSTDIR\ndm-host.exe.old"
  ${EndIf}
  File "${SRCROOT}\ndm-host.exe"
  ; WebView2 loader DLL: ndm-app.exe links the loader through this
  ; DLL (see src-tauri/Cargo.toml [patch] note) — it must sit beside
  ; the exe. The DLL itself imports only KERNEL32.
  File "${SRCROOT}\WebView2Loader.dll"
  ; App-local Universal CRT: Windows 7/8 carry no in-box UCRT,
  ; and ndm-app.exe still references six api-ms-win-crt-*
  ; forwarders out of Microsoft's static UCRT (see the
  ; .cargo/config.toml note). The SDK redist set is copied into
  ; SRCROOT by CI's Assemble step and lands beside the exes, so
  ; the loader resolves the forwarders from the app's own
  ; folder; on Windows 10/11 nothing changes in practice.
  File "${SRCROOT}\ucrtbase.dll"
  File "${SRCROOT}\api-ms-win-*.dll"

  SetOutPath "$INSTDIR\extension"
  File /r "${SRCROOT}\extension\*.*"

  SetOutPath "$INSTDIR"
  ; Flavour marker: exact file name is a contract with the app —
  ; its presence beside the exe is how the self-update tells an
  ; installed copy from a portable one.
  FileOpen $0 "$INSTDIR\installed.flag" w
  FileWrite $0 "${VERSION}"
  FileClose $0

  Delete "$INSTDIR\ndm-app.exe.old"
  Delete "$INSTDIR\ndm-host.exe.old"

  ; WebView2 runtime for older Windows — see the header comment
  ; and scripts\webview2-setup.ps1. The script does nothing when
  ; the runtime is already present, is fully silent in every mode,
  ; always exits 0 and is internally time-bounded; the /TIMEOUT
  ; here is only a last-resort guard so this step can never wedge
  ; the installer. Its exit code is ignored by design.
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  File "webview2-setup.ps1"
  nsExec::ExecToLog /TIMEOUT=1650000 'powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "$PLUGINSDIR\webview2-setup.ps1"'
  Pop $0
  SetOutPath "$INSTDIR"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Shortcuts: Start Menu folder + desktop.
  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\ndm-app.exe"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk" "$INSTDIR\uninstall.exe"
  CreateShortcut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\ndm-app.exe"

  ; App key + Add/Remove Programs entry (per-user, HKCU).
  WriteRegStr HKCU "${REGKEY}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${REGKEY}" "Version" "${VERSION}"
  WriteRegStr HKCU "${UNREGKEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKCU "${UNREGKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNREGKEY}" "Publisher" "Nill"
  WriteRegStr HKCU "${UNREGKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNREGKEY}" "DisplayIcon" "$INSTDIR\ndm-app.exe"
  WriteRegStr HKCU "${UNREGKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNREGKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNREGKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNREGKEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$DESKTOP\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"

  DeleteRegKey HKCU "${UNREGKEY}"
  DeleteRegKey HKCU "${REGKEY}"

  ; Program files only. User data under %APPDATA%\
  ; com.ndm.downloadmanager (settings, queue, native-messaging
  ; manifests) and the browser NativeMessagingHosts keys are
  ; deliberately left behind, so a reinstall picks everything
  ; back up — the standard per-user app behaviour.
  Delete "$INSTDIR\ndm-app.exe"
  Delete "$INSTDIR\ndm-app.exe.old"
  Delete "$INSTDIR\ndm-host.exe"
  Delete "$INSTDIR\ndm-host.exe.old"
  Delete "$INSTDIR\WebView2Loader.dll"
  Delete "$INSTDIR\ucrtbase.dll"
  Delete "$INSTDIR\api-ms-win-*.dll"
  Delete "$INSTDIR\installed.flag"
  ; a pre-slim install's bundled engine folder, if one is there —
  ; slim installs never create it (their engine lives in %APPDATA%,
  ; in the user data this section deliberately leaves behind)
  RMDir /r "$INSTDIR\components"
  RMDir /r "$INSTDIR\extension"
  RMDir /r "$INSTDIR\.ndm-update-staging"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  ${If} ${FileExists} "$INSTDIR\ndm-app.exe"
    ; Something (usually a still-running instance) held a file:
    ; finish the removal on reboot rather than failing.
    RMDir /r /REBOOTOK "$INSTDIR"
  ${EndIf}
SectionEnd
