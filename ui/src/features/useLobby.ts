import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue'
import { invoke, isTauri } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type { LobbyEvent, Preferences, RecentRoom, Session, Snapshot, VoiceStatus } from '../model'

interface RoomSession {
  session: Session
  snapshot: Snapshot
}

function withAvatars(snapshot: Snapshot, previous?: Snapshot): Snapshot {
  const avatars = new Map(previous?.members.map(member => [member.id, member.avatar]))
  const members = snapshot.members.map(member => member.avatar === undefined
    ? { ...member, avatar: avatars.get(member.id) }
    : member)
  return { ...snapshot, members }
}

export function useLobby(openInvite: (link: string) => void) {
  const preferences = ref<Preferences>()
  const current = shallowRef<RoomSession>()
  const session = computed(() => current.value?.session)
  const snapshot = computed(() => current.value?.snapshot)
  const active = computed(() => !!current.value && (!current.value.session.isHost || current.value.snapshot.members.length > 1))
  const ready = ref(false)
  const history = ref<RecentRoom[]>([])
  const historyLoading = ref(false)
  const historyError = ref('')
  const busy = ref(false)
  const offline = ref('')
  const error = ref('')
  const speakers = ref<string[]>([])
  const level = ref(0)
  const voice = ref<VoiceStatus>({ state: 'disconnected' })
  const inVoice = computed(() => ['connected', 'online', 'ready'].includes(voice.value.state))
  const voiceLabel = computed(() => {
    if (inVoice.value) return '语音已连接'
    if (voice.value.state === 'connecting') return '正在连接语音'
    if (voice.value.state === 'reconnecting') return '语音重新连接中'
    if (voice.value.state === 'device-error') return '音频设备不可用'
    if (['error', 'failed'].includes(voice.value.state)) return '语音连接失败'
    return '语音未连接'
  })
  const removers: UnlistenFn[] = []
  let disposed = false
  let historyVersion = 0
  let manuallyClosed = false
  let restoreWhenIdle = false
  let pending: { id: string; snapshot?: Snapshot; offline?: string; error?: string } | undefined

  async function refreshHistory() {
    const version = ++historyVersion
    historyLoading.value = true
    historyError.value = ''
    try {
      const rooms = await invoke<RecentRoom[]>('list_room_history')
      if (version === historyVersion) history.value = rooms
    } catch (reason) {
      if (version === historyVersion) historyError.value = String(reason)
    } finally {
      if (version === historyVersion) historyLoading.value = false
    }
  }

  function clearSession() {
    current.value = undefined
    speakers.value = []
    level.value = 0
    voice.value = { state: 'disconnected' }
  }

  function restoreHost() {
    if (busy.value || !restoreWhenIdle) return
    restoreWhenIdle = false
    if (!manuallyClosed && !current.value) void run(() => openSession(undefined, true))
  }

  function disconnected(message: string, isHost: boolean) {
    clearSession()
    offline.value = message || '房间连接已断开'
    if (!isHost) {
      restoreWhenIdle = true
      restoreHost()
    }
  }

  function eventReceived(event: LobbyEvent) {
    if (pending?.id === event.sessionId) {
      if (event.type === 'snapshot') pending.snapshot = withAvatars(event, pending.snapshot)
      else if (event.type === 'offline') pending.offline = event.message || '房间连接已断开'
      else pending.error = event.message
      return
    }
    const room = current.value
    if (!room || room.session.id !== event.sessionId) return
    if (event.type === 'snapshot') current.value = { session: room.session, snapshot: withAvatars(event, room.snapshot) }
    else if (event.type === 'offline') disconnected(event.message, room.session.isHost)
    else error.value = event.message
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
          if (['device-error', 'error', 'failed', 'disconnected', 'idle'].includes(status.state)) {
            speakers.value = []
            level.value = 0
          }
        }),
        subscribe<{ level: number }>('voice-level', e => { level.value = e.level }),
        subscribe<string[]>('speakers', ids => { speakers.value = ids }),
        subscribe<string>('invite-open', link => { if (ready.value) openInvite(link); else pendingInvite = link }),
      ])
      preferences.value = await invoke<Preferences>('get_preferences')
      const startupInvite = await invoke<string | null>('take_invite')
      ready.value = true
      void refreshHistory()
      if (preferences.value.nickname.trim()) void connect()
      const link = pendingInvite || startupInvite
      if (link) openInvite(link)
    } catch (reason) { error.value = String(reason) }
  })
  onBeforeUnmount(() => { disposed = true; removers.forEach(off => off()) })

  async function openSession(invite?: string, keepNotice = false) {
    if (!preferences.value?.nickname.trim()) throw new Error('请先设置昵称')
    const attempt = { id: crypto.randomUUID() } as NonNullable<typeof pending>
    pending = attempt
    try {
      const result = invite
        ? await invoke<Session & { snapshot: Snapshot; historyError?: string }>('join_lobby', { sessionId: attempt.id, invite })
        : await invoke<Session & { snapshot: Snapshot }>('host_lobby', { sessionId: attempt.id })
      if (attempt.offline) {
        disconnected(attempt.offline, result.isHost)
        throw new Error(attempt.offline)
      }
      const { id, isHost, invite: acceptedInvite } = result
      current.value = { session: { id, isHost, invite: acceptedInvite }, snapshot: attempt.snapshot ?? result.snapshot }
      if (!keepNotice) offline.value = ''
      if (attempt.error) error.value = attempt.error
      if (!isHost) {
        void refreshHistory()
        if ('historyError' in result && typeof result.historyError === 'string') error.value = result.historyError
      }
    } finally { pending = undefined }
  }

  async function run(operation: () => Promise<void>) {
    if (busy.value) return false
    busy.value = true
    error.value = ''
    try {
      await operation()
      return true
    } catch (reason) { error.value = reason instanceof Error ? reason.message : String(reason); return false }
    finally {
      busy.value = false
      restoreHost()
    }
  }

  function connect(invite?: string) {
    return run(async () => {
      if (!invite) manuallyClosed = false
      await openSession(invite)
    })
  }

  function leave() {
    const room = session.value
    if (!room) return Promise.resolve(false)
    return run(async () => {
      await invoke('leave_lobby', { sessionId: room.id })
      clearSession()
      offline.value = ''
      if (room.isHost) manuallyClosed = true
      else restoreWhenIdle = true
    })
  }

  function retryVoice() {
    return run(async () => {
      const room = session.value
      if (room && active.value) await invoke('lobby_action', { sessionId: room.id, action: { type: 'retryVoice' } })
    })
  }

  return { preferences, session, snapshot, ready, busy, offline, error, speakers, level, voice,
    active, inVoice, voiceLabel, connect, leave, retryVoice, history, historyLoading, historyError, refreshHistory }
}
