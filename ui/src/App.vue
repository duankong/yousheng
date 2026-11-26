<script setup lang="ts">
import { computed, nextTick, reactive, ref, watch } from 'vue'
import { Activity, ArrowRight, AudioLines, ChevronRight, Circle, CircleAlert, Clipboard, Copy, House, Link2, LoaderCircle, LogOut, Menu, Plus, Radio, Settings2, Trash2, Unlink2, UserRoundPlus, Users, Volume2, X } from 'lucide-vue-next'
import { invoke, isTauri } from '@tauri-apps/api/core'
import Dialog from './features/Dialog.vue'
import SettingsDialog from './features/SettingsDialog.vue'
import Chat, { chatSendHint, type ChatAttempt } from './features/Chat.vue'
import Members from './features/Members.vue'
import ProfileEditor from './features/ProfileEditor.vue'
import PlayerAvatar from './features/PlayerAvatar.vue'
import { useLobby } from './features/useLobby'
import { useAudioControls } from './features/useAudioControls'
import { useUpdater } from './features/useUpdater'
import type { Channel, Preferences } from './model'

type Panel = 'join' | 'invite' | 'create' | 'delete' | 'exit' | 'audio' | 'updates' | 'profile' | 'connection' | ''
type LocalChannel = Pick<Channel, 'id' | 'name'>
const transportLabels = { direct: '直连', relay: '中继', local: '本机主持', unknown: '暂未确定' }
type ConnectionDetails = { transport: keyof typeof transportLabels; rttMs: number | null }
const panel = ref<Panel>('')
const details = ref<ConnectionDetails>()
const detailsRefresh = ref(0)
const detailsError = ref(false)
const drawer = ref(false)
const showPeople = ref(false)
const invite = ref('')
const shareLink = ref('')
const pendingInvite = ref('')
const channelName = ref('')
const channelNameLength = computed(() => Array.from(channelName.value.trim()).length)
const deletion = ref<LocalChannel>()
const notice = ref('')
const copyInput = ref<HTMLInputElement>()
const lastJoin = ref('')
const joiningChannel = ref('')
const drafts = ref<Record<string, string>>({})
const outbox = ref<ChatAttempt[]>([])
let nextAttemptId = 0
const lastChatReceipt = ref<{ id: number; elapsedMs?: number; confirmed?: boolean }>()
let draftLobby = ''
const { preferences, snapshot, session, ready, busy, error, offline, active, voice, level, speakers, voiceLabel, inVoice, connect, leave, action,
  localChannels, loadingChannels, channelsError, refreshLocalChannels } = useLobby(link => { pendingInvite.value = link })
