; LilyPad's NSIS installer hooks (tauri.conf.json > bundle.windows.nsis.installerHooks).

; After installing, asks whether LilyPad should check for updates automatically, and saves the
; answer where the app reads it: %LOCALAPPDATA%\froglog-lilypad\update-settings.json, as
; {"check_for_updates":true|false} (lilypad-core's updates.rs -- keep the two in step). The app can
; change it later from Configure.
;
; Asked once: skipped when the file already exists (an upgrade keeps the earlier answer) and for
; silent (/S) and passive (/P) installs, which then check for updates by default, the same as a
; missing file.
; The installer runs per user (installMode defaults to currentUser), so $LOCALAPPDATA is the
; user's own folder.
!macro NSIS_HOOK_POSTINSTALL
  ; $R8/$R9 are borrowed inside Tauri's own install section, so they're saved and restored.
  Push $R9
  Push $R8
  IfSilent lilypad_updates_done
  ; Tauri's passive mode (/P: progress bar only, no questions) is unattended too.
  StrCmp $PassiveMode 1 lilypad_updates_done
  IfFileExists "$LOCALAPPDATA\froglog-lilypad\update-settings.json" lilypad_updates_done
  MessageBox MB_YESNO|MB_ICONQUESTION "Automatically check for LilyPad updates?" IDYES lilypad_updates_yes
  StrCpy $R9 "false"
  Goto lilypad_updates_write
  lilypad_updates_yes:
  StrCpy $R9 "true"
  lilypad_updates_write:
  CreateDirectory "$LOCALAPPDATA\froglog-lilypad"
  ClearErrors
  FileOpen $R8 "$LOCALAPPDATA\froglog-lilypad\update-settings.json" w
  IfErrors lilypad_updates_done
  FileWrite $R8 '{"check_for_updates":$R9}'
  FileClose $R8
  lilypad_updates_done:
  Pop $R8
  Pop $R9
!macroend
