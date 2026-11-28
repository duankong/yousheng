import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue'
import { invoke, isTauri } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type { LobbyEvent, Preferences, RecentRoom, Session, Snapshot, SnapshotUpdate, VoiceStatus } from '../model'

interface RoomSession {
  session: Session
  snapshot: Snapshot
}

interface RoomOpening {
  id: string
  snapshot?: Snapshot
  offline?: string
  error?: string
  cancellation?: Promise<boolean>
}

function applySnapshot(update: SnapshotUpdate, previous?: Snapshot): Snapshot | undefined {
  const { messagesAfter, retainedMessageCount, ...snapshot } = update
  let messages = snapshot.messages
  if (messagesAfter !== undefined) {
    if (!previous || previous.messages.at(-1)?.id !== messagesAfter) return
    messages = messages.length
      ? previous.messages.concat(messages).slice(-retainedMessageCount)
      : previous.messages
  }
  const avatars = new Map(previous?.members.map(member => [member.id, member.avatar]))
  const members = snapshot.members.map(member => member.avatar === undefined
    ? { ...member, avatar: avatars.get(member.id) }
    : member)
  return { ...snapshot, members, messages }
}

export function useLobby(openInvite: (link: string) => void, preserveChat?: (room: Snapshot) => void) {
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
  const previewError = ref('')
  const joinStatus = ref<'waiting' | 'cancelling' | 'finishing'>()
  const voiceRetry = shallowRef<{ sessionId: string }>()
  const retryingVoice = computed(() => !!voiceRetry.value && voiceRetry.value.sessionId === session.value?.id)
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
  let pending: RoomOpening | undefined
  let joining: Promise<boolean> | undefined
  let leaving: string | undefined

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
    voiceRetry.value = undefined
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
      if (event.type === 'snapshot') pending.snapshot = applySnapshot(event, pending.snapshot) ?? pending.snapshot
      else if (event.type === 'offline') pending.offline = event.message || '房间连接已断开'
      else pending.error = event.message
      return
    }
    const room = current.value
    if (!room || room.session.id !== event.sessionId) return
    if (event.type === 'snapshot') {
      const next = applySnapshot(event, room.snapshot)
      if (next) current.value = { session: room.session, snapshot: next }
    } else if (event.type === 'offline') {
      if (leaving !== room.session.id) preserveChat?.(room.snapshot)
      disconnected(event.message, room.session.isHost)
    } else error.value = event.message
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
        subscribe<string>('voice-preview-error', message => { previewError.value = message }),
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
    const attempt: RoomOpening = { id: crypto.randomUUID() }
    pending = attempt
    if (invite) joinStatus.value = 'waiting'
    try {
      const args = { sessionId: attempt.id }
      const result = await invoke<Session & { snapshot: Snapshot; historyError?: string }>(
        invite ? 'join_lobby' : 'host_lobby', invite ? { ...args, invite } : args,
      )
      if (attempt.cancellation && await attempt.cancellation) return false
      if (attempt.offline) {
        disconnected(attempt.offline, result.isHost)
        throw new Error(attempt.offline)
      }
      const { id, isHost, invite: acceptedInvite } = result
      voiceRetry.value = undefined
      current.value = { session: { id, isHost, invite: acceptedInvite }, snapshot: attempt.snapshot ?? result.snapshot }
      if (!keepNotice) offline.value = ''
      if (attempt.error) error.value = attempt.error
      if (!isHost) {
        void refreshHistory()
        if (result.historyError) error.value = result.historyError
      }
      return true
    } catch (reason) {
      if (attempt.cancellation && await attempt.cancellation) return false
      throw reason
    } finally {
      pending = undefined
      joinStatus.value = undefined
    }
  }

  async function run(operation: () => Promise<boolean | void>) {
    if (busy.value) return false
    busy.value = true
    error.value = ''
    try {
      return (await operation()) !== false
    } catch (reason) { error.value = reason instanceof Error ? reason.message : String(reason); return false }
    finally {
      busy.value = false
      restoreHost()
    }
  }

  function connect(invite?: string) {
    if (busy.value) return Promise.resolve(false)
    if (!invite) return run(() => {
      manuallyClosed = false
      return openSession()
    })
    const task = run(() => openSession(invite))
    joining = task
    void task.then(() => { if (joining === task) joining = undefined })
    return task
  }

  function requestJoinCancellation() {
    const attempt = pending
    if (!attempt || !joinStatus.value) return Promise.resolve(false)
    if (attempt.cancellation) return attempt.cancellation
    joinStatus.value = 'cancelling'
    attempt.cancellation = invoke<boolean>('cancel_join', { sessionId: attempt.id }).then(cancelled => {
      if (!cancelled) joinStatus.value = 'finishing'
      return cancelled
    }, reason => {
      attempt.cancellation = undefined
      joinStatus.value = 'waiting'
      error.value = String(reason)
      return false
    })
    return attempt.cancellation
  }

  async function cancelJoin() {
    const task = joining
    const cancelled = await requestJoinCancellation()
    if (cancelled) await task
    return cancelled
  }

  async function leave() {
    const room = session.value
    if (!room) return false
    if (joining) {
      const task = joining
      await requestJoinCancellation()
      await task
      if (session.value?.id !== room.id) return false
    }
    return run(async () => {
      leaving = room.id
      try {
        await invoke('leave_lobby', { sessionId: room.id })
        clearSession()
        offline.value = ''
        if (room.isHost) manuallyClosed = true
        else restoreWhenIdle = true
      } finally { leaving = undefined }
    })
  }

  async function retryVoice() {
    const room = session.value
    if (!room || !active.value || busy.value || retryingVoice.value) return
    const request = { sessionId: room.id }
    voiceRetry.value = request
    const currentRequest = () => !disposed && voiceRetry.value === request && session.value?.id === request.sessionId
    error.value = ''
    try {
      await invoke('lobby_action', { sessionId: room.id, action: { type: 'retryVoice' } })
    } catch (reason) {
      if (currentRequest() && !busy.value) error.value = reason instanceof Error ? reason.message : String(reason)
    } finally {
      if (currentRequest()) voiceRetry.value = undefined
    }
  }

  return { preferences, session, snapshot, ready, busy, offline, error, speakers, level, voice, previewError,
    active, inVoice, voiceLabel, connect, joinStatus, cancelJoin, leave, retryVoice, retryingVoice, history, historyLoading, historyError, refreshHistory }
}
