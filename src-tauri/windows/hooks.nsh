; Atlas' own steps in the Windows uninstaller (tauri.conf.json: bundle.windows.nsis.installerHooks).

!macro NSIS_HOOK_PREUNINSTALL
  ; Settings > Data > Uninstall Atlas passes /DELETEDATA when "Also delete my Atlas data" is ticked:
  ; the same as ticking "Delete the application data" in the uninstaller's own window.
  ClearErrors
  ${GetOptions} $CMDLINE "/DELETEDATA" $R0
  ${IfNot} ${Errors}
    StrCpy $DeleteAppDataCheckboxState 1
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; With the data, also the Rhythia sign-in in the Windows password store (secret.rs); never on an update.
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    nsExec::Exec '"$SYSDIR\cmdkey.exe" /delete:AtlasForRhythia/rhythia-session'
    Pop $0
  ${EndIf}
!macroend
