; Stoop: the Windows installer (NSIS 3, Unicode).
;
; Built by packaging/package.sh, which passes:
;   -DVERSION=<X.Y.Z>                       shown in Apps & features, and the file version
;   -DAPP_EXE=<path> -DAPP_NAME=<file name> the desktop app
;   -DCLI_EXE=<path> -DCLI_NAME=<file name> the command-line tool
;   -DREADME=<path> -DICON=<path to stoop.ico> -DOUTFILE=<installer path>
;
; Per user, no administrator rights: Stoop goes to %LOCALAPPDATA%\Stoop or the folder chosen, with a
; Start menu shortcut and an entry in Apps & features. /S installs silently. Your identity, contacts
; and settings live elsewhere and are kept when Stoop is uninstalled.

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef APP_EXE
  !error "pass -DAPP_EXE=<path to the desktop app>"
!endif
!ifndef CLI_EXE
  !error "pass -DCLI_EXE=<path to the command-line tool>"
!endif
!ifndef APP_NAME
  !define APP_NAME "antiphon.exe"
!endif
!ifndef CLI_NAME
  !define CLI_NAME "antiphon-cli.exe"
!endif
!ifndef README
  !error "pass -DREADME=<path to README.md>"
!endif
!ifndef ICON
  !error "pass -DICON=<path to stoop.ico>"
!endif
!ifndef OUTFILE
  !define OUTFILE "Stoop-${VERSION}-windows-x86_64.exe"
!endif

!define NAME "Stoop"
!define UNINSTALLER "uninstall-stoop.exe"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Stoop"
!define SHORTCUT "$SMPROGRAMS\Stoop.lnk"

Name "${NAME}"
OutFile "${OUTFILE}"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Stoop"
; an earlier install in another folder is updated where it is
InstallDirRegKey HKCU "${UNINSTKEY}" "InstallLocation"
BrandingText "Stoop ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${NAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "Stoop installer"
VIAddVersionKey "CompanyName" "Stoop"
VIAddVersionKey "LegalCopyright" "Stoop contributors"

!include "MUI2.nsh"
!include "FileFunc.nsh"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_NAME}"
!define MUI_FINISHPAGE_RUN_TEXT "Start Stoop"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; Wait until no running Stoop holds the program file (up to 10 s), then ask to quit it; a silent
; install closes a copy that is still running instead.
!macro WAIT_FOR_STOOP UN
Function ${UN}WaitForStoop
  StrCpy $0 0
  retry:
    IfFileExists "$INSTDIR\${APP_NAME}" 0 done
    ClearErrors
    Delete "$INSTDIR\${APP_NAME}"
    IfErrors 0 done
    IntOp $0 $0 + 1
    IntCmp $0 20 still 0 still
    Sleep 500
    Goto retry
  still:
    IfSilent kill
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Stoop is still running. Quit Stoop, then press Retry." IDRETRY again
    Abort
  again:
    StrCpy $0 0
    Goto retry
  kill:
    nsExec::Exec 'taskkill /F /IM ${APP_NAME}'
    Pop $1
    Sleep 1000
  done:
FunctionEnd
!macroend
!insertmacro WAIT_FOR_STOOP ""
!insertmacro WAIT_FOR_STOOP "un."

Section "Stoop" SecMain
  SetShellVarContext current
  SetOutPath "$INSTDIR"
  Call WaitForStoop
  File "/oname=${APP_NAME}" "${APP_EXE}"
  File "/oname=${CLI_NAME}" "${CLI_EXE}"
  File "/oname=README.md" "${README}"
  File "/oname=stoop.ico" "${ICON}"
  WriteUninstaller "$INSTDIR\${UNINSTALLER}"

  CreateShortcut "${SHORTCUT}" "$INSTDIR\${APP_NAME}" "" "$INSTDIR\stoop.ico" 0

  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${NAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\stoop.ico"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "Stoop"
  WriteRegStr HKCU "${UNINSTKEY}" "URLInfoAbout" "https://github.com/Krifmaz/Antiphon"
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '"$INSTDIR\${UNINSTALLER}"'
  WriteRegStr HKCU "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\${UNINSTALLER}" /S'
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K /G=0" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" "$0"
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  Call un.WaitForStoop
  Delete "$INSTDIR\${APP_NAME}"
  Delete "$INSTDIR\${CLI_NAME}"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\stoop.ico"
  Delete "$INSTDIR\${UNINSTALLER}"
  RMDir "$INSTDIR"
  Delete "${SHORTCUT}"
  DeleteRegKey HKCU "${UNINSTKEY}"
SectionEnd
