; Forus installer (NSIS 3, Modern UI 2).
;
; Per-user install: no administrator rights or UAC prompt, like most modern
; Windows apps. Build with:
;   makensis /DVERSION=0.1.0 /DEXE=..\target\release\forus.exe installer\forus.nsi

Unicode true
ManifestDPIAware true
ManifestSupportedOS Win10

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
  !define VERSION "0.1.0"
!endif
!ifndef EXE
  !define EXE "..\target\release\forus.exe"
!endif
!ifndef OUTFILE
  !define OUTFILE "..\target\Forus-${VERSION}-setup.exe"
!endif

!define APP "Forus"
!define WINDOW_CLASS "ForusViewerWindow"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Forus"

Name "${APP}"
OutFile "${OUTFILE}"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\Forus"
InstallDirRegKey HKCU "Software\Forus" "InstallDir"
SetCompressor /SOLID lzma
BrandingText "${APP} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "FileDescription" "${APP} Setup"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT License"

; ---- Pages -------------------------------------------------------------------

!define MUI_ICON "..\assets\forus.ico"
!define MUI_UNICON "..\assets\forus.ico"
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TITLE "Install ${APP} ${VERSION}"
!define MUI_WELCOMEPAGE_TEXT "Forus is a fast, GPU-rendered photo and video viewer for Windows.$\r$\n$\r$\nIt installs for your account only and doesn't need administrator rights.$\r$\n$\r$\nClick Next to continue."
!define MUI_COMPONENTSPAGE_NODESC

; The "show readme" checkbox doubles as "make default".
!define MUI_FINISHPAGE_SHOWREADME
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Make Forus my default photo and video viewer"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION MakeDefault
!define MUI_FINISHPAGE_RUN "$INSTDIR\forus.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Open Forus"
!define MUI_FINISHPAGE_TEXT "Forus is installed. It's in the Start menu and in File Explorer's $\"Open with$\" menu for photos and videos.$\r$\n$\r$\nIf you choose to make it your default viewer, Windows Settings opens; click $\"Set default$\" there to confirm."

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ---- Helpers -----------------------------------------------------------------

; Asks the user to close Forus if it's running (its exe would be locked).
!macro WaitForForusToClose
  ${Do}
    FindWindow $0 "${WINDOW_CLASS}"
    ${If} $0 == 0
      ${Break}
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Forus is running. Close it, then click Retry." /SD IDCANCEL IDRETRY +2
    Abort
  ${Loop}
!macroend

Function MakeDefault
  Exec '"$INSTDIR\forus.exe" --set-default'
FunctionEnd

; ---- Install -----------------------------------------------------------------

Section "Forus" SecMain
  SectionIn RO
  !insertmacro WaitForForusToClose

  SetOutPath "$INSTDIR"
  File "/oname=forus.exe" "${EXE}"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateShortcut "$SMPROGRAMS\Forus.lnk" "$INSTDIR\forus.exe" "" "$INSTDIR\forus.exe" 0 SW_SHOWNORMAL "" "Photo and video viewer"
  WriteRegStr HKCU "Software\Forus" "InstallDir" "$INSTDIR"

  ; Apps & features entry.
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APP}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$\"$INSTDIR\forus.exe$\",0"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "Forus"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" "$\"$INSTDIR\uninstall.exe$\" /S"
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1

  ; File types: "Open with" entries and Default apps capabilities.
  ExecWait '"$INSTDIR\forus.exe" --register'
SectionEnd

Section /o "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\Forus.lnk" "$INSTDIR\forus.exe" "" "$INSTDIR\forus.exe" 0
SectionEnd

; ---- Uninstall ---------------------------------------------------------------

Section "Uninstall"
  !insertmacro WaitForForusToClose

  ExecWait '"$INSTDIR\forus.exe" --unregister'

  Delete "$SMPROGRAMS\Forus.lnk"
  Delete "$DESKTOP\Forus.lnk"
  Delete "$INSTDIR\forus.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKCU "${UNINSTALL_KEY}"
  DeleteRegKey HKCU "Software\Forus"
SectionEnd
