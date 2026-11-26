<script lang="ts">
export interface ChatAttempt { id: number; channelId: string; original: string; error: string }
export function chatSendHint(attempts: ChatAttempt[], channelId: string, text: string): string {
  const room = attempts.filter(item => item.channelId === channelId)
  const duplicate = room.find(item => item.original.trim() === text.trim())
  if (duplicate) return duplicate.error ? '相同内容尚未确认，请先核对记录' : '相同内容正在等待大厅确认'
  if (room.filter(item => !item.error).length >= 8) return '已有 8 条消息等待确认，稍后可继续发送'
  return ''
}
</script>

<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { MessageSquare, Send, Headphones, LogOut, RotateCw } from 'lucide-vue-next'
import type { AudioSettings, Channel, Snapshot } from '../model'
import ChannelAudioControls from './ChannelAudioControls.vue'
import type { AudioControls } from './useAudioControls'
import PlayerAvatar from './PlayerAvatar.vue'

const props = defineProps<{ channel: Channel; snapshot: Snapshot; drafts: Record<string, string>; outbox: ChatAttempt[]; audio?: AudioSettings; controls: AudioControls; voiceLabel: string; inVoice: boolean; canRetryVoice: boolean; busy: boolean }>()
const emit = defineEmits<{ leave: []; retry: []; draft: [id: string, text: string]; send: [channelId: string]; restore: [id: number]; dismiss: [id: number]; editProfile: [] }>()
const draft = computed({ get: () => props.drafts[props.channel.id] ?? '', set: text => emit('draft', props.channel.id, text) })
const attempts = computed(() => props.outbox.filter(item => item.channelId === props.channel.id))
const sendHint = computed(() => chatSendHint(props.outbox, props.channel.id, draft.value))
const log = ref<HTMLElement>()
const atBottom = ref(true)
const time = new Intl.DateTimeFormat('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false })
const authors = computed(() => new Map(props.snapshot.members.map(member => [member.id, member])))
function updateScroll() { if (log.value) atBottom.value = log.value.scrollHeight - log.value.clientHeight - log.value.scrollTop < 72 }
watch(() => [props.channel.id, props.snapshot.messages.at(-1)?.id], async ([room], previous) => {
  const follow = atBottom.value || room !== previous?.[0] || props.snapshot.messages.at(-1)?.authorId === props.snapshot.selfId
  await nextTick()
  if (follow && log.value) log.value.scrollTop = log.value.scrollHeight
}, { immediate: true })

function keydown(event: KeyboardEvent) {
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.keyCode !== 229) {
    event.preventDefault()
    if (!event.repeat) emit('send', props.channel.id)
  }
}
</script>

