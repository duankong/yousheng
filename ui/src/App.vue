<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, reactive, ref, watch } from 'vue'
import { ChevronDown, ChevronRight, CircleAlert, Clipboard, Copy, Link2, LoaderCircle, Power, Settings2, UserRoundPlus, X } from 'lucide-vue-next'
import { invoke } from '@tauri-apps/api/core'
import Dialog from './features/Dialog.vue'
import SettingsDialog from './features/SettingsDialog.vue'
import Chat, { chatSendHint, type ChatAttempt } from './features/Chat.vue'
import Members from './features/Members.vue'
import ProfileEditor from './features/ProfileEditor.vue'
import PlayerAvatar from './features/PlayerAvatar.vue'
import { useLobby } from './features/useLobby'
import { useAudioControls } from './features/useAudioControls'
import { useUpdater } from './features/useUpdater'
import type { PeerSettings, ProfileUpdate } from './model'

type Panel = 'join' | 'invite' | 'close' | 'audio' | 'updates' | 'diagnostics' | 'profile' | 'connection' | ''
const panelTitles: Record<Panel, string> = { join: '加入房间', invite: '邀请好友', close: '关闭房间？', audio: '软件设置', updates: '软件设置', diagnostics: '软件设置', profile: '个人资料', connection: '房间连接详情', '': '' }
const transportLabels = { direct: '直连', relay: '中继', local: '本机房主', unknown: '暂未确定' }
type ConnectionDetails = { transport: keyof typeof transportLabels; rttMs: number | null }
const historyDate = new Intl.DateTimeFormat('zh-CN', { month: 'long', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false })
const panel = ref<Panel>('')
const details = ref<ConnectionDetails>()
const detailsRefresh = ref(0)
const detailsError = ref(false)
const invite = ref('')
const shareLink = ref('')
const pendingInvite = ref('')
const inviteError = ref('')
const notice = ref('')
const copyInput = ref<HTMLInputElement>()
const showHistory = ref(false)
const draft = ref('')
const outbox = ref<ChatAttempt[]>([])
let nextAttemptId = 0
let shareRequest = 0
const lastChatReceipt = ref<{ id: number; elapsedMs?: number; confirmed?: boolean }>()
const { preferences, snapshot, session, ready, busy, error, offline, active, voice, level, speakers, voiceLabel, inVoice, connect, leave, retryVoice, retryingVoice,
  history, historyLoading, historyError, refreshHistory } = useLobby(link => { pendingInvite.value = link })
const installBlocked = computed(() => busy.value || active.value)
const updater = useUpdater(preferences, ready, installBlocked, () => session.value?.id ?? null)
const audioControls = useAudioControls(preferences)
watch(() => audioControls.error, (message, previous) => {
  if (message && panel.value !== 'audio') error.value = message
  else if (!message && error.value === previous) error.value = ''
})
const dismissedUpdate = ref('')
const firstRun = computed(() => !!preferences.value && !preferences.value.nickname.trim())
const editingProfile = computed(() => firstRun.value || panel.value === 'profile')
const myName = computed(() => preferences.value?.nickname || '本地玩家')
const banner = computed(() => (!panel.value && error.value) || offline.value || preferences.value?.shortcutError || '')
const visibleHistory = computed(() => showHistory.value ? history.value : history.value.slice(0, 2))
const blocked = computed(() => busy.value || updater.installing || !ready.value || firstRun.value)
const inviteAction = computed(() => session.value?.isHost ? { icon: UserRoundPlus, title: '邀请好友', caption: '分享我的邀请链接' }
  : busy.value ? { icon: LoaderCircle, title: '正在开启房间…', caption: '' }
    : { icon: Power, title: '开启房间', caption: '本机房间已关闭' })

