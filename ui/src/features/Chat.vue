<script lang="ts">
export interface ChatAttempt { id: number; original: string; error: string }
export function chatSendHint(attempts: ChatAttempt[], text: string): string {
  const duplicate = attempts.find(item => item.original.trim() === text.trim())
  if (duplicate) return duplicate.error ? '相同内容尚未确认，请先核对记录' : '相同内容正在等待房主确认'
  if (attempts.filter(item => !item.error).length >= 8) return '已有 8 条消息等待确认，稍后可继续发送'
  return ''
}
</script>

<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { Activity, RotateCw } from 'lucide-vue-next'
import type { AudioSettings, Snapshot } from '../model'
import RoomAudioControls from './RoomAudioControls.vue'
import type { AudioControls } from './useAudioControls'
import PlayerAvatar from './PlayerAvatar.vue'

const props = defineProps<{ snapshot: Snapshot; host: boolean; outbox: ChatAttempt[]; audio?: AudioSettings; controls: AudioControls; voiceLabel: string; voiceMessage?: string; inVoice: boolean; canRetryVoice: boolean; busy: boolean }>()
const draft = defineModel<string>({ required: true })
const emit = defineEmits<{ leave: []; retry: []; send: []; restore: [id: number]; dismiss: [id: number]; editProfile: []; connection: [] }>()
const sendHint = computed(() => chatSendHint(props.outbox, draft.value))
const log = ref<HTMLElement>()
const atBottom = ref(true)
const time = new Intl.DateTimeFormat('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false })
const authors = computed(() => new Map(props.snapshot.members.map(member => [member.id, member])))
const roomAction = computed(() => props.host ? '关闭房间' : '退出房间')

function updateScroll() {
  const element = log.value
  if (element) atBottom.value = element.scrollHeight - element.clientHeight - element.scrollTop < 72
}
watch(() => props.snapshot.messages.at(-1)?.id, async () => {
  const follow = atBottom.value || props.snapshot.messages.at(-1)?.authorId === props.snapshot.selfId
  await nextTick()
  if (follow && log.value) log.value.scrollTop = log.value.scrollHeight
}, { immediate: true })

function keydown(event: KeyboardEvent) {
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.keyCode !== 229) {
    event.preventDefault()
    if (!event.repeat && !props.busy) emit('send')
  }
}
</script>

<template>
  <section class="chat" aria-labelledby="room-title">
    <header class="chat-heading">
      <h1 id="room-title" :title="snapshot.roomName">{{ snapshot.roomName }}</h1>
      <button class="button leave-room" :disabled="busy" :aria-haspopup="host ? 'dialog' : undefined" @click="emit('leave')">{{ roomAction }}</button>
    </header>
    <div ref="log" class="chat-log" role="log" aria-label="房间聊天记录" @scroll="updateScroll">
      <article v-for="msg in snapshot.messages" :key="msg.id" class="message" :class="{ 'message-self': msg.authorId === snapshot.selfId }">
        <PlayerAvatar :name="msg.authorName" :avatar="authors.get(msg.authorId)?.avatar" :editable="msg.authorId === snapshot.selfId"
          :class="{ mine: msg.authorId === snapshot.selfId }" @edit="emit('editProfile')" />
        <div class="message-content">
          <header><b>{{ msg.authorName }}</b><time :title="new Date(msg.time).toLocaleString('zh-CN')">{{ time.format(msg.time) }}</time></header>
          <p>{{ msg.text }}</p>
        </div>
      </article>
    </div>
    <form class="composer" @submit.prevent="emit('send')">
      <div v-if="outbox.length" class="chat-attempts" aria-live="polite">
        <div v-for="item in outbox" :key="item.id" class="chat-attempt">
          <template v-if="item.error">
            <strong>发送未确认</strong>
            <p class="attempt-original">{{ item.original }}</p>
            <small>{{ item.error }}。请先核对房间记录。</small>
            <div>
              <button type="button" class="text-button" :disabled="!!draft" @click="emit('restore', item.id)">放回输入框</button>
              <button type="button" class="text-button" @click="emit('dismiss', item.id)">移除提示</button>
            </div>
          </template>
          <template v-else>
            <strong>等待房主确认</strong><p class="attempt-preview">{{ item.original }}</p>
          </template>
        </div>
      </div>
      <div class="compose-box">
        <textarea v-model="draft" rows="1" placeholder="发送消息" aria-label="房间消息" title="Enter 发送，Shift + Enter 换行" @keydown="keydown"></textarea>
        <button class="send" :disabled="busy || !!sendHint || !draft.trim()" aria-label="发送消息">发送</button>
      </div>
      <p v-if="sendHint" class="send-hint" role="status">{{ sendHint }}</p>
    </form>
    <footer class="call-controls">
      <div class="voice-status">
        <button type="button" class="connection" :class="{ online: inVoice }" title="查看房间连接详情" aria-haspopup="dialog" @click="emit('connection')">
          <span role="status">{{ voiceLabel }}</span><Activity aria-hidden="true" />
        </button>
        <button v-if="canRetryVoice" :disabled="busy" class="text-button retry-voice" @click="emit('retry')"><RotateCw aria-hidden="true" />重试语音</button>
      </div>
      <RoomAudioControls :audio="audio" :controls="controls" />
    </footer>
    <p v-if="voiceMessage" class="voice-detail" role="status">{{ voiceMessage }}</p>
  </section>
