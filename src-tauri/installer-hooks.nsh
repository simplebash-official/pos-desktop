; NSIS installer hooks for MyroLogic POS
; Ensures sidecars (myrologic-backend.exe, myrologic-document-server.exe) and the main application
; are cleanly terminated before files are extracted, preventing Windows file-lock errors.
;
; Also records the installer's own activity into the unified activity log: each step
; appends one JSON line to %APPDATA%\com.myrologic.pos\logs\inbox\installer-<date>.jsonl,
; which the app ingests (and removes) on its next launch. Timestamps are written as
; naive local time; the app adds the UTC offset + timezone on ingest. The same hooks
; run for the in-app updater, which launches this installer.

!include "LogicLib.nsh"
!include "FileFunc.nsh"

!define MYROLOGIC_INBOX "$APPDATA\com.myrologic.pos\logs\inbox"

; MYROLOGIC_LOG <level> <event> <message>
; The line must stay valid JSON: no double quotes or backslashes (so no paths) in arguments.
!macro MYROLOGIC_LOG LEVEL EVENT MESSAGE
  Push $R0
  Push $R1
  Push $R2
  Push $R3
  Push $R4
  Push $R5
  Push $R6
  Push $R7
  ${GetTime} "" "L" $R0 $R1 $R2 $R3 $R4 $R5 $R6
  CreateDirectory "${MYROLOGIC_INBOX}"
  ClearErrors
  FileOpen $R7 "${MYROLOGIC_INBOX}\installer-$R2$R1$R0.jsonl" a
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
!macro MYROLOGIC_STOP IMAGE
  Push $0
  Push $1
  nsExec::ExecToStack 'taskkill /F /IM "${IMAGE}"'
  Pop $0
  Pop $1
  !insertmacro MYROLOGIC_LOG "info" "installer.process_stop" "taskkill ${IMAGE} exit code $0"
  Pop $1
  Pop $0
!macroend

!macro MYROLOGIC_STOP_ALL
  DetailPrint "Stopping MyroLogic POS background services..."
  !insertmacro MYROLOGIC_STOP "myrologic-backend.exe"
  !insertmacro MYROLOGIC_STOP "myrologic-document-server.exe"
  !insertmacro MYROLOGIC_STOP "MyroLogic POS.exe"
  Sleep 500
!macroend

!macro NSIS_HOOK_PREINSTALL
  ${If} ${FileExists} "$APPDATA\com.myrologic.pos\installation.json"
    !insertmacro MYROLOGIC_LOG "info" "installer.update_start" "Installing ${VERSION} over an existing installation"
  ${Else}
    !insertmacro MYROLOGIC_LOG "info" "installer.install_start" "First installation of MyroLogic POS ${VERSION} on this computer"
  ${EndIf}
  !insertmacro MYROLOGIC_STOP_ALL
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro MYROLOGIC_LOG "info" "installer.install_complete" "MyroLogic POS ${VERSION} files installed"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro MYROLOGIC_LOG "info" "installer.uninstall_start" "Uninstalling MyroLogic POS ${VERSION}"
  !insertmacro MYROLOGIC_STOP_ALL
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Only reaches the inbox if the user kept app data (the uninstaller's "delete
  ; app data" option removes the folder before this runs).
  !insertmacro MYROLOGIC_LOG "info" "installer.uninstall_complete" "MyroLogic POS ${VERSION} removed"
!macroend
