<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { MessageSquare, Send, Headphones, Mic, MicOff, Settings2, LogOut, RotateCw } from 'lucide-vue-next'
import { invoke } from '@tauri-apps/api/core'
import { modes, type AudioSettings, type Channel, type Snapshot } from '../model'
import PlayerAvatar from './PlayerAvatar.vue'

const props = defineProps<{ channel: Channel; snapshot: Snapshot; drafts: Record<string, string>; audio?: AudioSettings; voiceLabel: string; inVoice: boolean; canRetryVoice: boolean; busy: boolean }>()
const emit = defineEmits<{ mute: []; settings: []; leave: []; retry: []; draft: [id: string, text: string]; error: [error: string]; editProfile: [] }>()
const draft = computed({ get: () => props.drafts[props.channel.id] ?? '', set: text => emit('draft', props.channel.id, text) })
const sending = ref(false)
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

async function send() {
  const text = draft.value.trim()
  const channelId = props.channel.id
  if (!text || sending.value) return
  if (Array.from(text).length > 1000) { emit('error', '每条消息最多可发送 1000 个字符。'); return }
  const original = draft.value
  emit('error', '')
  sending.value = true
  try {
    await invoke('lobby_action', { action: { type: 'chat', channelId, text } })
    if (props.drafts[channelId] === original) emit('draft', channelId, '')
  } catch (error) { emit('error', String(error)) }
  finally { sending.value = false }
}
function keydown(event: KeyboardEvent) {
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.keyCode !== 229) {
    event.preventDefault()
    void send()
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
      <article v-for="msg in snapshot.messages" :key="msg.id" class="message">
        <PlayerAvatar :name="msg.authorName" :avatar="authors.get(msg.authorId)?.avatar" :editable="msg.authorId === snapshot.selfId"
          :class="{ mine: msg.authorId === snapshot.selfId }" @edit="emit('editProfile')" />
        <div><header><b>{{ msg.authorName }}</b><small v-if="msg.authorId === snapshot.selfId" class="self-tag">我</small><time :title="new Date(msg.time).toLocaleString('zh-CN')">{{ time.format(msg.time) }}</time></header><p>{{ msg.text }}</p></div>
      </article>
    </div>
    <form class="composer" @submit.prevent="send"><div class="compose-box">
      <textarea v-model="draft" rows="2" :placeholder="'发消息到「' + channel.name + '」'" aria-label="频道消息" @keydown="keydown"></textarea>
      <div><small>Enter 发送 · Shift + Enter 换行</small><button class="send" :disabled="sending || !draft.trim()" aria-label="发送消息"><Send /></button></div>
    </div></form>
    <footer class="call-controls">
      <div class="voice-status">
        <span class="connection" :class="{ online: inVoice }" role="status"><Headphones />{{ voiceLabel }}</span>
        <button v-if="canRetryVoice" :disabled="busy" class="text-button retry-voice" @click="emit('retry')"><RotateCw />重试语音</button>
      </div>
      <button :disabled="!audio" class="audio-button" :aria-pressed="audio?.muted" :title="audio?.muted ? '取消麦克风静音' : '麦克风静音'" @click="emit('mute')">
        <component :is="audio?.muted ? MicOff : Mic" />{{ audio?.muted ? '已静音' : (audio ? modes[audio.mode] : '麦克风') }}
        <kbd v-if="audio?.mode === 'ptt' && !audio.muted">{{ audio.shortcut }}</kbd></button>
      <button class="icon-button" title="声音设置" aria-label="声音设置" @click="emit('settings')"><Settings2 /></button>
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
.message p {
  color: #333;
}
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
.audio-button {
  min-height: 36px;
  border-color: #e4e4e4;
  border-radius: 10px;
}
.retry-voice {
  flex: none;
}
@media (max-width: 650px) {
  .call-controls {
    flex-wrap: wrap;
  }
  .voice-status {
    flex: 1 1 100%;
  }
}
@media (prefers-reduced-motion: reduce) {
  .compose-box { transition: none; }
}
</style>
