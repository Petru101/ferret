; Ferret's installer (makensis, from data/windows/package.sh: -DVERSION, -DSRC the package
; folder, -DOUT the installer). Installs for the current user into
; %LOCALAPPDATA%\Programs\Ferret (no administrator), with a Start menu shortcut and an
; uninstaller in Settings' installed apps. Ferret's own data (%LOCALAPPDATA%\Ferret: profiles,
; logs) is left in place on uninstall.
Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user
!define KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Ferret"

Name "Ferret"
Caption "Ferret ${VERSION}: find and change values in your games"
OutFile "${OUT}"
InstallDir "$LOCALAPPDATA\Programs\Ferret"
InstallDirRegKey HKCU "${KEY}" "InstallLocation"

!include "MUI2.nsh"
!define MUI_ICON "${SRC}\ferret.ico"
!define MUI_UNICON "${SRC}\ferret.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\bin\ferret.exe"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; Ferret running from the folder: asked to close it (its helper may be tracing a game: ending it
; from here could leave the game's breakpoints on). Then GLib's session bus it started, which
; keeps running for the sign-in session and would lock gdbus.exe.
!macro CloseFerret
  retry:
  nsExec::ExecToStack `powershell -NoProfile -Command "if (Get-Process ferret,ferret-helper -ErrorAction SilentlyContinue | Where-Object { $$_.Path -like '$INSTDIR\*' }) { 'running' }"`
  Pop $0
  Pop $1
  StrCmp $1 "" done
  MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Ferret is running. Close it, then choose Retry." /SD IDCANCEL IDRETRY retry
  Abort
  done:
  nsExec::Exec `powershell -NoProfile -Command "Get-Process gdbus -ErrorAction SilentlyContinue | Where-Object { $$_.Path -like '$INSTDIR\*' } | Stop-Process -Force"`
!macroend

Section "Ferret"
  !insertmacro CloseFerret
  RMDir /r "$INSTDIR\bin"
  RMDir /r "$INSTDIR\lib"
  RMDir /r "$INSTDIR\share"
  SetOutPath "$INSTDIR"
  File /r "${SRC}\*.*"
  CreateShortcut "$SMPROGRAMS\Ferret.lnk" "$INSTDIR\bin\ferret.exe" "" "$INSTDIR\ferret.ico"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${KEY}" "DisplayName" "Ferret"
  WriteRegStr HKCU "${KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${KEY}" "Publisher" "Petru Mare"
  WriteRegStr HKCU "${KEY}" "Comments" "Find and change values in your games"
  WriteRegStr HKCU "${KEY}" "URLInfoAbout" "https://github.com/Petru101/ferret"
  WriteRegStr HKCU "${KEY}" "DisplayIcon" "$INSTDIR\ferret.ico"
  WriteRegStr HKCU "${KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  !insertmacro CloseFerret
  Delete "$SMPROGRAMS\Ferret.lnk"
  RMDir /r "$INSTDIR\bin"
  RMDir /r "$INSTDIR\lib"
  RMDir /r "$INSTDIR\share"
  Delete "$INSTDIR\ferret.ico"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "${KEY}"
SectionEnd
