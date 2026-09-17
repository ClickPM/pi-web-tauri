; Tauri NSIS installer hooks.
;
; WHY THIS FILE EXISTS
; --------------------
; The Electron build shipped through electron-builder, whose NSIS writes its
; uninstall record under a UUID5 derived from the appId:
;   HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\{0872d550-...}
; Tauri's NSIS uses a different key entirely, so the two installers cannot see
; each other and a user who runs this one ends up with BOTH apps installed,
; both pointing at the same ~/.pi/agent data directory and fighting over it.
;
; So: look for the electron-builder record, and offer to remove it first. The
; user is asked rather than told — silently uninstalling another application is
; not something an installer should decide on its own. A silent install (/S)
; skips the prompt and leaves the old app alone.

!macro NSIS_HOOK_PREINSTALL
  ; electron-builder's key for appId com.agegr.piwebdesktop.
  !define LEGACY_UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\0872d550-066b-5742-b9b4-8eea0f9e1aac"

  SetRegView 64
  ReadRegStr $R0 HKCU "${LEGACY_UNINST_KEY}" "QuietUninstallString"
  StrCmp $R0 "" 0 legacy_found
  ReadRegStr $R0 HKLM "${LEGACY_UNINST_KEY}" "QuietUninstallString"
  StrCmp $R0 "" legacy_done 0

legacy_found:
  ${If} $\{Silent} == 1
    Goto legacy_done
  ${EndIf}
  MessageBox MB_YESNO|MB_ICONQUESTION \
    "检测到旧版 Pi（Electron 版），它会与本版本并排安装并共用同一个数据目录。$\n$\n是否先卸载旧版？（会话与模型凭证保存在 ~/.pi/agent，不会被删除）" \
    /SD IDYES IDNO legacy_done
  DetailPrint "正在卸载旧版 Pi…"
  ExecWait '$R0'

legacy_done:
  !undef LEGACY_UNINST_KEY
!macroend
