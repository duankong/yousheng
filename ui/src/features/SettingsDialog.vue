<script setup lang="ts">
import { computed, defineAsyncComponent, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { version } from '../../package.json'
import type { Updater } from './useUpdater'
import type { AudioControls } from './useAudioControls'
import AudioSettingsView from './AudioSettings.vue'
import Dialog from './Dialog.vue'

const ReleaseNotes = defineAsyncComponent(() => import('./ReleaseNotes.vue'))
type Tab = 'audio' | 'updates' | 'diagnostics'
const props = defineProps<{ tab: Tab; controls: AudioControls; level: number; speaking: boolean; connected: boolean; updater: Updater }>()
const emit = defineEmits<{ close: []; tab: [tab: Tab] }>()
const audioForm = ref<InstanceType<typeof AudioSettingsView>>()
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
    if (to) emit('tab', to)
    else emit('close')
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
  <Dialog title="软件设置" wide :dismissible="!locked" @close="navigate()">
    <div class="segmented settings-tabs" role="group" aria-label="设置分类">
      <button type="button" :aria-pressed="tab === 'audio'" :disabled="locked" @click="navigate('audio')">声音</button>
      <button type="button" :aria-pressed="tab === 'updates'" :disabled="locked" @click="navigate('updates')">软件更新</button>
      <button type="button" :aria-pressed="tab === 'diagnostics'" :disabled="locked" @click="navigate('diagnostics')">通话诊断</button>
    </div>
    <AudioSettingsView ref="audioForm" v-show="tab === 'audio'" :inert="leaving" :visible="tab === 'audio'" :controls="controls" :level="level" :speaking="speaking" :connected="connected" />
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
  </Dialog>
</template>

<style scoped>
.settings-tabs {
  width: fit-content;
  min-width: 220px;
  margin: 0 0 28px;
  padding: 4px;
  border-radius: 12px;
  background: #f3f3f3;
}
.settings-tabs button {
  min-width: 100px;
  min-height: 34px;
  padding: 7px 16px;
  border-radius: 9px;
}
.settings-tabs button[aria-pressed=true] {
  color: var(--accent);
  box-shadow: 0 1px 4px #00000012;
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
