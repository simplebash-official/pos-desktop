; NSIS installer hooks for Jana2U POS
; Ensures sidecars (jana2u-backend.exe, jana2u-document-server.exe) and the main application
; are cleanly terminated before files are extracted, preventing Windows file-lock errors.

!macro NSIS_HOOK_PREINSTALL
  DetailPrint "Stopping Jana2U POS background services..."
  nsExec::Exec 'taskkill /F /IM "jana2u-backend.exe"'
  nsExec::Exec 'taskkill /F /IM "jana2u-document-server.exe"'
  nsExec::Exec 'taskkill /F /IM "Jana2U POS.exe"'
  Sleep 500
!macroend

!macro NSIS_HOOK_POSTINSTALL
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Stopping Jana2U POS background services..."
  nsExec::Exec 'taskkill /F /IM "jana2u-backend.exe"'
  nsExec::Exec 'taskkill /F /IM "jana2u-document-server.exe"'
  nsExec::Exec 'taskkill /F /IM "Jana2U POS.exe"'
  Sleep 500
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
