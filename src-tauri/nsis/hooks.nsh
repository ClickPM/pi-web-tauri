; Tauri NSIS installer hooks.
;
; WHY THIS FILE EXISTS
; --------------------
; The Electron build shipped through electron-builder, whose NSIS writes its
; uninstall record under a UUID5 derived from the appId:
;   HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\{0872d550-...}
; Tauri's NSIS uses a different key entirely, so the two installers cannot see
; each other. Without this hook a user who runs this installer ends up with BOTH
; apps installed, both pointing at the same ~/.pi/agent data directory and both
; trying to run a pi-web server against it.
;
; So: look for the electron-builder record and OFFER to remove it. The user is
; asked rather than told — silently uninstalling another application is not a
; decision an installer should make on its own. A silent install (/S) skips the
; prompt entirely and leaves the old app alone.
;
; SetRegView 64 matters: NSIS itself is a 32-bit process, so without it the
; reads land in Software\WOW6432Node\... and never find electron-builder's key.

!macro NSIS_HOOK_PREINSTALL
  SetRegView 64

  ; Per-user install first (electron-builder ran with perMachine: false), then
  ; the machine-wide location in case a build was ever published that way.
  ReadRegStr $R0 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\0872d550-066b-5742-b9b4-8eea0f9e1aac" "QuietUninstallString"
  StrCmp $R0 "" 0 piwd_legacy_found
  ReadRegStr $R0 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\0872d550-066b-5742-b9b4-8eea0f9e1aac" "QuietUninstallString"
  StrCmp $R0 "" piwd_legacy_done 0

piwd_legacy_found:
  ; Never prompt during a silent install, and never uninstall behind the user's
  ; back either — a silent install simply leaves the old app in place.
  IfSilent piwd_legacy_done

  MessageBox MB_YESNO|MB_ICONQUESTION \
    "检测到旧版 Pi（Electron 版）。$\n$\n它与本版本会并排安装，并且会争用同一个数据目录，建议先卸载。$\n$\n是否现在卸载旧版？（会话与模型凭证保存在 ~/.pi/agent，不会被删除）" \
    /SD IDNO IDNO piwd_legacy_done

  DetailPrint "正在卸载旧版 Pi…"
  ExecWait '$R0'

piwd_legacy_done:
  SetRegView Default
!macroend
