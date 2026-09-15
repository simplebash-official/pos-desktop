; NSIS installer hooks for Jana2U POS
; Ensures sidecars (jana2u-backend.exe, jana2u-document-server.exe) and the main application
; are cleanly terminated before files are extracted, preventing Windows file-lock errors.
;
; Also records the installer's own activity into the unified activity log: each step
; appends one JSON line to %APPDATA%\com.jana2u.pos\logs\inbox\installer-<date>.jsonl,
; which the app ingests (and removes) on its next launch. Timestamps are written as
; naive local time; the app adds the UTC offset + timezone on ingest. The same hooks
; run for the in-app updater, which launches this installer.

!include "LogicLib.nsh"
!include "FileFunc.nsh"

!define JANA2U_INBOX "$APPDATA\com.jana2u.pos\logs\inbox"

; JANA2U_LOG <level> <event> <message>
; The line must stay valid JSON: no double quotes or backslashes (so no paths) in arguments.
!macro JANA2U_LOG LEVEL EVENT MESSAGE
  Push $R0
  Push $R1
  Push $R2
  Push $R3
  Push $R4
  Push $R5
  Push $R6
  Push $R7
  ${GetTime} "" "L" $R0 $R1 $R2 $R3 $R4 $R5 $R6
  CreateDirectory "${JANA2U_INBOX}"
  ClearErrors
  FileOpen $R7 "${JANA2U_INBOX}\installer-$R2$R1$R0.jsonl" a
  ${IfNot} ${Errors}
    FileSeek $R7 0 END
    FileWrite $R7 '{"ts":"$R2-$R1-$R0 $R4:$R5:$R6","level":"${LEVEL}","category":"lifecycle","event":"${EVENT}","msg":"${MESSAGE}","data":{"installer_version":"${VERSION}"}}$\r$\n'
    FileClose $R7
  ${EndIf}
  Pop $R7
  Pop $R6
  Pop $R5
  Pop $R4
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
!macroend

; Kill one process image and log the taskkill exit code (0 = was running, 128 = not running).
!macro JANA2U_STOP IMAGE
  Push $0
  Push $1
  nsExec::ExecToStack 'taskkill /F /IM "${IMAGE}"'
  Pop $0
  Pop $1
  !insertmacro JANA2U_LOG "info" "installer.process_stop" "taskkill ${IMAGE} exit code $0"
  Pop $1
  Pop $0
!macroend

!macro JANA2U_STOP_ALL
  DetailPrint "Stopping Jana2U POS background services..."
  !insertmacro JANA2U_STOP "jana2u-backend.exe"
  !insertmacro JANA2U_STOP "jana2u-document-server.exe"
  !insertmacro JANA2U_STOP "Jana2U POS.exe"
  Sleep 500
!macroend

!macro NSIS_HOOK_PREINSTALL
  ${If} ${FileExists} "$APPDATA\com.jana2u.pos\installation.json"
    !insertmacro JANA2U_LOG "info" "installer.update_start" "Installing ${VERSION} over an existing installation"
  ${Else}
    !insertmacro JANA2U_LOG "info" "installer.install_start" "First installation of Jana2U POS ${VERSION} on this computer"
  ${EndIf}
  !insertmacro JANA2U_STOP_ALL
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro JANA2U_LOG "info" "installer.install_complete" "Jana2U POS ${VERSION} files installed"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro JANA2U_LOG "info" "installer.uninstall_start" "Uninstalling Jana2U POS ${VERSION}"
  !insertmacro JANA2U_STOP_ALL
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Only reaches the inbox if the user kept app data (the uninstaller's "delete
  ; app data" option removes the folder before this runs).
  !insertmacro JANA2U_LOG "info" "installer.uninstall_complete" "Jana2U POS ${VERSION} removed"
!macroend
