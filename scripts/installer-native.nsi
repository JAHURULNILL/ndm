; Nill Download Manager — per-user NSIS installer (NATIVE variant).
;
; NATIVE: This file is the Phase-4 release-swap replacement for
; scripts\installer.nsi (NDM Native UI Programme, NATIVE-PLAN.md
; §4 Phase 4). It is NOT used by any build tonight: the live
; 2.0.9 release path keeps building scripts\installer.nsi
; byte-untouched. At the Phase-4 swap (first native Windows
; release, 2.1.0) this file replaces installer.nsi, and
; scripts\webview2-setup.ps1 is deleted in the same change.
;
; NATIVE: Full list of deltas vs installer.nsi — nothing else
; differs; every other line is byte-identical to installer.nsi:
;   D1. This header block (native identity + swap instructions
;       + this delta list).
;   D2. The header paragraph "WebView2 on older Windows: ..."
;       is replaced by the "Native shell:" paragraph below —
;       the native shell needs no WebView2 runtime at all.
;   D3. Install section: the WebView2Loader.dll comment + File
;       line are removed — the native shell exe does not link
;       the WebView2 loader, so no loader DLL ships beside it.
;   D4. Install section: the whole WebView2 runtime block is
;       removed (InitPluginsDir / SetOutPath "$PLUGINSDIR" /
;       File "webview2-setup.ps1" / the nsExec::ExecToLog
;       powershell invocation / Pop $0 / SetOutPath restore) —
;       no runtime is downloaded or installed at install time.
;   D5. Uninstall section: Delete "$INSTDIR\WebView2Loader.dll"
;       is removed — a native install never places that file.
;   D6. "; NATIVE:" marker comments at the app-exe sites
;       (finish-page Run target, silent-mode relaunch, the
;       rename-aside + File pair, shortcuts, DisplayIcon,
;       uninstall deletes) recording that the installed file
;       NAME stays ndm-app.exe while its CONTENT becomes the
;       native shell binary (see the layout note below). No
;       functional change in D6.
;
; NATIVE: App-exe name decision: the native Windows shell
; (shell-win crate, binary ndm-shell-win) ships AS the app exe
; under the existing installed name ndm-app.exe — the CI
; Assemble step copies/renames it into SRCROOT as ndm-app.exe,
; exactly as it places the Tauri binary there today. Keeping
; the name preserves every contract that keys off it: the
; self-update relaunch, shortcuts, the Add/Remove Programs
; DisplayIcon, and the uninstall file list. What changes is
; only the footprint: no WebView2Loader.dll and no embedded
; WebView UI assets; the setup stays in the ≈4.4 MB class
; (the WebView2 runtime was downloaded at install time, never
; inside the setup) and the app exe itself shrinks from
; 14.06 MB to an estimated 6–9 MB (NATIVE-PLAN.md §4).
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
; Native shell: the app window is drawn by raw Win32 controls
; (the windows crate) — there is no WebView, no WebView2
; runtime and no webview2-setup.ps1 step anywhere in this
; installer. Nothing is downloaded at install time beyond the
; setup's own payload, in any mode, on any Windows version
; the app supports (7/8/8.1/10/11).

!ifndef VERSION
  !define VERSION "2.1.0"
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
; NATIVE: finish-page Run target — file name unchanged (ndm-app.exe); its content is now the native shell binary.
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
    ; NATIVE: silent-mode relaunch target — same file name, native shell binary.
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
  ; NATIVE: rename-aside + install of the app exe — same file
  ; name and same SRCROOT slot; SRCROOT\ndm-app.exe now holds
  ; the native shell binary (renamed by CI's Assemble step).
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
  ; NATIVE: no WebView2Loader.dll File line here — delta D3:
  ; the native shell does not link the WebView2 loader, so the
  ; loader DLL is not in SRCROOT and is not installed.
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

  ; NATIVE: no WebView2 runtime block here — delta D4: the
  ; InitPluginsDir / webview2-setup.ps1 extraction / nsExec
  ; powershell invocation from installer.nsi are gone. The
  ; native shell needs no runtime provisioning of any kind,
  ; so installation proceeds straight to the uninstaller,
  ; shortcuts and registry below, in silent and interactive
  ; modes alike (the silent-install layout behaviour is
  ; otherwise unchanged).

  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Shortcuts: Start Menu folder + desktop.
  ; NATIVE: shortcut targets — same file name, native shell binary.
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
  ; NATIVE: DisplayIcon — same file name, native shell binary (its icon resource is the app icon).
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
  ; NATIVE: uninstall file list — same names, native shell
  ; binary; no WebView2Loader.dll delete (delta D5, below).
  Delete "$INSTDIR\ndm-app.exe"
  Delete "$INSTDIR\ndm-app.exe.old"
  Delete "$INSTDIR\ndm-host.exe"
  Delete "$INSTDIR\ndm-host.exe.old"
  ; NATIVE: no Delete "$INSTDIR\WebView2Loader.dll" here —
  ; delta D5: a native install never places that file. (An
  ; upgrade-in-place over a Tauri install is handled by the
  ; install section's layout plus the /REBOOTOK sweep below;
  ; the stale loader file, if any, is inert beside the native
  ; exe, which never loads it.)
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