const connected = computed(() => !!(snapshot.value || session.value))
const lobbyStatus = computed(() => {
  if (snapshot.value) return { label: '大厅已连接', icon: Radio, tone: 'live' }
  if (session.value) return { label: '正在同步', icon: LoaderCircle, tone: 'pending' }
  if (offline.value) return { label: '大厅已断开', icon: Unlink2, tone: 'idle' }
  if (busy.value) return { label: '正在连接', icon: LoaderCircle, tone: 'pending' }
  return { label: '大厅未开启', icon: Circle, tone: 'idle' }
})
const installBlocked = computed(() => busy.value || connected.value || inVoice.value || ['connecting', 'reconnecting'].includes(voice.value.state))
const updater = useUpdater(preferences, ready, installBlocked)
const audioControls = useAudioControls(preferences)
const currentAudio = computed(() => preferences.value && { ...preferences.value.audio, ...audioControls.changes })
watch(() => audioControls.error, (message, previous) => {
  if (message && panel.value !== 'audio') error.value = message
  else if (!message && error.value === previous) error.value = ''
})
const dismissedUpdate = ref('')
const firstRun = computed(() => !!preferences.value && !preferences.value.nickname.trim())
const editingProfile = computed(() => firstRun.value || panel.value === 'profile')
const myName = computed(() => preferences.value?.nickname || '本地玩家')
const navChannels = computed<(LocalChannel & { canDelete?: boolean })[]>(() => snapshot.value?.channels ?? (connected.value ? [] : localChannels.value))
const heading = computed(() => active.value?.name || (connected.value ? '我的大厅' : '本机频道'))
const banner = computed(() => (!panel.value && error.value) || offline.value || preferences.value?.shortcutError || '')
const panelTitle: Record<Panel, string> = { join: '通过邀请加入', invite: '邀请好友', create: '创建语音频道', delete: '删除频道', exit: '退出大厅', audio: '软件设置', updates: '软件设置', profile: '个人资料', connection: '大厅连接详情', '': '' }
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
let timer: ReturnType<typeof setTimeout>
function inform(message: string) { clearTimeout(timer); notice.value = message; timer = setTimeout(() => { notice.value = '' }, 3600) }
function openPanel(next: Panel) {
  drawer.value = false
  error.value = ''
  if (next === 'create') channelName.value = ''
  panel.value = next
}
function closePanel() { if (!busy.value) { panel.value = ''; error.value = '' } }
// An invite may arrive during onboarding or another connection. Keep the latest
// one until the player has a saved identity and the current action has finished.
watch([pendingInvite, ready, busy, editingProfile, () => updater.installing], async ([link, initialized, working, editing, installing]) => {
  if (!link || !initialized || working || editing || installing) return
  // Let the completed action dismiss its own dialog before showing this invite.
  await nextTick()
  if (pendingInvite.value !== link || busy.value || editingProfile.value || updater.installing) return
  pendingInvite.value = ''
  if ((session.value?.invite === link || shareLink.value === link) && connected.value) return
  invite.value = link
  openPanel('join')
  if (!connected.value) await enter()
})
function profileSaved(updated: Preferences) {
  const onboarding = firstRun.value
  preferences.value = updated
  closePanel()
  if (updated.profileSyncError) error.value = updated.profileSyncError
  else if (!onboarding) inform('个人资料已保存')
}
async function enter() {
  if (busy.value || !invite.value.trim()) return
  const link = invite.value.trim()
  lastJoin.value = link
  if (await connect(link) && invite.value.trim() === link) panel.value = ''
}
async function startHost() {
  lastJoin.value = ''
  await connect()
}
async function createChannel() {
  if (!channelNameLength.value || channelNameLength.value > 20 || busy.value) return
  if (!connected.value) lastJoin.value = ''
  if (await action({ type: 'create', name: channelName.value.trim() }, !connected.value) && panel.value === 'create') closePanel()
}
function askDelete(c: LocalChannel) { deletion.value = c; openPanel('delete') }
async function deleteChannel() {
  if (deletion.value && await action({ type: 'delete', channelId: deletion.value.id }) && panel.value === 'delete') closePanel()
}
async function joinChannel(id: string) {
  if (busy.value || id === active.value?.id) return
  drawer.value = false
  if (!connected.value) lastJoin.value = ''
  joiningChannel.value = id
  try { await action({ type: 'join', channelId: id }, !connected.value) }
  finally { joiningChannel.value = '' }
}
async function leaveSession() {
  if (await leave()) { panel.value = ''; lastJoin.value = ''; drafts.value = {}; outbox.value = []; draftLobby = '' }
}
function removeAttempt(id: number) {
  outbox.value = outbox.value.filter(item => item.id !== id)
}
async function sendChat(channelId: string) {
  const original = drafts.value[channelId] ?? ''
  const text = original.trim()
  if (!text || active.value?.id !== channelId) return
  if (chatSendHint(outbox.value, channelId, text)) return
  if (Array.from(text).length > 1000) { error.value = '每条消息最多可发送 1000 个字符。'; return }
  const attempt = reactive<ChatAttempt>({ id: ++nextAttemptId, channelId, original, error: '' })
  const started = performance.now()
  lastChatReceipt.value = { id: attempt.id }
  outbox.value.push(attempt)
  drafts.value[channelId] = ''
  error.value = ''
  try {
    await invoke('lobby_action', { action: { type: 'chat', channelId, text } })
    removeAttempt(attempt.id)
  } catch (reason) {
    attempt.error = String(reason) || '大厅未返回发送结果'
  } finally {
    if (lastChatReceipt.value?.id === attempt.id) {
      lastChatReceipt.value = { id: attempt.id, elapsedMs: Math.round(performance.now() - started), confirmed: !attempt.error }
    }
  }
}
function restoreAttempt(id: number) {
  const item = outbox.value.find(attempt => attempt.id === id)
  if (!item?.error || item.channelId !== active.value?.id || drafts.value[item.channelId]) return
  drafts.value[item.channelId] = item.original
  removeAttempt(id)
}
async function openShare() {
  const current = session.value
  if (!current) return
  openPanel('invite')
  shareLink.value = current.isHost ? '' : current.invite
  if (!current.isHost) return
  try {
    const link = await invoke<string>('share_host_invite')
    if (session.value === current && panel.value === 'invite') shareLink.value = link
  } catch (reason) {
    if (session.value === current && panel.value === 'invite') error.value = String(reason)
  }
}
async function copyInvite() {
  if (!shareLink.value) return
  try { await navigator.clipboard.writeText(shareLink.value); inform('邀请链接已复制'); panel.value = '' }
  catch { openPanel('invite'); await nextTick(); copyInput.value?.select(); error.value = '请按 Ctrl+C 复制已选中的邀请链接。' }
}
function dismissBanner() {
  if (error.value && !panel.value) error.value = ''
  else if (offline.value) offline.value = ''
  else if (preferences.value) preferences.value.shortcutError = null
}
watch(() => snapshot.value?.channelId, () => { showPeople.value = false })
watch(() => session.value?.invite, link => {
  shareLink.value = ''
  lastChatReceipt.value = undefined
  if (link && link !== draftLobby) { drafts.value = {}; outbox.value = []; draftLobby = link }
})
watch(offline, message => {
  if (message && !['profile', 'join', 'audio', 'updates'].includes(panel.value)) panel.value = ''
})
</script>

