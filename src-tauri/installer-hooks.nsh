; NSIS installer hooks for SimpleBash POS
; Ensures sidecars (simplebash-backend.exe, simplebash-document-server.exe) and the main application
; are cleanly terminated before files are extracted, preventing Windows file-lock errors.
;
; Also records the installer's own activity into the unified activity log: each step
; appends one JSON line to %APPDATA%\com.simplebash.pos\logs\inbox\installer-<date>.jsonl,
; which the app ingests (and removes) on its next launch. Timestamps are written as
; naive local time; the app adds the UTC offset + timezone on ingest. The same hooks
; run for the in-app updater, which launches this installer.

!include "LogicLib.nsh"
!include "FileFunc.nsh"

!define SIMPLEBASH_INBOX "$APPDATA\com.simplebash.pos\logs\inbox"

; SIMPLEBASH_LOG <level> <event> <message>
; The line must stay valid JSON: no double quotes or backslashes (so no paths) in arguments.
!macro SIMPLEBASH_LOG LEVEL EVENT MESSAGE
  Push $R0
  Push $R1
  Push $R2
  Push $R3
  Push $R4
  Push $R5
  Push $R6
  Push $R7
  ${GetTime} "" "L" $R0 $R1 $R2 $R3 $R4 $R5 $R6
  CreateDirectory "${SIMPLEBASH_INBOX}"
  ClearErrors
  FileOpen $R7 "${SIMPLEBASH_INBOX}\installer-$R2$R1$R0.jsonl" a
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
!macro SIMPLEBASH_STOP IMAGE
  Push $0
  Push $1
  nsExec::ExecToStack 'taskkill /F /IM "${IMAGE}"'
  Pop $0
  Pop $1
  !insertmacro SIMPLEBASH_LOG "info" "installer.process_stop" "taskkill ${IMAGE} exit code $0"
  Pop $1
  Pop $0
!macroend

!macro SIMPLEBASH_STOP_ALL
  DetailPrint "Stopping SimpleBash POS background services..."
  !insertmacro SIMPLEBASH_STOP "simplebash-backend.exe"
  !insertmacro SIMPLEBASH_STOP "simplebash-document-server.exe"
  !insertmacro SIMPLEBASH_STOP "SimpleBash POS.exe"
  Sleep 500
!macroend

!macro NSIS_HOOK_PREINSTALL
  ${If} ${FileExists} "$APPDATA\com.simplebash.pos\installation.json"
    !insertmacro SIMPLEBASH_LOG "info" "installer.update_start" "Installing ${VERSION} over an existing installation"
  ${Else}
    !insertmacro SIMPLEBASH_LOG "info" "installer.install_start" "First installation of SimpleBash POS ${VERSION} on this computer"
  ${EndIf}
  !insertmacro SIMPLEBASH_STOP_ALL
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro SIMPLEBASH_LOG "info" "installer.install_complete" "SimpleBash POS ${VERSION} files installed"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro SIMPLEBASH_LOG "info" "installer.uninstall_start" "Uninstalling SimpleBash POS ${VERSION}"
  !insertmacro SIMPLEBASH_STOP_ALL
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Only reaches the inbox if the user kept app data (the uninstaller's "delete
  ; app data" option removes the folder before this runs).
  !insertmacro SIMPLEBASH_LOG "info" "installer.uninstall_complete" "SimpleBash POS ${VERSION} removed"
!macroend
