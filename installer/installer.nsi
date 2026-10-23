; The Utility Closet installer (NSIS 3.x)
; Requires administrator (the app itself needs elevation for SFC/registry work).
; Installs to %PROGRAMFILES%\UtilityCloset.

!define APP_NAME "The Utility Closet"
!define APP_VERSION "1.0.4"
!define APP_PUBLISHER "Shawn Rodis"
!define APP_URL "https://github.com/shawnrodis/UtilityCloset"

Name "${APP_NAME} ${APP_VERSION}"
OutFile "UtilityCloset-Setup-v${APP_VERSION}.exe"
InstallDir "$PROGRAMFILES\UtilityCloset"
RequestExecutionLevel admin
ShowInstDetails show

!include "MUI2.nsh"
!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\UtilityCloset.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch The Utility Closet"
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "The Utility Closet" SecApp
  SectionIn RO  ; required — cannot be unchecked
  SetOutPath "$INSTDIR"
  File "UtilityCloset.exe"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; Add/Remove Programs entry (machine-wide, needs admin).
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\UtilityCloset" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\UtilityCloset" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\UtilityCloset" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\UtilityCloset" "URLInfoAbout" "${APP_URL}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\UtilityCloset" "UninstallString" "$\"$INSTDIR\Uninstall.exe$\""
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\UtilityCloset" "DisplayIcon" "$INSTDIR\UtilityCloset.exe"
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\UtilityCloset" "EstimatedSize" 16448

  ; Start Menu shortcuts (all users).
  CreateDirectory "$SMPROGRAMS\The Utility Closet"
  CreateShortcut "$SMPROGRAMS\The Utility Closet\The Utility Closet.lnk" "$INSTDIR\UtilityCloset.exe"
  CreateShortcut "$SMPROGRAMS\The Utility Closet\Uninstall.lnk" "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Desktop Icon" SecDesktop
  CreateShortcut "$DESKTOP\The Utility Closet.lnk" "$INSTDIR\UtilityCloset.exe"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} "Installs the application and adds Start Menu shortcuts."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Adds a Utility Closet icon to your desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  Delete "$INSTDIR\UtilityCloset.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\UtilityCloset"

  Delete "$SMPROGRAMS\The Utility Closet\The Utility Closet.lnk"
  Delete "$SMPROGRAMS\The Utility Closet\Uninstall.lnk"
  RMDir "$SMPROGRAMS\The Utility Closet"

  Delete "$DESKTOP\The Utility Closet.lnk"
SectionEnd
