; Everything the app needs beyond its own files.
;
; Tauri's NSIS package installs the binaries and a shortcut, and stops there.
; That is not enough to have a *working* MDM: the browser reaches the app
; through a native messaging host, and Firefox finds that host only through a
; manifest named by a registry value. Without the three things below, a machine
; that ran the installer gets a download manager the browser cannot talk to —
; which looks exactly like the app being broken.
;
; This is the middle of install.ps1, expressed for the installer, so that the
; script and the .exe leave a machine in the same state:
;
;   1. the native messaging manifest, and the registry value pointing at it
;   2. the mdm:// URI scheme
;   3. the signed extension, put somewhere the user can install it from
;
; All per-user (HKCU, %APPDATA%), so the installer never needs elevation.

!include "WordFunc.nsh"

!define MDM_HOST_NAME "io.mdm.host"
!define MDM_EXT_ID    "mdm@ramlej.local"
; Chromium derives an extension id from its public key, so the key pinned in
; manifest.chrome.json pins this. It has to be known here because the native
; messaging manifest names who may connect before anything is installed.
!define MDM_CHROME_ID "pegdlonllkokelfmdafooihklghlkimh"

!macro NSIS_HOOK_POSTINSTALL
  ; Roaming %APPDATA%, matching what paths.rs resolves at runtime.
  SetShellVarContext current

  CreateDirectory "$APPDATA\mdm"

  ; JSON requires backslashes to be doubled, and $INSTDIR is full of them.
  ; Writing the path raw produces a manifest Firefox parses as invalid and
  ; ignores, which fails silently — the host simply never launches.
  ${WordReplace} "$INSTDIR\mdm-host.exe" "\" "\\" "+" $R0

  ClearErrors
  FileOpen $R1 "$APPDATA\mdm\${MDM_HOST_NAME}.json" w
  IfErrors mdm_manifest_failed
  FileWrite $R1 '{$\r$\n'
  FileWrite $R1 '  "name": "${MDM_HOST_NAME}",$\r$\n'
  FileWrite $R1 '  "description": "My Download Manager native host",$\r$\n'
  FileWrite $R1 '  "path": "$R0",$\r$\n'
  FileWrite $R1 '  "type": "stdio",$\r$\n'
  FileWrite $R1 '  "allowed_extensions": ["${MDM_EXT_ID}"]$\r$\n'
  FileWrite $R1 '}$\r$\n'
  FileClose $R1

  ; Firefox on Windows does not search directories for manifests the way it
  ; does on Linux; this value is what "registering" actually means.
  WriteRegStr HKCU "Software\Mozilla\NativeMessagingHosts\${MDM_HOST_NAME}" "" \
    "$APPDATA\mdm\${MDM_HOST_NAME}.json"

