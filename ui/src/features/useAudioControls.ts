import { computed, reactive, ref, shallowRef, type Ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import type { AudioSettings, Preferences } from '../model'

export function useAudioControls(preferences: Ref<Preferences | undefined>) {
  const pending = shallowRef<Partial<AudioSettings>>({})
  const applying = shallowRef<Partial<AudioSettings>>({})
  const changes = computed(() => ({ ...applying.value, ...pending.value }))
  const error = ref('')
  let timer: ReturnType<typeof setTimeout> | undefined
  let writing: Promise<boolean> | undefined

  async function drain() {
    let success = true
    try {
      while (!timer && Object.keys(pending.value).length) {
        const patch = pending.value
        pending.value = {}
        applying.value = patch
        try {
          const prefs = preferences.value
          if (!prefs) throw new Error('个人设置尚未就绪')
          const audio = { ...prefs.audio, ...patch }
          await invoke('set_audio', { audio })
          if (preferences.value) {
            preferences.value.audio = { ...preferences.value.audio, ...patch }
            preferences.value.shortcutError = null
          }
        } catch (reason) {
          error.value = '声音设置未应用：' + String(reason)
          success = false
        } finally { applying.value = {} }
      }
      return success
    } finally { writing = undefined }
  }

  function flush(): Promise<boolean> {
    clearTimeout(timer)
    timer = undefined
    if (writing) return writing
    if (!Object.keys(pending.value).length) return Promise.resolve(true)
    writing = Promise.resolve().then(drain)
    return writing
  }

  function change<K extends keyof AudioSettings>(key: K, value: AudioSettings[K], dragging = false) {
    const current = { ...preferences.value?.audio, ...changes.value }
    if (current[key] === value) return
    pending.value = { ...pending.value, [key]: value }
    error.value = ''
    // A sustained drag previews periodically. Pointer release and navigation flush immediately.
    if (!dragging) void flush()
    else if (!timer) timer = setTimeout(flush, 120)
  }

  return reactive({ changes, error, change, flush })
}

export type AudioControls = ReturnType<typeof useAudioControls>
