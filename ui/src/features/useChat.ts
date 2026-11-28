import { reactive, ref, watch, type Ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import type { Session, Snapshot } from '../model'

export interface ChatAttempt { id: number; original: string; error: string }
interface RecoveryEntry { id: number; original: string; roomName: string; kind: 'draft' | 'unconfirmed' }
interface ChatReceipt { id: number; elapsedMs?: number; confirmed?: boolean }

export function chatSendHint(attempts: ChatAttempt[], text: string): string {
  const duplicate = attempts.find(item => item.original.trim() === text.trim())
  if (duplicate) return duplicate.error ? '相同内容尚未确认，请先核对记录' : '相同内容正在等待房主确认'
  if (attempts.filter(item => !item.error).length >= 8) return '已有 8 条消息等待确认，稍后可继续发送'
  return ''
}

export function useChat(
  session: Readonly<Ref<Session | undefined>>,
  snapshot: Readonly<Ref<Snapshot | undefined>>,
  blocked: () => boolean,
  error: Ref<string>,
) {
  const draft = ref('')
  const outbox = ref<ChatAttempt[]>([])
  const recovery = ref<RecoveryEntry[]>([])
  const lastChatReceipt = ref<ChatReceipt>()
  let nextId = 0

  watch(() => session.value?.id, () => {
    lastChatReceipt.value = undefined
    draft.value = ''
    outbox.value = []
  }, { flush: 'sync' })

  function preserve(room: Snapshot) {
    if (draft.value) recovery.value.push({
      id: ++nextId, original: draft.value, roomName: room.roomName, kind: 'draft',
    })
    for (const { id, original } of outbox.value) {
      recovery.value.push({ id, original, roomName: room.roomName, kind: 'unconfirmed' })
    }
  }

  function removeAttempt(id: number) {
    outbox.value = outbox.value.filter(item => item.id !== id)
  }

  function removeRecovery(id: number) {
    recovery.value = recovery.value.filter(item => item.id !== id)
  }

  async function sendChat() {
    const room = session.value
    const text = draft.value.trim()
    const roomId = snapshot.value?.roomId
    if (!room || !roomId || blocked() || !text || chatSendHint(outbox.value, text)) return
    if (Array.from(text).length > 1000) {
      error.value = '每条消息最多可发送 1000 个字符。'
      return
    }
    const attempt = reactive<ChatAttempt>({ id: ++nextId, original: draft.value, error: '' })
    const started = performance.now()
    lastChatReceipt.value = { id: attempt.id }
    outbox.value.push(attempt)
    draft.value = ''
    error.value = ''
    try {
      await invoke('lobby_action', { sessionId: room.id, action: { type: 'chat', roomId, text } })
      if (session.value?.id === room.id) removeAttempt(attempt.id)
      else removeRecovery(attempt.id)
    } catch (reason) {
      attempt.error = String(reason) || '房主未返回发送结果'
    } finally {
      if (lastChatReceipt.value?.id === attempt.id) {
        lastChatReceipt.value = { id: attempt.id, elapsedMs: Math.round(performance.now() - started), confirmed: !attempt.error }
      }
    }
  }

  function restoreAttempt(id: number) {
    const item = outbox.value.find(attempt => attempt.id === id)
    if (!item?.error || draft.value || blocked()) return
    draft.value = item.original
    removeAttempt(id)
  }

  return { draft, outbox, recovery, lastChatReceipt, preserve, sendChat, restoreAttempt, removeAttempt, removeRecovery }
}