mdm_manifest_failed:

  ; The mdm:// scheme, so a link built as mdm://https://… launches the app.
  WriteRegStr HKCU "Software\Classes\mdm" "" "URL:MDM Protocol"
  WriteRegStr HKCU "Software\Classes\mdm" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\mdm\shell\open\command" "" \
    '"$INSTDIR\mdm.exe" "%1"'

  ; The extension, copied out of the install directory to a stable place:
  ; %APPDATA%\mdm\extension, which is where install.ps1 puts it, and the same
  ; directory relative to MDM's own that install.sh and the app use on Linux.
  ; One path to write down, whichever way somebody installed.
  ;
  ; Firefox will not install an add-on from an installer, so the .xpi is put
  ; where the user can open it — the finish page says how.
  CreateDirectory "$APPDATA\mdm\extension"
  IfFileExists "$INSTDIR\extension\mdm-firefox.xpi" 0 +2
    CopyFiles /SILENT "$INSTDIR\extension\mdm-firefox.xpi" "$APPDATA\mdm\extension\mdm-firefox.xpi"

  ; The flat layout an installer before this one left behind. The .xpi is only
  ; litter — Firefox copied the add-on into the profile as it installed it —
  ; but the folder is not: Chromium re-reads an unpacked extension's folder at
  ; every start, so a browser pointed at the old one would keep running it,
  ; and a version that is never updated again has no symptom beyond fixes that
  ; appear to do nothing.
  Delete "$APPDATA\mdm\mdm-firefox.xpi"
  RMDir /r "$APPDATA\mdm\mdm-chrome"

  ; ---------------------------------------------------------------- Chromium
  ;
  ; A second manifest, because the key naming who may connect is
  ; `allowed_origins` with a chrome-extension:// URL where Firefox's is
  ; `allowed_extensions` with a bare id. Same host binary, same protocol.
  ClearErrors
  FileOpen $R2 "$APPDATA\mdm\${MDM_HOST_NAME}.chrome.json" w
  IfErrors mdm_chrome_manifest_failed
  FileWrite $R2 '{$\r$\n'
  FileWrite $R2 '  "name": "${MDM_HOST_NAME}",$\r$\n'
  FileWrite $R2 '  "description": "My Download Manager native host",$\r$\n'
  FileWrite $R2 '  "path": "$R0",$\r$\n'
  FileWrite $R2 '  "type": "stdio",$\r$\n'
  FileWrite $R2 '  "allowed_origins": ["chrome-extension://${MDM_CHROME_ID}/"]$\r$\n'
  FileWrite $R2 '}$\r$\n'
  FileClose $R2

  ; One value per browser family: each reads only its own hive, and a value
  ; written for a browser that is not installed is inert.
  WriteRegStr HKCU "Software\Google\Chrome\NativeMessagingHosts\${MDM_HOST_NAME}" "" \
    "$APPDATA\mdm\${MDM_HOST_NAME}.chrome.json"
  WriteRegStr HKCU "Software\Microsoft\Edge\NativeMessagingHosts\${MDM_HOST_NAME}" "" \
    "$APPDATA\mdm\${MDM_HOST_NAME}.chrome.json"
  WriteRegStr HKCU "Software\Chromium\NativeMessagingHosts\${MDM_HOST_NAME}" "" \
    "$APPDATA\mdm\${MDM_HOST_NAME}.chrome.json"
  WriteRegStr HKCU "Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\${MDM_HOST_NAME}" "" \
    "$APPDATA\mdm\${MDM_HOST_NAME}.chrome.json"
  WriteRegStr HKCU "Software\Vivaldi\NativeMessagingHosts\${MDM_HOST_NAME}" "" \
    "$APPDATA\mdm\${MDM_HOST_NAME}.chrome.json"

mdm_chrome_manifest_failed:

  ; Chromium loads an *unpacked folder*, not a file, so the extension is put
  ; somewhere stable and the user points "Load unpacked" at it. It keeps its
  ; pinned id, which is the one the manifest above just allowed.
  IfFileExists "$INSTDIR\extension\mdm-chrome\manifest.json" 0 +3
    CreateDirectory "$APPDATA\mdm\extension\mdm-chrome"
    CopyFiles /SILENT "$INSTDIR\extension\mdm-chrome\*.*" "$APPDATA\mdm\extension\mdm-chrome"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  SetShellVarContext current

  DeleteRegKey HKCU "Software\Mozilla\NativeMessagingHosts\${MDM_HOST_NAME}"
  DeleteRegKey HKCU "Software\Google\Chrome\NativeMessagingHosts\${MDM_HOST_NAME}"
  DeleteRegKey HKCU "Software\Microsoft\Edge\NativeMessagingHosts\${MDM_HOST_NAME}"
  DeleteRegKey HKCU "Software\Chromium\NativeMessagingHosts\${MDM_HOST_NAME}"
  DeleteRegKey HKCU "Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\${MDM_HOST_NAME}"
  DeleteRegKey HKCU "Software\Vivaldi\NativeMessagingHosts\${MDM_HOST_NAME}"
  DeleteRegKey HKCU "Software\Classes\mdm"

  Delete "$APPDATA\mdm\${MDM_HOST_NAME}.json"
  Delete "$APPDATA\mdm\${MDM_HOST_NAME}.chrome.json"
  RMDir /r "$APPDATA\mdm\extension"
  ; And the flat layout, for a machine that has had both.
  Delete "$APPDATA\mdm\mdm-firefox.xpi"
  RMDir /r "$APPDATA\mdm\mdm-chrome"

  ; The database and settings are deliberately left. An uninstall is not a
  ; request to lose a download history, and a reinstall that finds its library
  ; intact is the better surprise of the two.
!macroend