</template>

<style scoped>
.chat { min-width: 0; min-height: 0; display: flex; flex-direction: column; padding: 16px 16px 12px; }
.chat-heading { display: flex; flex: none; justify-content: space-between; align-items: center; gap: 8px; }
.chat-heading h1 { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 21px; line-height: 1.35; }
.leave-room { flex: none; color: var(--muted); }
.leave-room:hover:not(:disabled) { color: var(--danger); border-color: color-mix(in srgb, var(--danger) 25%, var(--line)); }
.chat-log { min-height: 0; flex: 1; overflow: auto; overscroll-behavior: contain; padding: 14px 2px; scrollbar-gutter: stable; }
.message { display: flex; align-items: flex-start; gap: 8px; }
.message + .message { margin-top: 14px; }
.message-content { min-width: 0; max-width: min(76%, 62ch); }
.message header { display: flex; align-items: baseline; gap: 7px; font-size: 12px; }
.message header b { overflow-wrap: anywhere; }
.message time { color: var(--muted); white-space: nowrap; font-size: 11px; }
.message p {
  width: fit-content;
  max-width: 100%;
  padding: 5px 9px;
  margin-top: 3px;
  border-radius: 8px;
  background: var(--panel);
  font-size: 13px;
  line-height: 1.55;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.message-self { flex-direction: row-reverse; }
.message-self header { flex-direction: row-reverse; }
.message-self p { margin-inline-start: auto; background: var(--accent-soft); }
.chat-attempts { max-height: min(20vh, 130px); overflow: auto; margin-bottom: 8px; border-inline-end: 2px solid var(--accent); padding-inline-end: 10px; text-align: end; font-size: 12px; color: var(--muted); }
.chat-attempt + .chat-attempt { margin-top: 8px; }
.chat-attempt strong { color: var(--ink); }
.attempt-original { white-space: pre-wrap; overflow-wrap: anywhere; user-select: text; }
.attempt-preview { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.compose-box { display: flex; align-items: flex-end; gap: 6px; padding: 4px; border: 1px solid var(--line); border-radius: 8px; transition: border-color .15s ease, box-shadow .15s ease; }
.compose-box:focus-within { border-color: var(--accent); box-shadow: 0 0 0 2px var(--accent-soft); }
.compose-box textarea { field-sizing: content; flex: 1; min-width: 0; min-height: 30px; max-height: 85px; resize: none; border: 0; padding: 6px; background: transparent; color: inherit; outline: none; line-height: 1.35; }
.compose-box textarea::placeholder { color: var(--muted); }
.send { flex: none; min-height: 30px; padding: 5px 10px; border-radius: 6px; background: var(--ink); color: white; font-size: 12px; }
.send:hover:not(:disabled) { background: #393939; }
.send:disabled { background: var(--panel); color: var(--muted); opacity: 1; }
.send-hint { margin-top: 5px; font-size: 11px; color: var(--muted); }
.call-controls { display: flex; align-items: center; flex: none; min-height: 38px; gap: 6px; margin-top: 7px; }
.voice-status { display: flex; align-items: center; flex: 1; min-width: 0; gap: 4px; }
.connection { display: inline-flex; align-items: center; gap: 6px; padding: 5px 6px; border-radius: 6px; color: var(--muted); font-size: 12px; }
.connection::before { content: ''; width: 7px; height: 7px; flex: none; border-radius: 50%; background: #aaa; }
.connection.online::before { background: var(--live); }
.connection:hover { background: var(--panel); }
.connection svg, .retry-voice svg { width: 13px; height: 13px; }
.retry-voice { color: var(--accent); padding: 3px 5px; }
.voice-detail { margin-top: 4px; color: var(--muted); font-size: 12px; line-height: 1.45; overflow-wrap: anywhere; }
</style>
