<script setup lang="ts">
import { computed, defineAsyncComponent, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { Activity, RefreshCw, Volume2 } from 'lucide-vue-next'
import { version } from '../../package.json'
import type { Updater } from './useUpdater'
import type { AudioControls } from './useAudioControls'
import AudioSettingsView from './AudioSettings.vue'
import Dialog from './Dialog.vue'

const ReleaseNotes = defineAsyncComponent(() => import('./ReleaseNotes.vue'))
const tabs = [
  { id: 'audio', label: '声音', icon: Volume2 },
  { id: 'updates', label: '软件更新', icon: RefreshCw },
  { id: 'diagnostics', label: '通话诊断', icon: Activity },
] as const
type Tab = typeof tabs[number]['id']
const props = defineProps<{ tab: Tab; controls: AudioControls; level: number; speaking: boolean; connected: boolean; previewError: string; updater: Updater }>()
const emit = defineEmits<{ close: []; tab: [tab: Tab] }>()
const audioForm = ref<InstanceType<typeof AudioSettingsView>>()
const content = ref<HTMLElement>()
const leaving = ref(false)
const exporting = ref(false)
const exportPath = ref('')
const exportError = ref('')
const locked = computed(() => leaving.value || props.updater.saving || props.updater.installing || exporting.value)

async function navigate(to?: Tab) {
  if (locked.value || to === props.tab) return
  leaving.value = true
  try {
    if (props.tab === 'audio' && audioForm.value && !(await audioForm.value.flush())) return
    if (to) {
      emit('tab', to)
      if (content.value) content.value.scrollTop = 0
    } else emit('close')
  } finally { leaving.value = false }
}

function toggleUpdates(event: Event) {
  const checkbox = event.target as HTMLInputElement
  const selected = checkbox.checked
  checkbox.checked = props.updater.enabled
  void props.updater.setEnabled(selected)
}

async function exportDiagnostics() {
  if (locked.value) return
  exporting.value = true
  exportPath.value = ''
  exportError.value = ''
  try {
    exportPath.value = await invoke<string>('export_call_diagnostics')
  } catch (reason) {
    const message = reason instanceof Error ? reason.message : String(reason ?? '')
    exportError.value = message || '请稍后重试。'
  } finally { exporting.value = false }
}
</script>

<template>
  <Dialog class="settings-dialog" title="软件设置" wide :dismissible="!locked" @close="navigate()">
    <div class="settings-layout">
      <div class="settings-nav" role="group" aria-label="设置分类">
        <button v-for="item in tabs" :id="`settings-${item.id}`" :key="item.id" type="button"
          aria-controls="settings-content" :aria-pressed="tab === item.id" :aria-disabled="locked" @click="navigate(item.id)">
          <component :is="item.icon" aria-hidden="true" />
          {{ item.label }}
        </button>
      </div>
      <div id="settings-content" ref="content" class="settings-content" role="region" :aria-labelledby="`settings-${tab}`" tabindex="0">
        <AudioSettingsView ref="audioForm" v-show="tab === 'audio'" :inert="leaving" :visible="tab === 'audio'" :controls="controls"
          :level="level" :speaking="speaking" :connected="connected" :preview-error="previewError" />
        <section v-if="tab === 'updates'" class="settings-section update-settings">
          <p class="section-description">友声 GroveCall · 当前版本 {{ version }}</p>
          <label class="switch-row">启动时自动检查更新<input type="checkbox" role="switch" :checked="updater.enabled" :disabled="locked" @change="toggleUpdates" /></label>
          <p class="section-description">从 GitHub 获取新版本，安装前会询问你。</p>
          <div v-if="updater.available" class="update-version">
            <h3>发现新版本 {{ updater.available.version }}</h3>
            <ReleaseNotes v-if="updater.available.notes.trim()" :notes="updater.available.notes" />
            <p class="section-description">{{ updater.installBlocked ? '请先结束当前通话并返回首页，再安装更新。' : '安装更新会关闭本机空房间，并重新启动友声。' }}</p>
          </div>
          <p v-if="updater.error" class="form-error" role="alert">{{ updater.error }}</p>
          <p class="section-description" role="status">{{ updater.installing ? '正在下载并验证更新…' : updater.checking ? '正在检查更新…' : updater.status }}</p>
          <footer class="dialog-actions">
            <button class="button" :disabled="updater.checking || locked" @click="updater.check(true)">检查更新</button>
            <button v-if="updater.available" class="button primary" :disabled="updater.installBlocked || updater.checking || locked" @click="updater.install()">下载并安装</button>
          </footer>
        </section>
        <section v-else-if="tab === 'diagnostics'" class="settings-section">
          <p class="section-description">友声在本次运行中会自动在本机内存里保留最近 30 分钟内的通话诊断。退出后会清空。</p>
          <p class="section-description">报告包含音频设备状态、语音收发和网络连接统计，排除录音、聊天内容和邀请链接。导出后文件会保存到「下载」文件夹，是否分享由你决定。</p>
          <div role="status">
            <p v-if="exporting || exportPath" class="section-description">{{ exporting ? '正在导出诊断…' : '诊断文件已保存。' }}</p>
          </div>
          <label v-if="exportPath" class="field">保存路径
            <input :value="exportPath" readonly spellcheck="false" @click="($event.target as HTMLInputElement).select()" />
          </label>
          <p v-if="exportError" class="form-error" role="alert">导出诊断失败。{{ exportError }}</p>
          <footer class="dialog-actions">
            <button type="button" class="button tinted" :disabled="locked" @click="exportDiagnostics">{{ exporting ? '正在导出…' : '导出诊断' }}</button>
          </footer>
        </section>
      </div>
    </div>
  </Dialog>
</template>

<style scoped>
.settings-dialog {
  height: min(572px, calc(100dvh - 50px));
  overflow: hidden;
}
.settings-dialog :deep(.dialog-body) {
  display: grid;
  grid-template-rows: auto minmax(0, 1fr);
  height: 100%;
  padding: 0;
}
.settings-dialog :deep(.dialog-head) {
  min-height: 64px;
  margin: 0;
  padding-inline: 24px 20px;
  border-bottom: 1px solid var(--line);
}
.settings-layout {
  display: grid;
  grid-template-columns: clamp(116px, 24%, 172px) minmax(0, 1fr);
  min-height: 0;
}
.settings-nav {
  display: flex;
  flex-direction: column;
  gap: 5px;
  overflow-y: auto;
  padding: 20px clamp(8px, 1.5vw, 14px);
  border-inline-end: 1px solid var(--line);
  background: var(--panel);
}
.settings-nav button {
  position: relative;
  display: flex;
  flex: none;
  align-items: center;
  gap: 8px;
  border-radius: 8px;
  min-height: 42px;
  padding: 8px 10px;
  color: var(--muted);
  font-size: 13px;
  font-weight: 550;
  text-align: start;
}
.settings-nav button svg {
  width: 16px;
  height: 16px;
}
.settings-nav button:hover:not([aria-disabled=true]) {
  background: var(--line);
  color: var(--ink);
}
.settings-nav button[aria-pressed=true] {
  background: var(--accent-soft);
  color: var(--accent);
  font-weight: 650;
}
.settings-nav button[aria-pressed=true]::before {
  position: absolute;
  inset-block: 12px;
  inset-inline-start: 0;
  width: 2px;
  border-radius: 2px;
  background: currentColor;
  content: '';
}
.settings-nav button[aria-disabled=true] {
  cursor: default;
  opacity: .65;
}
.settings-content {
  min-width: 0;
  overflow: auto;
  padding: clamp(16px, 3vw, 26px);
  scrollbar-gutter: stable;
  overscroll-behavior: contain;
}
.settings-content:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: -2px;
}
.settings-content .dialog-actions {
  flex-wrap: wrap;
}
.update-settings {
  padding-top: 2px;
}
.update-settings > .section-description:first-child {
  font-size: 14px;
  color: var(--ink);
  font-weight: 600;
}
.update-settings .switch-row {
  padding: 16px 0;
  margin: 12px 0 0;
  border-bottom: 1px solid var(--line);
}
.update-version {
  padding: 18px;
  border: 1px solid #ededed;
  border-radius: 12px;
  background: #fafafa;
}
.update-version h3 {
  margin: 0 0 8px;
}
</style>
