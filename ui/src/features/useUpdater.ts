import { computed, reactive, ref, watch, type Ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import type { Preferences } from '../model'

interface Update { version: string; notes: string }

export function useUpdater(preferences: Ref<Preferences | undefined>, ready: Ref<boolean>, installBlocked: Ref<boolean>, sessionId: () => string | null) {
  const enabled = computed(() => preferences.value?.autoUpdate ?? true)
  const saving = ref(false)
  const checking = ref(false)
  const installing = ref(false)
  const available = ref<Update | null>(null)
  const status = ref('')
  const error = ref('')
  let request = 0

  async function check(manual: boolean) {
    if (!ready.value || checking.value || installing.value || (!manual && !enabled.value)) return
    const id = ++request
    checking.value = true
    error.value = ''
    status.value = ''
    try {
      const update = await invoke<Update | null>('check_update', { manual })
      if (request !== id) return
      available.value = update
      status.value = update ? '' : '已是最新版本'
    } catch (reason) {
      if (request === id) error.value = '检查更新失败：' + String(reason)
    } finally {
      if (request === id) checking.value = false
    }
  }

  async function setEnabled(value: boolean) {
    if (!preferences.value || saving.value || installing.value || value === enabled.value) return
    saving.value = true
    error.value = ''
    try {
      await invoke('set_auto_update', { enabled: value })
      preferences.value.autoUpdate = value
      if (value) void check(false)
      else {
        request++
        checking.value = false
        available.value = null
        status.value = ''
      }
    } catch (reason) { error.value = '保存更新设置失败：' + String(reason) }
    finally { saving.value = false }
  }

  async function install() {
    const update = available.value
    if (!update || installBlocked.value || checking.value || installing.value) return
    installing.value = true
    error.value = ''
    try {
      await invoke('install_update', { version: update.version, sessionId: sessionId() })
      status.value = '安装程序已启动'
      available.value = null
    } catch (reason) { error.value = '安装更新失败：' + String(reason) }
    finally { installing.value = false }
  }

  watch(ready, initialized => { if (initialized && enabled.value) void check(false) })
  return reactive({ enabled, saving, checking, installing, installBlocked, available, status, error, setEnabled, check, install })
}

export type Updater = ReturnType<typeof useUpdater>