<template>
  <section class="chat">
    <div class="chat-heading">
      <span><MessageSquare /> 频道聊天</span>
      <button class="text-button" @click="emit('leave')"><LogOut /> 退出频道</button>
    </div>
    <div ref="log" class="chat-log" role="log" aria-label="频道聊天记录" @scroll="updateScroll">
      <div v-if="!snapshot.messages.length" class="chat-empty">
        <span class="empty-icon"><MessageSquare /></span><h3>频道聊天</h3><p>发送消息，和队友分享信息。</p>
      </div>
      <article v-for="msg in snapshot.messages" :key="msg.id" class="message" :class="{ 'message-self': msg.authorId === snapshot.selfId }">
        <PlayerAvatar :name="msg.authorName" :avatar="authors.get(msg.authorId)?.avatar" :editable="msg.authorId === snapshot.selfId"
          :class="{ mine: msg.authorId === snapshot.selfId }" @edit="emit('editProfile')" />
        <div class="message-content"><header><b>{{ msg.authorName }}</b><small v-if="msg.authorId === snapshot.selfId" class="self-tag">我</small><time :title="new Date(msg.time).toLocaleString('zh-CN')">{{ time.format(msg.time) }}</time></header><p>{{ msg.text }}</p></div>
      </article>
    </div>
    <form class="composer" @submit.prevent="emit('send', channel.id)">
      <div v-if="attempts.length" class="chat-attempts" aria-live="polite">
        <div v-for="item in attempts" :key="item.id" class="chat-attempt">
          <template v-if="item.error">
            <strong>发送未确认</strong>
            <p class="attempt-original">{{ item.original }}</p>
            <small>{{ item.error }}。请先核对频道记录。</small>
            <div><button type="button" class="text-button" :disabled="!!draft" @click="emit('restore', item.id)">放回输入框</button>
              <button type="button" class="text-button" @click="emit('dismiss', item.id)">移除提示</button></div>
          </template>
          <template v-else>
            <strong>等待大厅确认</strong><p class="attempt-preview">{{ item.original }}</p>
          </template>
        </div>
      </div>
      <div class="compose-box">
      <textarea v-model="draft" rows="2" :placeholder="'发消息到「' + channel.name + '」'" aria-label="频道消息" @keydown="keydown"></textarea>
      <div><small>{{ sendHint || 'Enter 发送 · Shift + Enter 换行' }}</small><button class="send" :disabled="!!sendHint || !draft.trim()" aria-label="发送消息"><Send /></button></div>
    </div></form>
    <footer class="call-controls">
      <div class="voice-status">
        <span class="connection" :class="{ online: inVoice }" role="status"><Headphones />{{ voiceLabel }}</span>
        <button v-if="canRetryVoice" :disabled="busy" class="text-button retry-voice" @click="emit('retry')"><RotateCw />重试语音</button>
      </div>
      <ChannelAudioControls :audio="audio" :controls="controls" />
    </footer>
  </section>
</template>

<style scoped>
.chat-heading {
  min-height: 56px;
  padding-top: 12px;
  padding-bottom: 12px;
}
.chat-heading > span {
  color: #343434;
  font-size: 14px;
}
.chat-heading .text-button {
  min-height: 34px;
  padding: 7px 10px;
}
.chat-log {
  padding-top: 4px;
}
.chat-empty .empty-icon {
  width: 44px;
  height: 44px;
  border-radius: 14px;
  background: #f6f6f6;
  color: #777;
}
.chat-empty .empty-icon svg {
  width: 20px;
  height: 20px;
}
.chat-empty h3 {
  margin-top: 4px;
}
.message {
  margin-top: 20px;
}
.message-content {
  max-width: min(76%, 62ch);
}
.message-self {
  flex-direction: row-reverse;
}
.message-self header {
  justify-content: flex-end;
}
.message-self p {
  width: fit-content;
  max-width: 100%;
  margin-inline-start: auto;
}
.message p {
  color: #333;
}
.chat-attempts {
  max-height: min(20vh, 144px);
  overflow: auto;
  margin-bottom: 8px;
  border-inline-end: 2px solid var(--accent);
  padding-inline-end: 10px;
  text-align: end;
  font-size: 12px;
  color: var(--muted);
}
.chat-attempt + .chat-attempt { margin-top: 8px; }
.chat-attempt strong { color: var(--ink); }
.attempt-original { white-space: pre-wrap; overflow-wrap: anywhere; user-select: text; }
.attempt-preview { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.chat-attempt .text-button { font-size: 12px; }
.compose-box {
  transition: border-color .2s ease, box-shadow .2s ease;
}
.compose-box textarea {
  display: block;
  min-height: 42px;
}
.compose-box textarea::placeholder {
  color: var(--muted);
}
.compose-box small {
  font-size: 11px;
  color: var(--muted);
}
.send {
  width: 36px;
  height: 36px;
  border-radius: 11px;
}
.send:disabled {
  background: #ededed;
  color: #999;
}
.call-controls {
  min-height: 60px;
  gap: 10px;
  padding-top: 10px;
  padding-bottom: 10px;
}
.voice-status {
  display: flex;
  align-items: center;
  flex: 1;
  min-width: 0;
  gap: 5px;
}
.connection {
  min-width: 0;
  margin-right: 0;
}
.retry-voice {
  flex: none;
}
@media (prefers-reduced-motion: reduce) {
  .compose-box { transition: none; }
}
</style>
