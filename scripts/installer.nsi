; Nill Download Manager — per-user NSIS installer.
;
; Packages the assembled portable tree (the exact folder the
; portable ZIP wraps) so the installed layout is identical to the
; portable one, which is what the app's tool discovery expects:
;
;   <InstallDir>\ndm-app.exe
;   <InstallDir>\ndm-host.exe          (beside the app)
;   <InstallDir>\components\*.exe      (yt-dlp, deno, ffmpeg)
;   <InstallDir>\extension\*           (browser extension)
;   <InstallDir>\installed.flag        (install-flavour marker —
;                                       the app's self-update reads
;                                       this exact file name)
;   <InstallDir>\uninstall.exe
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

!ifndef VERSION
  !define VERSION "2.0.0"
!endif
!ifndef SRCROOT
  !define SRCROOT "NDM-Windows-Portable"
!endif
!ifndef OUTFILE
  !define OUTFILE "NDM_${VERSION}_x64-setup.exe"
!endif
!define ICON "${__FILEDIR__}\..\src-tauri\icons\icon.ico"
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

  SetOutPath "$INSTDIR\components"
  File /r "${SRCROOT}\components\*.*"
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
  Delete "$INSTDIR\installed.flag"
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
