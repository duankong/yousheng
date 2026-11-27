import { computed, reactive, ref, shallowRef, type Ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import type { AudioSettings, PeerSettings, Preferences } from '../model'

export const audioGainLimits = { inputGain: 200, outputGain: 150 } as const
type AudioGain = keyof typeof audioGainLimits
const defaultPeer: PeerSettings = { volume: 100, muted: false }

export function useAudioControls(preferences: Ref<Preferences | undefined>) {
  const current = computed(() => preferences.value?.audio)
  const pending = shallowRef<Partial<AudioSettings>>({})
  const applying = shallowRef<Partial<AudioSettings>>({})
  const effective = computed(() => {
    const audio = current.value
    if (audio) return { ...audio, ...applying.value, ...pending.value }
  })
  const error = ref('')
  const peerChanges = reactive(new Map<string, PeerSettings>())
  let timer: ReturnType<typeof setTimeout> | undefined
  let writing: Promise<boolean> | undefined
  let speakerGainBefore: number | undefined
  let speakerRevision = 0

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
    const audio = effective.value
    if (!audio || audio[key] === value) return
    if (key === 'outputGain') {
      speakerGainBefore ??= audio.outputGain
      ++speakerRevision
    }
    pending.value = { ...pending.value, [key]: value }
    error.value = ''
    // A sustained drag previews periodically. Pointer release and navigation flush immediately.
    if (!dragging) void flush()
    else if (!timer) timer = setTimeout(flush, 120)
  }

  async function finishGain(key: AudioGain) {
    if (key === 'inputGain') {
      await flush()
      return
    }
    const before = speakerGainBefore
    speakerGainBefore = undefined
    const revision = speakerRevision
    const audio = effective.value
    const saved = await flush()
    if (!saved || !audio || before === undefined || audio.outputGain === before ||
        audio.outputGain === 0 || audio.outputMuted || revision !== speakerRevision ||
        effective.value?.outputMuted || current.value?.outputGain !== audio.outputGain) return
    try {
      await invoke('preview_speaker_volume', { gain: audio.outputGain })
    } catch (reason) {
      if (revision === speakerRevision) error.value = '扬声器试听失败：' + String(reason)
    }
  }

  function peerSettings(id: string) {
    return peerChanges.get(id) ?? preferences.value?.peers[id] ?? defaultPeer
  }

  function peerSaving(id: string) {
    return peerChanges.has(id)
  }

  async function savePeer(id: string, settings: PeerSettings) {
    if (!preferences.value || peerSaving(id)) return
    peerChanges.set(id, settings)
    try {
      await invoke('set_peer', { id, ...settings })
      if (preferences.value) preferences.value.peers[id] = settings
    } finally { peerChanges.delete(id) }
  }

  return reactive({ current, effective, error, change, flush, finishGain, peerSettings, peerSaving, savePeer })
}

export type AudioControls = ReturnType<typeof useAudioControls>
