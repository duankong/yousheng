<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { Activity, ArrowDownLeft, ArrowRight, AudioLines, AudioWaveform, ChevronRight, CircleAlert, Clipboard, Copy, Headphones, House, Link2, LogOut, Menu, Plus, Radio, Settings2, Trash2, UserRound, UserRoundPlus, Users, Volume2, X } from 'lucide-vue-next'
import { invoke, isTauri } from '@tauri-apps/api/core'
import Dialog from './features/Dialog.vue'
import AudioSettingsView from './features/AudioSettings.vue'
import Chat from './features/Chat.vue'
import Members from './features/Members.vue'
import { useLobby } from './features/useLobby'
import { modes, type AudioSettings, type Channel } from './model'

type Panel = 'host' | 'join' | 'invite' | 'create' | 'delete' | 'exit' | 'audio' | ''
const panel = ref<Panel>('')
const drawer = ref(false)
const showPeople = ref(false)
const nickname = ref('')
const invite = ref('')
const channelName = ref('')
const deletion = ref<Channel>()
const notice = ref('')
const muteBusy = ref(false)
const copyInput = ref<HTMLInputElement>()
const lastJoin = ref('')
const drafts = ref<Record<string, string>>({})
let draftLobby = ''
const { preferences, snapshot, session, ready, busy, error, offline, active, voice, level, speakers, voiceLabel, inVoice, connect, leave, action } = useLobby(onInvite)
const connected = computed(() => !!(snapshot.value || session.value))
const myName = computed(() => snapshot.value?.members.find(m => m.id === snapshot.value?.selfId)?.name || preferences.value?.nickname || '本地玩家')
const heading = computed(() => active.value?.name || (connected.value ? '我的大厅' : '大厅首页'))
const banner = computed(() => (!panel.value && error.value) || offline.value || preferences.value?.shortcutError || '')
const panelTitle: Record<Panel, string> = { host: '开启大厅', join: '通过邀请加入', invite: '邀请好友', create: '创建语音频道', delete: '删除频道', exit: '退出大厅', audio: '声音设置', '': '' }
const artBars = Array.from({ length: 77 }, (_, i) => {
  const t = i / 76
  return {
    height: (16 + 118 * Math.pow(Math.sin(Math.PI * t), 0.85) * (0.28 + 0.72 * Math.pow(Math.sin(8 * t - 0.6), 2))) + 'px',
    transform: 'translateY(' + (20 * Math.sin(t * 10 + 1)) + 'px)',
    opacity: 0.48 + 0.48 * Math.sin(Math.PI * t),
  }
})
let timer: ReturnType<typeof setTimeout>
function inform(message: string) { clearTimeout(timer); notice.value = message; timer = setTimeout(() => { notice.value = '' }, 3600) }
function openPanel(next: Panel) {
  drawer.value = false
  error.value = ''
  if (next === 'host' || next === 'join') nickname.value = preferences.value?.nickname ?? '玩家'
  if (next === 'create') channelName.value = ''
  panel.value = next
}
function closePanel() { if (!busy.value) { panel.value = ''; error.value = '' } }
async function onInvite(link: string) {
  if (!link || (session.value?.invite === link && connected.value)) return
  invite.value = link
  openPanel('join')
  if (!connected.value && nickname.value.trim() && !busy.value) await enter()
}
async function enter() {
  const hosting = panel.value === 'host'
  if (busy.value || !nickname.value.trim() || (!hosting && !invite.value.trim())) return
  lastJoin.value = hosting ? '' : invite.value.trim()
  if (await connect(nickname.value.trim(), hosting ? undefined : lastJoin.value)) panel.value = ''
}
async function createChannel() {
  if (!channelName.value.trim() || busy.value) return
  if (await action({ type: 'create', name: channelName.value.trim() })) closePanel()
}
function askDelete(c: Channel) { deletion.value = c; openPanel('delete') }
async function deleteChannel() {
  if (deletion.value && await action({ type: 'delete', channelId: deletion.value.id })) closePanel()
}
async function joinChannel(id: string) {
  drawer.value = false
  if (id !== active.value?.id && !busy.value) await action({ type: 'join', channelId: id })
}
async function leaveSession() {
  if (await leave()) { panel.value = ''; lastJoin.value = ''; drafts.value = {}; draftLobby = '' }
}
async function copyInvite() {
  if (!session.value?.invite) return
  try { await navigator.clipboard.writeText(session.value.invite); inform('邀请链接已复制'); panel.value = '' }
  catch { openPanel('invite'); await nextTick(); copyInput.value?.select(); error.value = '请按 Ctrl+C 复制已选中的邀请链接。' }
}
async function mute() {
  if (!preferences.value || muteBusy.value) return
  const audio = { ...preferences.value.audio, muted: !preferences.value.audio.muted }
  muteBusy.value = true
  try { await invoke('set_audio', { audio }); preferences.value.audio = audio }
  catch (reason) { error.value = String(reason) }
  finally { muteBusy.value = false }
}
function audioSaved(audio: AudioSettings) {
  if (preferences.value) { preferences.value.audio = audio; preferences.value.shortcutError = null }
  closePanel()
  inform('声音设置已保存')
}
function dismissBanner() {
  if (error.value && !panel.value) error.value = ''
  else if (offline.value) offline.value = ''
  else if (preferences.value) preferences.value.shortcutError = null
}
watch(() => snapshot.value?.channelId, () => { showPeople.value = false })
watch(() => session.value?.invite, link => {
  if (link && link !== draftLobby) { drafts.value = {}; draftLobby = link }
})
watch(offline, message => {
  if (message && !['host', 'join', 'audio'].includes(panel.value)) panel.value = ''
})
</script>

