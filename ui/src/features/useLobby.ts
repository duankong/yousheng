import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { invoke, isTauri } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type { Action, LobbyEvent, Preferences, Session, Snapshot, VoiceStatus } from '../model'

export function useLobby(openInvite: (link: string) => void) {
  const preferences = ref<Preferences>()
  const session = ref<Session>()
  const snapshot = ref<Snapshot>()
  const ready = ref(false)
  const busy = ref(false)
  const offline = ref('')
  const error = ref('')
  const speakers = ref<string[]>([])
  const level = ref(0)
  const voice = ref<VoiceStatus>({ state: 'disconnected' })
  const active = computed(() => snapshot.value?.channels.find(c => c.id === snapshot.value?.channelId))
  const inVoice = computed(() => ['connected', 'online', 'ready'].includes(voice.value.state))
  const voiceLabel = computed(() => {
    if (inVoice.value) return '语音已连接'
    if (voice.value.state === 'connecting') return '正在连接语音'
    if (voice.value.state === 'reconnecting') return '语音重新连接中'
    if (['error', 'failed'].includes(voice.value.state)) return '语音连接失败'
    return '语音未连接'
  })
  const removers: UnlistenFn[] = []
  let disposed = false
  let offlineVersion = 0

  function clearSession() {
    snapshot.value = undefined
    session.value = undefined
    speakers.value = []
    level.value = 0
    voice.value = { state: 'disconnected' }
  }

  function eventReceived(event: LobbyEvent) {
    if (event.type === 'snapshot') {
      snapshot.value = event
      offline.value = ''
    } else if (event.type === 'offline') {
      offlineVersion++
      clearSession()
      offline.value = event.message || '大厅暂时离线'
    } else { error.value = event.message }
  }

  async function subscribe<T>(name: string, handle: (payload: T) => void) {
    const off = await listen<T>(name, event => handle(event.payload))
    if (disposed) off()
    else removers.push(off)
  }

  onMounted(async () => {
    if (!isTauri()) return
    try {
      let pendingInvite = ''
      await Promise.all([
        subscribe<LobbyEvent>('lobby-event', eventReceived),
        subscribe<VoiceStatus>('voice-status', status => {
          voice.value = status
          if (['error', 'failed', 'disconnected', 'idle'].includes(status.state)) {
            speakers.value = []
            level.value = 0
          }
        }),
        subscribe<{ level: number }>('voice-level', e => { level.value = e.level }),
        subscribe<string[]>('speakers', ids => { speakers.value = ids }),
        subscribe<string>('invite-open', value => { if (ready.value) openInvite(value); else pendingInvite = value }),
      ])
      preferences.value = await invoke<Preferences>('get_preferences')
      const startupInvite = await invoke<string | null>('take_invite')
      ready.value = true
      if (pendingInvite || startupInvite) openInvite(pendingInvite || startupInvite!)
    } catch (reason) { error.value = String(reason) }
  })
  onBeforeUnmount(() => { disposed = true; removers.forEach(off => off()) })

  async function connect(nickname: string, invite?: string) {
    busy.value = true
    error.value = ''
    offline.value = ''
    try {
      if (session.value || snapshot.value) {
        await invoke('leave_lobby')
        clearSession()
      }
      const version = offlineVersion
      const result = invite
        ? await invoke<Session>('join_lobby', { nickname, invite })
        : await invoke<Session>('host_lobby', { nickname })
      if (version !== offlineVersion) return false
      session.value = result
      if (preferences.value) preferences.value.nickname = nickname
      return true
    } catch (reason) { error.value = String(reason); return false }
    finally { busy.value = false }
  }

  async function leave() {
    busy.value = true
    try {
      await invoke('leave_lobby')
      clearSession()
      offline.value = ''
      return true
    } catch (reason) { error.value = String(reason); return false }
    finally { busy.value = false }
  }

  async function action(action: Action) {
    busy.value = true
    error.value = ''
    try { await invoke('lobby_action', { action }); return true }
    catch (reason) { error.value = String(reason); return false }
    finally { busy.value = false }
  }

  return { preferences, session, snapshot, ready, busy, offline, error, speakers, level,
    voice, active, inVoice, voiceLabel, connect, leave, action }
}
