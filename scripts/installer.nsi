; Nill Download Manager — per-user NSIS installer.
;
; App-exe name decision: the native Windows shell (shell-win
; crate, binary ndm-shell-win) ships AS the app exe under the
; installed name ndm-app.exe — the CI Assemble step
; copies/renames it into SRCROOT as ndm-app.exe. Keeping the
; name preserves every contract that keys off it: the
; self-update relaunch, shortcuts, the Add/Remove Programs
; DisplayIcon, and the uninstall file list.
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
; Universal build (CI): with /DUNIVERSAL=1, SRCROOT instead holds
; both arch payloads as subfolders (SRCROOT\x64, SRCROOT\x86 —
; each a complete portable tree) and the installer extracts the
; one matching the machine at install time: one setup for 32-bit
; and 64-bit Windows alike. Everything past extraction — file
; names, installed.flag, shortcuts, registry, uninstall list —
; is identical to a plain single-arch build.
;
; Native shell: the app window is drawn by raw Win32 controls
; (the windows crate) — there is no WebView, no WebView2
; runtime and no webview2-setup.ps1 step anywhere in this
; installer. Nothing is downloaded at install time beyond the
; setup's own payload, in any mode, on any Windows version
; the app supports (7/8/8.1/10/11).

!ifndef VERSION
  !define VERSION "2.3.5"
!endif
!ifndef SRCROOT
  !define SRCROOT "NDM-Windows-Portable"
!endif
!ifndef OUTFILE
  !define OUTFILE "NDM_${VERSION}_x64-setup.exe"
!endif
!ifndef ICON
  !define ICON "..\assets\icon.ico"
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
!ifdef UNIVERSAL
  ; ${RunningX64} — the runtime machine-arch test for the payload
  ; pick below. Included only in universal builds; a plain build
  ; never sees it.
  !include "x64.nsh"
!endif
!include "MUI2.nsh"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
; finish-page Run target — file name unchanged (ndm-app.exe); its content is now the native shell binary.
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
    ; silent-mode relaunch target — same file name, native shell binary.
    Exec '"$INSTDIR\ndm-app.exe"'
  ${EndIf}
FunctionEnd