<template>
  <div class="app-shell">
    <div v-if="drawer" class="drawer-scrim" @click="drawer = false"></div>
    <aside class="sidebar" :class="{ expanded: drawer }">
      <div class="brand"><span class="brand-mark"><AudioWaveform /></span><div><strong>队频</strong><small>DUIPIN</small></div>
        <button v-if="drawer" class="icon-button close-drawer" aria-label="关闭导航" @click="drawer = false"><X /></button>
      </div>
      <nav class="navigation" aria-label="大厅导航">
        <div class="nav-heading">工作区</div>
        <button class="nav-item" :class="{ selected: !active }" :disabled="busy" @click="drawer = false; active && action({type: 'leave'})">
          <House /><span>{{ connected ? '我的大厅' : '大厅首页' }}</span>
        </button>
        <div class="nav-heading section-label">语音频道<button v-if="snapshot" class="icon-button" title="创建频道" aria-label="创建频道" @click="openPanel('create')"><Plus /></button></div>
        <div v-if="!snapshot?.channels.length" class="nav-hint">{{ connected ? '暂无频道' : '连接大厅后显示频道' }}</div>
        <div v-for="c in snapshot?.channels" :key="c.id" class="channel-row" :class="{ selected: active?.id === c.id }">
          <button class="nav-item" :disabled="busy" :aria-current="active?.id === c.id" @click="joinChannel(c.id)">
            <Volume2 /><span>{{ c.name }}</span><small>{{ snapshot?.members.filter(m => m.channelId === c.id).length }}</small></button>
          <button v-if="c.canDelete" class="delete-channel icon-button" :aria-label="'删除' + c.name" :title="'删除' + c.name" @click="askDelete(c)"><Trash2 /></button>
        </div>
      </nav>
      <div class="sidebar-bottom">
        <button class="settings-link" :disabled="!preferences" @click="openPanel('audio')"><Settings2 /> 声音设置 <ChevronRight /></button>
        <div class="profile"><span class="profile-avatar"><UserRound /></span><span><b>{{ myName }}</b><small>{{ connected ? (session?.isHost ? '大厅管理员' : '大厅成员') : '我的身份' }}</small></span>
          <button v-if="connected" class="icon-button" :title="session?.isHost ? '停止大厅' : '退出大厅'" :aria-label="session?.isHost ? '停止大厅' : '退出大厅'" @click="openPanel('exit')"><LogOut /></button></div>
      </div>
    </aside>
    <main>
      <header class="topbar">
        <button class="icon-button mobile-menu" aria-label="展开导航" @click="drawer = true"><Menu /></button>
        <div class="breadcrumb"><House /><span>大厅</span><ChevronRight /><strong>{{ heading }}</strong></div>
        <span class="badge" :class="{ live: connected && snapshot }"><i></i>{{ connected ? (snapshot ? '大厅已连接' : '正在同步') : '未连接' }}</span>
        <button v-if="snapshot" class="icon-button people-toggle" aria-label="查看成员" :aria-expanded="showPeople" @click="showPeople = !showPeople"><Users /></button>
      </header>
      <div v-if="banner" class="alert" role="alert"><CircleAlert /><span>{{ banner }}</span>
        <button v-if="banner === offline && lastJoin && !connected" class="text-button" @click="invite = lastJoin; openPanel('join'); enter()">重试</button>
        <button v-if="banner === preferences?.shortcutError" class="text-button" @click="openPanel('audio')">声音设置</button>
        <button class="icon-button" aria-label="关闭提示" @click="dismissBanner"><X /></button></div>
      <div v-if="!connected" class="landing-scroll"><div class="landing">
        <section class="hero">
          <div class="hero-grid"></div><div class="wave-art" aria-hidden="true"><i v-for="(bar, i) in artBars" :key="i" :style="bar"></i></div>
          <div class="hero-content"><span class="hero-sign"><Radio /> 队频语音</span><h1>和队友连上线<span>。</span></h1>
            <p>开一个大厅，邀请朋友一起语音。</p></div>
        </section>
        <section class="entry-section"><h2>进入大厅</h2>
          <div class="entry-grid">
            <button class="entry-card" :disabled="isTauri() && !ready" @click="openPanel('host')">
              <span class="feature-icon hosting"><Radio /></span><strong>开启大厅</strong><span class="entry-description">由这台电脑主持大厅。</span><ArrowRight class="entry-arrow" />
            </button>
            <button class="entry-card" :disabled="isTauri() && !ready" @click="openPanel('join')">
              <span class="feature-icon joining"><Link2 /></span><strong>通过邀请加入</strong><span class="entry-description">使用好友发来的邀请链接。</span><ArrowRight class="entry-arrow" />
            </button>
          </div>
        </section>
        <section v-if="preferences" class="local-audio">
          <span class="device-icon"><Headphones /></span><div><strong>我的声音</strong><p>{{ preferences.audio.muted ? '麦克风已静音' : modes[preferences.audio.mode] }} · {{ preferences.audio.outputGain }}% 播放音量</p></div>
          <button class="button" @click="openPanel('audio')"><Settings2 /> 声音设置</button>
        </section>
      </div></div>
      <template v-else>
        <header class="room-header"><span class="room-icon"><component :is="active ? Volume2 : Radio" /></span>
          <div><h1>{{ heading }}</h1><p>{{ active ? '语音频道' : (session?.isHost ? '由你主持的语音大厅' : '语音大厅') }}</p></div>
          <button class="button primary invite-button" :disabled="!session?.invite" @click="openPanel('invite')"><UserRoundPlus /> 邀请好友</button>
        </header>
        <div v-if="snapshot" class="workspace" :class="{ 'people-open': showPeople }">
          <Chat v-if="active" :channel="active" :snapshot="snapshot" :audio="preferences?.audio" :voice-label="voiceLabel" :in-voice="inVoice"
            :drafts="drafts" @draft="(id, text) => { drafts[id] = text }"
            :can-retry-voice="['error', 'failed', 'disconnected'].includes(voice.state)" :busy="busy"
            @retry="action({ type: 'join', channelId: active.id })" @mute="mute" @settings="openPanel('audio')" @leave="action({ type: 'leave' })" @error="error = $event" />
          <section v-else class="lobby-overview"><div class="overview-title"><h2>语音频道</h2>
              <button class="button" @click="openPanel('create')"><Plus /> 创建频道</button></div>
            <p class="section-description">{{ snapshot.channels.length ? '选择频道，与频道里的队友通话和聊天。' : '创建第一个语音频道，和队友开始聊天。' }}</p>
            <div class="room-list"><button v-for="c in snapshot.channels" :key="c.id" :disabled="busy" @click="joinChannel(c.id)">
              <span class="list-icon"><Volume2 /></span><div><b>{{ c.name }}</b><small>{{ snapshot.members.filter(m => m.channelId === c.id).length }} 位成员</small></div><ArrowDownLeft />
            </button></div>
            <div v-if="!snapshot.channels.length" class="overview-empty"><span class="empty-icon"><AudioLines /></span><h3>还没有语音频道</h3></div>
          </section>
          <Members :snapshot="snapshot" :speakers="speakers" :peers="preferences?.peers ?? {}" @saved="(id, settings) => { if (preferences) preferences.peers[id] = settings }" @error="error = $event" />
        </div>
        <div v-else class="loading"><Activity /> 正在同步频道…</div>
        <p v-if="active && voice.message" class="voice-detail" role="status">{{ voice.message }}</p>
      </template>
    </main>
    <AudioSettingsView v-if="panel === 'audio' && preferences" :audio="preferences.audio" :level="level" :speaking="!!snapshot && speakers.includes(snapshot.selfId)" :connected="inVoice" @close="closePanel" @saved="audioSaved" />
    <Dialog v-else-if="panel" :key="panel" :title="panel === 'exit' && session?.isHost ? '停止大厅' : panelTitle[panel]" @close="closePanel">
      <form v-if="panel === 'host' || panel === 'join'" @submit.prevent="enter">
        <p class="dialog-desc">{{ panel === 'host' ? '大厅运行在这台电脑上，开启后即可邀请好友。' : (session?.isHost ? '加入新大厅将停止你主持的大厅，所有成员会断开连接，本次聊天记录也会清除。' : (connected ? '加入新大厅将退出当前大厅。' : '输入邀请链接和你想让队友看到的昵称。')) }}</p>
        <label class="field">我的昵称<input v-model.trim="nickname" required maxlength="32" autocomplete="nickname" placeholder="输入昵称" /></label>
        <label v-if="panel === 'join'" class="field">大厅邀请链接<input v-model.trim="invite" required spellcheck="false" placeholder="粘贴队频邀请链接" /></label>
        <p v-if="!isTauri()" class="form-error">请在队频 Windows 客户端中连接大厅。</p>
        <p v-if="error || offline" class="form-error" role="alert">{{ error || offline }}</p>
        <footer class="dialog-actions"><button class="button" type="button" :disabled="busy" @click="closePanel">取消</button><button class="button primary" :disabled="!ready || busy || !nickname.trim() || (panel === 'join' && !invite.trim())">{{ busy ? '正在连接…' : panelTitle[panel] }}</button></footer>
      </form>
      <div v-if="panel === 'invite'"><p class="dialog-desc">将邀请链接发给好友，即可加入这个大厅。</p>
        <label class="field">大厅邀请链接<input ref="copyInput" :value="session?.invite" readonly @focus="copyInput?.select()" /></label>
        <p v-if="error" class="form-error" role="alert">{{ error }}</p>
        <footer class="dialog-actions"><button class="button primary" @click="copyInvite"><Copy /> 复制链接</button></footer>
      </div>
      <form v-if="panel === 'create'" @submit.prevent="createChannel"><label class="field">频道名称<input v-model.trim="channelName" required maxlength="40" placeholder="例如 排位一队" /></label>
        <p v-if="error" class="form-error" role="alert">{{ error }}</p>
        <footer class="dialog-actions"><button type="button" class="button" :disabled="busy" @click="closePanel">取消</button><button class="button primary" :disabled="busy || !channelName.trim()">创建频道</button></footer>
      </form>
      <div v-if="panel === 'delete' || panel === 'exit'"><p class="dialog-desc">{{ panel === 'delete' ? '删除「' + deletion?.name + '」后，频道成员会返回大厅，此频道的聊天记录将被清除。' : (session?.isHost ? '所有成员将断开连接，本次大厅的聊天记录将被清除。' : '当前通话将结束。你可以通过同一邀请再次加入。') }}</p>
        <p v-if="error" class="form-error" role="alert">{{ error }}</p>
        <footer class="dialog-actions"><button class="button" :disabled="busy" @click="closePanel">取消</button><button class="button danger" :disabled="busy" @click="panel === 'delete' ? deleteChannel() : leaveSession()">{{ panel === 'delete' ? '删除频道' : (session?.isHost ? '停止大厅' : '退出大厅') }}</button></footer>
      </div>
    </Dialog>
    <div v-if="notice" class="toast" role="status"><Clipboard /> {{ notice }}</div>
  </div>
</template>
