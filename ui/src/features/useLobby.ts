import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { invoke, isTauri } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type { Action, Channel, LobbyEvent, Preferences, Session, Snapshot, VoiceStatus } from '../model'

export function useLobby(openInvite: (link: string) => void) {
  const preferences = ref<Preferences>()
  const session = ref<Session>()
  const snapshot = ref<Snapshot>()
  const ready = ref(false)
  const localChannels = ref<Pick<Channel, 'id' | 'name'>[]>([])
  const loadingChannels = ref(false)
  const channelsError = ref('')
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
  let localVersion = 0

  async function refreshLocalChannels() {
    const version = ++localVersion
    loadingChannels.value = true
    channelsError.value = ''
    try {
      const channels = await invoke<Pick<Channel, 'id' | 'name'>[]>('list_local_channels')
      if (version === localVersion) localChannels.value = channels
    } catch (reason) {
      if (version === localVersion) channelsError.value = String(reason)
    } finally {
      if (version === localVersion) loadingChannels.value = false
    }
  }

  watch([snapshot, session], ([state, current]) => {
    if (state && current?.isHost) {
      localVersion++
      localChannels.value = state.channels.map(({ id, name }) => ({ id, name }))
      loadingChannels.value = false
      channelsError.value = ''
    }
  })

  function clearSession() {
    snapshot.value = undefined
    session.value = undefined
    speakers.value = []
    level.value = 0
    voice.value = { state: 'disconnected' }
  }

  function eventReceived(event: LobbyEvent) {
    if (event.type === 'snapshot') {
      const previous = new Map(snapshot.value?.members.map(member => [member.id, member.avatar]))
      for (const member of event.members) {
        if (member.avatar === undefined) member.avatar = previous.get(member.id) || ''
      }
      snapshot.value = event
      offline.value = ''
    } else if (event.type === 'offline') {
      offlineVersion++
      if (session.value?.isHost) void refreshLocalChannels()
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
    void refreshLocalChannels()
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

  async function openSession(invite?: string) {
    const nickname = preferences.value?.nickname.trim()
    if (!nickname) throw new Error('请先设置昵称')
    offline.value = ''
    if (session.value || snapshot.value) {
      const wasHost = session.value?.isHost
      await invoke('leave_lobby')
      clearSession()
      if (wasHost) void refreshLocalChannels()
    }
    const version = offlineVersion
    const result = invite
      ? await invoke<Session>('join_lobby', { nickname, invite })
      : await invoke<Session>('host_lobby', { nickname })
    if (version !== offlineVersion) throw new Error('大厅连接已断开')
    session.value = result
  }

  async function run(operation: () => Promise<void>) {
    if (busy.value) return false
    busy.value = true
    error.value = ''
    try {
      await operation()
      return true
    } catch (reason) { error.value = String(reason); return false }
    finally { busy.value = false }
  }

  function connect(invite?: string) { return run(() => openSession(invite)) }

  function leave() {
    return run(async () => {
      const wasHost = session.value?.isHost
      await invoke('leave_lobby')
      clearSession()
      offline.value = ''
      if (wasHost) void refreshLocalChannels()
    })
  }

  function action(action: Action, startLocal = false) {
    return run(async () => {
      if (startLocal && !session.value && !snapshot.value) await openSession()
      await invoke('lobby_action', { action })
    })
  }

  return { preferences, session, snapshot, ready, busy, offline, error, speakers, level,
    voice, active, inVoice, voiceLabel, connect, leave, action,
    localChannels, loadingChannels, channelsError, refreshLocalChannels }
}
