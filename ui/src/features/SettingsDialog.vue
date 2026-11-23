<script setup lang="ts">
import { computed, ref } from 'vue'
import { version } from '../../package.json'
import type { AudioSettings } from '../model'
import type { Updater } from './useUpdater'
import AudioSettingsView from './AudioSettings.vue'
import Dialog from './Dialog.vue'

const props = defineProps<{ tab: 'audio' | 'updates'; audio: AudioSettings; level: number; speaking: boolean; connected: boolean; updater: Updater }>()
const emit = defineEmits<{ close: []; tab: [tab: 'audio' | 'updates']; saved: [audio: AudioSettings] }>()
const savingAudio = ref(false)
const locked = computed(() => savingAudio.value || props.updater.saving || props.updater.installing)

function toggleUpdates(event: Event) {
  const checkbox = event.target as HTMLInputElement
  const selected = checkbox.checked
  checkbox.checked = props.updater.enabled
  void props.updater.setEnabled(selected)
}
</script>

<template>
  <Dialog title="软件设置" wide :dismissible="!locked" @close="emit('close')">
    <div class="segmented" role="group" aria-label="设置分类">
      <button type="button" :aria-pressed="tab === 'audio'" :disabled="locked" @click="emit('tab', 'audio')">声音</button>
      <button type="button" :aria-pressed="tab === 'updates'" :disabled="locked" @click="emit('tab', 'updates')">软件更新</button>
    </div>
    <AudioSettingsView v-show="tab === 'audio'" :visible="tab === 'audio'" :audio="audio" :level="level" :speaking="speaking" :connected="connected"
      @close="emit('close')" @saved="emit('saved', $event)" @saving="savingAudio = $event" />
    <section v-if="tab === 'updates'" class="settings-section update-settings">
      <p class="section-description">当前版本 {{ version }}</p>
      <label class="switch-row">启动时自动检查更新<input type="checkbox" role="switch" :checked="updater.enabled" :disabled="locked" @change="toggleUpdates" /></label>
      <p class="section-description">从 GitHub 获取新版本，安装前会询问你。</p>
      <div v-if="updater.available" class="update-version">
        <h3>发现新版本 {{ updater.available.version }}</h3>
        <p v-if="updater.available.notes" class="update-notes">{{ updater.available.notes }}</p>
        <p class="section-description">{{ updater.installBlocked ? '请先退出或停止大厅，再安装更新。' : '安装更新会关闭友声。' }}</p>
      </div>
      <p v-if="updater.error" class="form-error" role="alert">{{ updater.error }}</p>
      <p class="section-description" role="status">{{ updater.installing ? '正在下载并验证更新…' : updater.checking ? '正在检查更新…' : updater.status }}</p>
      <footer class="dialog-actions">
        <button class="button" :disabled="updater.checking || locked" @click="updater.check(true)">检查更新</button>
        <button v-if="updater.available" class="button primary" :disabled="updater.installBlocked || updater.checking || locked" @click="updater.install()">下载并安装</button>
      </footer>
    </section>
  </Dialog>
</template>