watch([panel, session, detailsRefresh], async ([view, current], _, onCleanup) => {
  if (view !== 'connection') return
  details.value = undefined
  detailsError.value = false
  if (!current) return
  let cancelled = false
  onCleanup(() => { cancelled = true })
  try {
    const result = await invoke<ConnectionDetails>('lobby_connection_details')
    if (!cancelled) details.value = result
  } catch {
    if (!cancelled) detailsError.value = true
  }
})
let timer: ReturnType<typeof setTimeout> | undefined
onBeforeUnmount(() => clearTimeout(timer))
function inform(message: string) {
  clearTimeout(timer)
  notice.value = message
  timer = setTimeout(() => { notice.value = '' }, 3600)
}
function openPanel(next: Panel) {
  error.value = ''
  panel.value = next
}
function closePanel() {
  if (!busy.value && !updater.installing) {
    shareRequest++
    inviteError.value = ''
    panel.value = ''
    error.value = ''
  }
}
function askJoin(link = '') {
  if (blocked.value) return
  invite.value = link
  openPanel('join')
}
// Keep the latest external invite until identity setup and room transitions finish.
watch([pendingInvite, ready, busy, editingProfile, () => updater.installing], async ([link, initialized, working, editing, installing]) => {
  if (!link || !initialized || working || editing || installing) return
  await nextTick()
  if (pendingInvite.value !== link || blocked.value || editingProfile.value) return
  pendingInvite.value = ''
  if (session.value?.invite === link || shareLink.value === link) return
  askJoin(link)
  if (!active.value) await enter()
})
function profileSaved(updated: ProfileUpdate) {
  if (!preferences.value) return
  const onboarding = firstRun.value
  preferences.value.nickname = updated.nickname
  preferences.value.avatar = updated.avatar
  closePanel()
  if (updated.profileSyncError) error.value = updated.profileSyncError
  else if (onboarding) void connect()
  else inform('个人资料已保存')
}
async function savePeer(id: string, settings: PeerSettings) {
  try { await audioControls.savePeer(id, settings) }
  catch (reason) { error.value = String(reason) }
}
async function enter() {
  const link = invite.value.trim()
  if (blocked.value || !link) return
  if (await connect(link) && panel.value === 'join') panel.value = ''
}
async function joinRecent(link: string) {
  if (blocked.value) return
  if (active.value) askJoin(link)
  else {
    askJoin(link)
    await enter()
  }
}
function inviteOrOpenRoom() {
  if (blocked.value) return
  if (session.value?.isHost) void openShare()
  else void connect()
}
async function leaveRoom() {
  if (session.value?.isHost) openPanel('close')
  else await leave()
}
async function closeRoom() {
  if (session.value?.isHost && await leave()) panel.value = ''
}
function removeAttempt(id: number) {
  outbox.value = outbox.value.filter(item => item.id !== id)
}
async function sendChat() {
  const room = session.value
  const text = draft.value.trim()
  const roomId = snapshot.value?.roomId
  if (!room || !roomId || !active.value || blocked.value || !text || chatSendHint(outbox.value, text)) return
  if (Array.from(text).length > 1000) {
    error.value = '每条消息最多可发送 1000 个字符。'
    return
  }
  const attempt = reactive<ChatAttempt>({ id: ++nextAttemptId, original: draft.value, error: '' })
  const started = performance.now()
  lastChatReceipt.value = { id: attempt.id }
  outbox.value.push(attempt)
  draft.value = ''
  error.value = ''
  try {
    await invoke('lobby_action', { sessionId: room.id, action: { type: 'chat', roomId, text } })
    removeAttempt(attempt.id)
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
  if (!item?.error || draft.value || !active.value) return
  draft.value = item.original
  removeAttempt(id)
}
async function openShare() {
  const current = session.value
  if (!current || blocked.value) return
  const request = ++shareRequest
  openPanel('invite')
  inviteError.value = ''
  shareLink.value = current.isHost ? '' : current.invite
  if (!current.isHost) return
  try {
    const link = await invoke<string>('share_host_invite')
    if (request === shareRequest && session.value?.id === current.id && panel.value === 'invite') shareLink.value = link
  } catch (reason) {
    if (request === shareRequest && session.value?.id === current.id && panel.value === 'invite') inviteError.value = String(reason)
  }
}
async function copyInvite() {
  if (!shareLink.value) return
  try {
    await navigator.clipboard.writeText(shareLink.value)
    closePanel()
    inform('邀请链接已复制')
  } catch {
    copyInput.value?.select()
    inviteError.value = '请按 Ctrl+C 复制已选中的邀请链接。'
  }
}
function dismissBanner() {
  if (error.value && !panel.value) error.value = ''
  else if (offline.value) offline.value = ''
  else if (preferences.value) preferences.value.shortcutError = null
}
watch(() => session.value?.id, () => {
  shareLink.value = ''
  lastChatReceipt.value = undefined
  draft.value = ''
  outbox.value = []
})
watch(offline, message => {
  if (message && ['invite', 'close', 'connection'].includes(panel.value)) panel.value = ''
})
</script>

<template>
  <div class="app-shell">
    <header class="masthead">
      <div class="brand">
        <img src="/grovecall.png" width="31" height="31" alt="" />
        <strong>友声</strong><span>GroveCall</span>
      </div>
      <div class="account">
        <button class="profile-edit" :disabled="!preferences || blocked" aria-label="编辑个人资料" title="编辑个人资料" @click="openPanel('profile')">
          <PlayerAvatar class="profile-avatar mine" :name="myName" :avatar="preferences?.avatar" person />
          <span class="nickname">{{ myName }}</span>
        </button>
        <button class="button settings-link" :disabled="!preferences || blocked" @click="openPanel('audio')"><Settings2 aria-hidden="true" />软件设置</button>
      </div>
    </header>

    <main class="main-view">
      <div v-if="banner" class="alert" role="alert">
        <CircleAlert aria-hidden="true" /><span>{{ banner }}</span>
        <button class="icon-button" aria-label="关闭提示" @click="dismissBanner"><X aria-hidden="true" /></button>
      </div>
      <div v-if="updater.available && updater.enabled && dismissedUpdate !== updater.available.version" class="alert update-banner" role="status">
        <span>友声 {{ updater.available.version }} 可以更新了</span>
        <button class="text-button" :disabled="blocked" @click="openPanel('updates')">查看更新</button>
        <button class="icon-button" aria-label="稍后提醒" @click="dismissedUpdate = updater.available.version"><X aria-hidden="true" /></button>
      </div>

      <div v-if="active && snapshot && session" :key="session.id" class="workspace">
        <Chat v-model="draft" :snapshot="snapshot" :host="session.isHost" :controls="audioControls"
          :voice-label="voiceLabel" :voice-message="voice.message" :in-voice="inVoice" :outbox="outbox" :busy="blocked"
          :can-retry-voice="['device-error', 'error', 'failed', 'disconnected'].includes(voice.state)" :retrying-voice="retryingVoice"
          @send="sendChat" @restore="restoreAttempt" @dismiss="removeAttempt" @retry="retryVoice"
          @edit-profile="openPanel('profile')" @leave="leaveRoom" @connection="openPanel('connection')" />
        <Members :snapshot="snapshot" :speakers="speakers" :peers="preferences?.peers ?? {}" :controls="audioControls" :busy="blocked"
          @invite="openShare" @edit-profile="openPanel('profile')" @save="savePeer" />
      </div>
      <div v-else class="home-view">
        <div class="room-actions">
          <button class="room-action room-action-primary" :disabled="blocked" :aria-haspopup="session?.isHost ? 'dialog' : undefined" @click="inviteOrOpenRoom">
            <span class="action-icon"><component :is="inviteAction.icon" aria-hidden="true" :class="{ 'room-spinner': !session && busy }" /></span>
            <span class="action-label"><b>{{ inviteAction.title }}</b><small v-if="inviteAction.caption">{{ inviteAction.caption }}</small></span>
            <ChevronRight class="action-chevron" aria-hidden="true" />
          </button>
          <button class="room-action" :disabled="blocked" aria-haspopup="dialog" @click="askJoin()">
            <span class="action-icon"><Link2 aria-hidden="true" /></span>
            <span class="action-label"><b>加入房间</b><small>粘贴好友的邀请链接</small></span>
            <ChevronRight class="action-chevron" aria-hidden="true" />
          </button>
        </div>
        <section class="history" aria-labelledby="history-title">
          <header class="history-heading">
            <h2 id="history-title">历史房间</h2>
            <button v-if="history.length > 2" class="text-button history-more" :aria-expanded="showHistory" aria-controls="room-history" @click="showHistory = !showHistory">
              {{ showHistory ? '收起' : '查看全部' }}<ChevronDown aria-hidden="true" />
            </button>
          </header>
          <div v-if="historyError" class="history-empty" role="alert">读取历史房间失败。<button class="text-button" @click="refreshHistory">重试</button></div>
          <ul v-else-if="visibleHistory.length" id="room-history" class="history-list">
            <li v-for="(entry, index) in visibleHistory" :key="entry.hostId" class="past-room">
              <PlayerAvatar :name="entry.nickname" :class="index % 2 ? 'past-avatar-purple' : 'past-avatar-green'" />
              <div><b>{{ entry.nickname }} 的房间</b><time :datetime="new Date(entry.lastJoinedAt).toISOString()">{{ historyDate.format(entry.lastJoinedAt) }}</time></div>
              <button class="button rejoin" :disabled="blocked" :aria-label="'加入' + entry.nickname + '的房间'" @click="joinRecent(entry.invite)">加入</button>
            </li>
          </ul>
          <p v-else class="history-empty" :role="historyLoading ? 'status' : undefined">{{ historyLoading ? '正在读取历史房间…' : '加入过的好友房间会显示在这里。' }}</p>
        </section>
      </div>
    </main>

    <ProfileEditor v-if="editingProfile && preferences" :preferences="preferences" :first-run="firstRun" @close="closePanel" @saved="profileSaved" />
    <SettingsDialog v-else-if="(panel === 'audio' || panel === 'updates' || panel === 'diagnostics') && preferences" :tab="panel" :level="level"
      :speaking="!!snapshot && speakers.includes(snapshot.selfId)" :connected="inVoice" :updater="updater" :controls="audioControls" @tab="panel = $event" @close="closePanel" />
    <Dialog v-else-if="panel" :key="panel" :title="panelTitles[panel]" :dismissible="!busy" @close="closePanel">
      <div v-if="panel === 'connection'">
        <div role="status">
          <p v-if="detailsError" class="dialog-desc">暂时无法读取连接信息，请刷新重试。</p>
          <p v-else-if="!details" class="dialog-desc">正在读取连接信息…</p>
          <dl v-if="details || lastChatReceipt" class="connection-facts">
            <template v-if="details">
              <div><dt>连接方式</dt><dd>{{ transportLabels[details.transport] }}</dd></div>
              <div v-if="details.transport !== 'local'"><dt>和房主的网络往返时间</dt><dd>{{ details.rttMs === null ? '暂无数据' : details.rttMs < 1 ? '小于 1 ms' : Math.round(details.rttMs) + ' ms' }}</dd></div>
            </template>
            <div v-if="lastChatReceipt"><dt>最近一条文字消息</dt>
              <dd>{{ lastChatReceipt.elapsedMs === undefined ? '等待房主确认' : (lastChatReceipt.confirmed ? '房主已确认' : '未确认') + ' · ' + lastChatReceipt.elapsedMs + ' ms' }}</dd></div>
          </dl>
          <p v-if="details?.transport === 'local'" class="dialog-desc">房间在这台电脑上运行。队友可在自己的连接详情中查看网络延迟。</p>
        </div>
        <footer v-if="details?.transport !== 'local'" class="dialog-actions"><button class="button" :disabled="!details && !detailsError" @click="detailsRefresh++">刷新</button></footer>
      </div>
      <form v-else-if="panel === 'join'" @submit.prevent="enter">
        <p v-if="active" class="dialog-desc">{{ session?.isHost ? '加入成功后会关闭你的房间，当前成员会断开连接。' : '加入成功后会退出当前房间。' }}</p>
        <label class="field">好友的邀请链接<input v-model.trim="invite" required autofocus spellcheck="false" placeholder="粘贴友声邀请链接" :disabled="busy" /></label>
        <p v-if="error" class="form-error" role="alert">{{ error }}</p>
        <footer class="dialog-actions">
          <button class="button" type="button" :disabled="busy" @click="closePanel">取消</button>
          <button class="button tinted" :disabled="blocked || !invite.trim()">{{ busy ? '正在加入…' : '加入房间' }}</button>
        </footer>
      </form>
      <div v-else-if="panel === 'invite'">
        <label class="field">邀请链接<input ref="copyInput" :value="shareLink" :placeholder="inviteError ? '' : '正在准备邀请链接…'" readonly @focus="copyInput?.select()" /></label>
        <p v-if="inviteError" class="form-error" role="alert">{{ inviteError }}</p>
        <footer class="dialog-actions">
          <button class="button" type="button" @click="closePanel">关闭</button>
          <button class="button tinted" :disabled="!shareLink" @click="copyInvite"><Copy aria-hidden="true" />复制链接</button>
        </footer>
      </div>
      <div v-else-if="panel === 'close'">
        <p class="dialog-desc">关闭后，房间里的所有成员都会断开连接。</p>
        <p v-if="error" class="form-error" role="alert">{{ error }}</p>
        <footer class="dialog-actions">
          <button class="button" autofocus :disabled="busy" @click="closePanel">取消</button>
          <button class="button danger" :disabled="busy" @click="closeRoom">关闭房间</button>
        </footer>
      </div>
    </Dialog>
    <Transition name="notice"><div v-if="notice" class="toast" role="status"><Clipboard aria-hidden="true" />{{ notice }}</div></Transition>
  </div>
</template>