<template>
  <div class="app-shell">
    <div v-if="drawer" class="drawer-scrim" @click="drawer = false"></div>
    <aside class="sidebar" :class="{ expanded: drawer }">
      <div class="brand">
        <img class="brand-mark" src="/grovecall.png" width="42" height="42" alt="" />
        <div><strong>友声</strong><small>GroveCall</small></div>
        <button v-if="drawer" class="icon-button close-drawer" aria-label="关闭导航" @click="drawer = false"><X /></button>
      </div>
      <nav class="navigation" aria-label="大厅导航">
        <button class="nav-item" :class="{ selected: !active }" :disabled="busy" @click="drawer = false; active && action({type: 'leave'})">
          <House /><span>{{ connected ? '我的大厅' : '本机频道' }}</span>
        </button>
        <div class="nav-heading section-label">语音频道<button class="icon-button" :disabled="busy || (isTauri() && !ready)" title="创建频道" aria-label="创建频道" @click="openPanel('create')"><Plus /></button></div>
        <div v-if="!navChannels.length" class="nav-hint">{{ !connected && loadingChannels ? '正在读取频道…' : !connected && channelsError ? '频道读取失败' : '暂无频道' }}</div>
        <div v-for="c in navChannels" :key="c.id" class="channel-row" :class="{ selected: active?.id === c.id }">
          <button class="nav-item" :disabled="busy" :aria-current="active?.id === c.id" @click="joinChannel(c.id)">
            <Volume2 /><span>{{ c.name }}</span><small v-if="joiningChannel === c.id" role="status">正在加入…</small><small v-else-if="snapshot">{{ snapshot.members.filter(m => m.channelId === c.id).length }}</small></button>
          <button v-if="c.canDelete" class="delete-channel icon-button" :aria-label="'删除' + c.name" :title="'删除' + c.name" @click="askDelete(c)"><Trash2 /></button>
        </div>
      </nav>
      <div class="sidebar-bottom">
        <button class="settings-link" :disabled="!preferences" @click="openPanel('audio')"><Settings2 /> 软件设置 <ChevronRight /></button>
        <div class="profile"><button class="profile-edit" :disabled="!preferences || busy" title="编辑个人资料" aria-label="编辑个人资料" @click="openPanel('profile')"><PlayerAvatar class="profile-avatar" :name="myName" :avatar="preferences?.avatar" /><span><b>{{ myName }}</b><small>{{ connected ? (session?.isHost ? '大厅管理员' : '大厅成员') : '个人资料' }}</small></span></button>
          <button v-if="connected" class="icon-button" :title="session?.isHost ? '停止大厅' : '退出大厅'" :aria-label="session?.isHost ? '停止大厅' : '退出大厅'" @click="openPanel('exit')"><LogOut /></button></div>
      </div>
    </aside>
    <main>
      <header class="topbar">
        <button class="icon-button mobile-menu" aria-label="展开导航" :aria-expanded="drawer" @click="drawer = true"><Menu /></button>
        <div class="topbar-title"><component :is="active ? Volume2 : House" /><strong>{{ heading }}</strong></div>
        <span class="lobby-status" :data-tone="lobbyStatus.tone" role="status">
          <button v-if="snapshot" class="connection-details-button" title="查看大厅连接详情" aria-haspopup="dialog" @click="openPanel('connection')">
            <Radio aria-hidden="true" />{{ lobbyStatus.label }}<ChevronRight aria-hidden="true" />
          </button>
          <template v-else><component :is="lobbyStatus.icon" aria-hidden="true" />{{ lobbyStatus.label }}</template>
        </span>
        <button v-if="connected" class="button invite-button" :disabled="!session" @click="openShare"><UserRoundPlus /> 邀请好友</button>
        <button v-if="snapshot" class="icon-button people-toggle" :aria-label="showPeople ? '关闭成员列表' : '查看成员'" :aria-expanded="showPeople" @click="showPeople = !showPeople"><Users /></button>
      </header>
      <div v-if="banner" class="alert" role="alert"><CircleAlert /><span>{{ banner }}</span>
        <button v-if="banner === offline && lastJoin && !connected" class="text-button" @click="invite = lastJoin; openPanel('join'); enter()">重试</button>
        <button class="icon-button" aria-label="关闭提示" @click="dismissBanner"><X /></button></div>
      <div v-if="updater.available && updater.enabled && dismissedUpdate !== updater.available.version" class="alert update-banner" role="status">
        <span>友声 {{ updater.available.version }} 可以更新了</span>
        <button class="text-button" @click="openPanel('updates')">查看更新</button>
        <button class="icon-button" aria-label="稍后提醒" @click="dismissedUpdate = updater.available.version"><X /></button>
      </div>
      <div v-if="!connected" class="landing-scroll"><div class="landing">
        <header class="hero">
          <div class="hero-emblem" aria-hidden="true"><img class="hero-mark" src="/grovecall.png" alt="" /></div>
          <div class="hero-content">
            <h1>本机频道</h1>
            <p>选择一个频道，和队友开始语音聊天。</p>
            <div class="home-actions">
              <button class="button primary" :disabled="busy || !ready" @click="startHost"><Radio /> 开启大厅</button>
              <button class="button" :disabled="busy || (isTauri() && !ready)" @click="openPanel('join')"><Link2 /> 通过邀请加入</button>
            </div>
          </div>
        </header>
        <section class="home-channels"><div class="overview-title"><h2>我的频道 <span v-if="localChannels.length" class="channel-count">{{ localChannels.length }}</span></h2>
          <button class="button" :disabled="busy || (isTauri() && !ready)" @click="openPanel('create')"><Plus /> 创建频道</button></div>
          <div v-if="channelsError" class="local-list-error" role="alert"><span>{{ channelsError }}</span><button class="button" @click="refreshLocalChannels">重新读取</button></div>
          <div v-else-if="loadingChannels && !localChannels.length" class="overview-empty" role="status">正在读取频道…</div>
          <div v-else-if="localChannels.length" class="room-list"><button v-for="c in localChannels" :key="c.id" :disabled="busy || !ready" @click="joinChannel(c.id)">
            <span class="list-icon"><Volume2 /></span><div><b>{{ c.name }}</b><small>{{ joiningChannel === c.id ? '正在加入…' : '点击进入频道' }}</small></div><ArrowRight /></button></div>
          <div v-else class="overview-empty"><span class="empty-icon"><AudioLines /></span><h3>还没有语音频道</h3><p>创建一个频道，随时邀请队友加入。</p></div>
        </section>
      </div></div>
      <template v-else>
        <div v-if="snapshot" class="workspace" :class="{ 'people-open': showPeople }">
          <Chat v-if="active" :channel="active" :snapshot="snapshot" :audio="currentAudio" :controls="audioControls" :voice-label="voiceLabel" :in-voice="inVoice"
            :drafts="drafts" @draft="(id, text) => { drafts[id] = text }"
            :outbox="outbox" @send="sendChat" @restore="restoreAttempt" @dismiss="removeAttempt"
            :can-retry-voice="['device-error', 'error', 'failed', 'disconnected'].includes(voice.state)" :busy="busy"
            @retry="action({ type: 'join', channelId: active.id })" @edit-profile="openPanel('profile')" @leave="action({ type: 'leave' })" />
          <section v-else class="lobby-overview"><div class="overview-title"><h2>语音频道</h2>
              <button class="button" @click="openPanel('create')"><Plus /> 创建频道</button></div>
            <p class="section-description">{{ snapshot.channels.length ? '选择频道，与频道里的队友通话和聊天。' : '创建第一个语音频道，和队友开始聊天。' }}</p>
            <div class="room-list"><button v-for="c in snapshot.channels" :key="c.id" :disabled="busy" @click="joinChannel(c.id)">
              <span class="list-icon"><Volume2 /></span><div><b>{{ c.name }}</b><small>{{ snapshot.members.filter(m => m.channelId === c.id).length }} 位成员</small></div><ArrowRight />
            </button></div>
            <div v-if="!snapshot.channels.length" class="overview-empty"><span class="empty-icon"><AudioLines /></span><h3>还没有语音频道</h3></div>
          </section>
          <Members :snapshot="snapshot" :speakers="speakers" :peers="preferences?.peers ?? {}" @edit-profile="openPanel('profile')" @saved="(id, settings) => { if (preferences) preferences.peers[id] = settings }" @error="error = $event" />
        </div>
        <div v-else class="loading"><Activity /> 正在同步频道…</div>
        <p v-if="active && voice.message" class="voice-detail" role="status">{{ voice.message }}</p>
      </template>
    </main>
    <ProfileEditor v-if="editingProfile && preferences" :preferences="preferences" :first-run="firstRun" @close="closePanel" @saved="profileSaved" />
    <SettingsDialog v-else-if="(panel === 'audio' || panel === 'updates') && preferences" :tab="panel" :audio="preferences.audio" :level="level" :speaking="!!snapshot && speakers.includes(snapshot.selfId)" :connected="inVoice"
      :updater="updater" :controls="audioControls" @tab="panel = $event" @close="closePanel" />
    <Dialog v-else-if="panel" :key="panel" :title="panel === 'exit' && session?.isHost ? '停止大厅' : panelTitle[panel]" @close="closePanel">
      <div v-if="panel === 'connection'">
        <div role="status">
          <p v-if="detailsError" class="dialog-desc">暂时无法读取连接信息，请刷新重试。</p>
          <p v-else-if="!details" class="dialog-desc">正在读取连接信息…</p>
          <dl v-if="details || lastChatReceipt" class="connection-facts">
            <template v-if="details">
              <div><dt>连接方式</dt><dd>{{ transportLabels[details.transport] }}</dd></div>
              <div v-if="details.transport !== 'local'"><dt>到大厅主机的往返时间</dt><dd>{{ details.rttMs === null ? '暂无数据' : details.rttMs < 1 ? '小于 1 ms' : Math.round(details.rttMs) + ' ms' }}</dd></div>
            </template>
            <div v-if="lastChatReceipt"><dt>最近一条文字消息</dt>
              <dd>{{ lastChatReceipt.elapsedMs === undefined ? '等待大厅确认' : (lastChatReceipt.confirmed ? '主机已确认' : '未确认') + ' · ' + lastChatReceipt.elapsedMs + ' ms' }}</dd></div>
          </dl>
          <p v-if="details?.transport === 'local'" class="dialog-desc">大厅在这台电脑上运行。队友可在自己的连接详情中查看到你的网络延迟。</p>
        </div>
        <footer v-if="details?.transport !== 'local'" class="dialog-actions"><button class="button" :disabled="!details && !detailsError" @click="detailsRefresh++">刷新</button></footer>
      </div>
      <form v-if="panel === 'join'" @submit.prevent="enter">
        <p class="dialog-desc">{{ session?.isHost ? '加入新大厅将停止你主持的大厅，所有成员会断开连接，本次聊天记录也会清除。' : (connected ? '加入新大厅将退出当前大厅。' : '输入好友发来的大厅邀请链接。') }}</p>
        <label class="field">大厅邀请链接<input v-model.trim="invite" required spellcheck="false" placeholder="粘贴友声邀请链接" /></label>
        <p v-if="!isTauri()" class="form-error">请在友声 Windows 客户端中连接大厅。</p>
        <p v-if="error || offline" class="form-error" role="alert">{{ error || offline }}</p>
        <footer class="dialog-actions"><button class="button" type="button" :disabled="busy" @click="closePanel">取消</button><button class="button primary" :disabled="!ready || busy || !invite.trim()">{{ busy ? '正在连接…' : '加入大厅' }}</button></footer>
      </form>
      <div v-if="panel === 'invite'"><p class="dialog-desc">将邀请链接发给好友，即可加入这个大厅。</p>
        <label class="field">大厅邀请链接<input ref="copyInput" :value="shareLink" :placeholder="error ? '' : '正在准备邀请链接…'" readonly @focus="copyInput?.select()" /></label>
        <p v-if="error" class="form-error" role="alert">{{ error }}</p>
        <footer class="dialog-actions"><button class="button primary" :disabled="!shareLink" @click="copyInvite"><Copy /> 复制链接</button></footer>
      </div>
      <form v-if="panel === 'create'" @submit.prevent="createChannel"><label class="field">频道名称<input v-model.trim="channelName" required maxlength="40" placeholder="例如 排位一队" aria-describedby="channel-name-length" :aria-invalid="channelNameLength > 20" /></label>
        <p id="channel-name-length" :class="channelNameLength > 20 ? 'form-error' : 'field-hint'">{{ channelNameLength }} / 20 个字符<span v-if="channelNameLength > 20">，请缩短频道名称</span></p>
        <p v-if="error" class="form-error" role="alert">{{ error }}</p>
        <footer class="dialog-actions"><button type="button" class="button" :disabled="busy" @click="closePanel">取消</button><button class="button primary" :disabled="!ready || busy || !channelNameLength || channelNameLength > 20">创建频道</button></footer>
      </form>
      <div v-if="panel === 'delete' || panel === 'exit'"><p class="dialog-desc">{{ panel === 'delete' ? '删除「' + deletion?.name + '」后，频道成员会返回大厅，此频道的聊天记录将被清除。' : (session?.isHost ? '所有成员将断开连接，本次大厅的聊天记录将被清除。' : '当前通话将结束。你可以通过同一邀请再次加入。') }}</p>
        <p v-if="error" class="form-error" role="alert">{{ error }}</p>
        <footer class="dialog-actions"><button class="button" :disabled="busy" @click="closePanel">取消</button><button class="button danger" :disabled="busy" @click="panel === 'delete' ? deleteChannel() : leaveSession()">{{ panel === 'delete' ? '删除频道' : (session?.isHost ? '停止大厅' : '退出大厅') }}</button></footer>
      </div>
    </Dialog>
    <Transition name="notice"><div v-if="notice" class="toast" role="status"><Clipboard /> {{ notice }}</div></Transition>
  </div>
</template>