; Payload extraction for the files that land flat in $INSTDIR.
; A plain build reads them from the flat SRCROOT, unchanged. A
; UNIVERSAL build carries both arch payloads under SRCROOT and
; extracts the one matching the machine: ${RunningX64} asks the
; OS (the installer itself is a 32-bit process, so on 64-bit
; Windows this is the WOW64 answer — exactly "this machine is
; 64-bit"). The extracted names in $INSTDIR are identical either
; way, so nothing after extraction can tell the modes apart.
!macro NDM_PAYLOAD_FILE _file
  !ifdef UNIVERSAL
    ${If} ${RunningX64}
      File "${SRCROOT}\x64\${_file}"
    ${Else}
      File "${SRCROOT}\x86\${_file}"
    ${EndIf}
  !else
    File "${SRCROOT}\${_file}"
  !endif
!macroend

Section "Install"
  SetOutPath "$INSTDIR"

  ; Replace the binaries rename-aside first: renaming a running
  ; .exe is allowed on Windows, so an instance that is still
  ; shutting down (self-update hands over exactly like that)
  ; cannot lock the new copy out. The .old files are best-effort
  ; deleted below and by the app on its next start.
  ; rename-aside + install of the app exe — same file
  ; name and same SRCROOT slot; SRCROOT\ndm-app.exe now holds
  ; the native shell binary (renamed by CI's Assemble step).
  ${If} ${FileExists} "$INSTDIR\ndm-app.exe"
    Delete "$INSTDIR\ndm-app.exe.old"
    Rename "$INSTDIR\ndm-app.exe" "$INSTDIR\ndm-app.exe.old"
  ${EndIf}
  !insertmacro NDM_PAYLOAD_FILE ndm-app.exe
  ${If} ${FileExists} "$INSTDIR\ndm-host.exe"
    Delete "$INSTDIR\ndm-host.exe.old"
    Rename "$INSTDIR\ndm-host.exe" "$INSTDIR\ndm-host.exe.old"
  ${EndIf}
  !insertmacro NDM_PAYLOAD_FILE ndm-host.exe
  ; no WebView2Loader.dll File line here: the native shell
  ; the native shell does not link the WebView2 loader, so the
  ; loader DLL is not in SRCROOT and is not installed.
  ; App-local Universal CRT: Windows 7/8 carry no in-box UCRT,
  ; and ndm-app.exe still references six api-ms-win-crt-*
  ; forwarders out of Microsoft's static UCRT (see the
  ; .cargo/config.toml note). The SDK redist set is copied into
  ; SRCROOT by CI's Assemble step and lands beside the exes, so
  ; the loader resolves the forwarders from the app's own
  ; folder; on Windows 10/11 nothing changes in practice.
  !insertmacro NDM_PAYLOAD_FILE ucrtbase.dll
  !insertmacro NDM_PAYLOAD_FILE api-ms-win-*.dll

  SetOutPath "$INSTDIR\extension"
  ; The extension payload is the curated set the extension
  ; packager ships — never build artefacts that happen to sit in
  ; the source folder: gen_icons.py is the icon build script and
  ; manifest.firefox.json belongs to the separate Firefox zip
  ; (Firefox reads the manifest.json placed beside it there).
  !ifdef UNIVERSAL
    ${If} ${RunningX64}
      File /r /x gen_icons.py /x manifest.firefox.json "${SRCROOT}\x64\extension\*.*"
    ${Else}
      File /r /x gen_icons.py /x manifest.firefox.json "${SRCROOT}\x86\extension\*.*"
    ${EndIf}
  !else
    File /r /x gen_icons.py /x manifest.firefox.json "${SRCROOT}\extension\*.*"
  !endif

  SetOutPath "$INSTDIR"
  ; Flavour marker: exact file name is a contract with the app —
  ; its presence beside the exe is how the self-update tells an
  ; installed copy from a portable one.
  FileOpen $0 "$INSTDIR\installed.flag" w
  FileWrite $0 "${VERSION}"
  FileClose $0

  Delete "$INSTDIR\ndm-app.exe.old"
  Delete "$INSTDIR\ndm-host.exe.old"

  ; no WebView2 runtime block here: the native shell needs
  ; no runtime provisioning of any kind, so installation
  ; proceeds straight to the uninstaller,
  ; shortcuts and registry below, in silent and interactive
  ; modes alike (the silent-install layout behaviour is
  ; otherwise unchanged).

  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Shortcuts: Start Menu folder + desktop.
  ; shortcut targets — same file name, native shell binary.
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
  ; DisplayIcon — same file name, native shell binary (its icon resource is the app icon).
  WriteRegStr HKCU "${UNREGKEY}" "DisplayIcon" "$INSTDIR\ndm-app.exe"
  WriteRegStr HKCU "${UNREGKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNREGKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNREGKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNREGKEY}" "NoRepair" 1
SectionEnd

; Is ndm-app.exe currently running? Sets $0 to 0 when tasklist
; finds the process, non-zero otherwise. Used by the uninstaller
; before it touches any file.
!macro NDM_CHECK_RUNNING
  nsExec::ExecToStack '"$SYSDIR\cmd.exe" /C tasklist /FI "IMAGENAME eq ndm-app.exe" /NH | findstr /I /C:"ndm-app.exe" > NUL'
  Pop $0
  Pop $1
!macroend

Section "Uninstall"
  ; A still-running instance holds its exe locked and owns the
  ; single-instance port: without this check the deletes below
  ; half-land (shortcuts and registry go, the locked exe stays
  ; for the /REBOOTOK sweep) and the "uninstalled" app keeps
  ; running — and blocking a reinstall's first launch — until
  ; reboot. Ask it to close first: taskkill without /F sends
  ; WM_CLOSE, the graceful path. Interactive uninstalls refuse
  ; while it stays up (nothing has been deleted yet); silent
  ; ones force-close so an unattended uninstall cannot strand
  ; a zombie holding the port.
  !insertmacro NDM_CHECK_RUNNING
  ${If} $0 == 0
    nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /IM ndm-app.exe'
    Sleep 1500
    !insertmacro NDM_CHECK_RUNNING
    ${If} $0 == 0
      ${If} ${Silent}
        nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /IM ndm-app.exe'
        Sleep 500
      ${Else}
        MessageBox MB_OK|MB_ICONEXCLAMATION "${APPNAME} is still running. Close it, then run the uninstaller again."
        Abort
      ${EndIf}
    ${EndIf}
  ${EndIf}

  Delete "$DESKTOP\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"

  DeleteRegKey HKCU "${UNREGKEY}"
  DeleteRegKey HKCU "${REGKEY}"

  ; The browser NativeMessagingHosts keys point at the host
  ; executable this section deletes: left behind, every browser
  ; keeps a live registration for a program that no longer
  ; exists and the extension fails against a dead host. The
  ; app re-writes these keys on every launch, so a reinstall
  ; restores them by itself — removing them here costs nothing.
  DeleteRegKey HKCU "Software\Google\Chrome\NativeMessagingHosts\com.ndm.host"
  DeleteRegKey HKCU "Software\Chromium\NativeMessagingHosts\com.ndm.host"
  DeleteRegKey HKCU "Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.ndm.host"
  DeleteRegKey HKCU "Software\Microsoft\Edge\NativeMessagingHosts\com.ndm.host"
  DeleteRegKey HKCU "Software\Mozilla\NativeMessagingHosts\com.ndm.host"

  ; Program files only. User data under %APPDATA%\
  ; com.ndm.downloadmanager (settings, queue, native-messaging
  ; manifests) is deliberately left behind, so a reinstall
  ; picks everything back up — the standard per-user app
  ; behaviour.
  ; uninstall file list — same names, native shell
  ; binary; no WebView2Loader.dll delete.
  Delete "$INSTDIR\ndm-app.exe"
  Delete "$INSTDIR\ndm-app.exe.old"
  Delete "$INSTDIR\ndm-host.exe"
  Delete "$INSTDIR\ndm-host.exe.old"
  ; no Delete "$INSTDIR\WebView2Loader.dll" here —
  ; a native install never places that file. (An
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
