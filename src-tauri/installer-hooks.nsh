!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr SHCTX "${UNINSTKEY}" "DisplayName" "${PRODUCTNAME} GroveCall"
!macroend
