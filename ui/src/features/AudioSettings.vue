<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { Mic, Volume2, AudioWaveform } from 'lucide-vue-next'
import { modes, type AudioSettings, type Devices } from '../model'
import type { AudioControls } from './useAudioControls'

const props = defineProps<{ audio: AudioSettings; controls: AudioControls; level: number; speaking: boolean; connected: boolean; visible: boolean }>()
const draft = computed(() => ({ ...props.audio, ...props.controls.changes }))
const shortcutEdit = ref<string | null>(null)
const devices = ref<Devices>({ inputs: [], outputs: [] })
const error = ref('')
const monitoring = ref(false)
const monitorError = ref('')
const modeKeys = Object.keys(modes) as (keyof typeof modes)[]
const meter = computed(() => Math.max(0, Math.min(100, props.level)))
const meterStatus = computed(() => {
  if (draft.value.inputDevice !== props.audio.inputDevice || draft.value.inputGain !== props.audio.inputGain ||
      draft.value.mode !== props.audio.mode || draft.value.threshold !== props.audio.threshold) return '正在应用输入设置'
  if (monitorError.value) return '麦克风不可用'
  if (!monitoring.value) return '正在打开麦克风'
  if (draft.value.mode === 'activation') return meter.value >= draft.value.threshold ? '达到阈值' : '低于阈值'
  if (props.connected && props.speaking) return '正在发言'
  return '试音中'
})

let monitorRequest = 0
async function startMonitor() {
  const request = ++monitorRequest
  monitoring.value = false
  monitorError.value = ''
  try {
    await invoke('monitor_microphone', { active: true })
    if (request === monitorRequest) monitoring.value = true
  } catch (reason) { if (request === monitorRequest) monitorError.value = String(reason) }
}
watch(() => props.audio.inputDevice, () => {
  if (props.visible && monitorError.value) void startMonitor()
})
watch(() => props.visible, (visible, _, onCleanup) => {
  if (!visible) { void flush(); return }
  let current = true
  void startMonitor()
  onCleanup(() => {
    current = false
    ++monitorRequest
    void invoke('monitor_microphone', { active: false }).catch(() => {})
  })
  void invoke<Devices>('list_devices')
    .then(value => { if (current) devices.value = value })
    .catch(reason => { if (current) error.value = String(reason) })
}, { immediate: true })

function finishShortcut() {
  if (shortcutEdit.value === null) return
  props.controls.change('shortcut', shortcutEdit.value.trim())
  shortcutEdit.value = null
}
function flush() {
  finishShortcut()
  return props.controls.flush()
}
defineExpose({ flush })
onBeforeUnmount(() => { void flush() })
</script>

<template>
    <div>
      <p v-if="controls.error || error" class="form-error" role="alert">{{ controls.error || error }}</p>
      <section class="settings-section">
        <h3><Mic /> 麦克风</h3>
        <label class="field">输入设备
          <select :value="draft.inputDevice" @change="controls.change('inputDevice', ($event.target as HTMLSelectElement).value)">
            <option value="">系统默认设备</option>
            <option v-if="draft.inputDevice && !devices.inputs.some(d => d.id === draft.inputDevice)" :value="draft.inputDevice">{{ draft.inputDevice }}</option>
            <option v-for="d in devices.inputs.filter(d => d.id)" :key="d.id" :value="d.id">{{ d.name }}</option>
          </select>
        </label>
        <label class="range-label" for="audio-input-gain"><span>输入音量</span><output>{{ draft.inputGain }}%</output>
          <input id="audio-input-gain" :value="draft.inputGain" type="range" min="0" max="200" @input="controls.change('inputGain', ($event.target as HTMLInputElement).valueAsNumber, true)" @change="controls.flush()" />
        </label>
        <span class="field-name">发言方式</span>
        <div class="segmented" role="group" aria-label="发言方式">
          <button v-for="mode in modeKeys" :key="mode" type="button" :aria-pressed="draft.mode === mode" @click="controls.change('mode', mode)">{{ modes[mode] }}</button>
        </div>
        <label v-if="draft.mode === 'ptt'" class="field">全局发言快捷键
          <input :value="shortcutEdit ?? draft.shortcut" placeholder="例如 F8" maxlength="60" @input="shortcutEdit = ($event.target as HTMLInputElement).value" @change="finishShortcut" @keydown.enter.prevent="finishShortcut" />
        </label>
        <label v-if="draft.mode === 'activation'" class="range-label" for="audio-threshold"><span>激活阈值</span><output>{{ draft.threshold }}%</output>
          <input id="audio-threshold" :value="draft.threshold" type="range" min="0" max="100" @input="controls.change('threshold', ($event.target as HTMLInputElement).valueAsNumber, true)" @change="controls.flush()" />
        </label>
        <div class="level-panel"><div><span><AudioWaveform /> 实时输入电平</span>
          <small>{{ meterStatus }}</small></div>
          <div class="meter" role="meter" aria-label="输入电平" :aria-valuenow="meter" aria-valuemin="0" aria-valuemax="100">
            <i :style="{ width: meter + '%' }"></i>
            <b v-if="draft.mode === 'activation'" :style="{ left: draft.threshold + '%' }"></b>
          </div>
        </div>
        <p v-if="monitorError" class="form-error" role="alert">{{ monitorError }}</p>
      </section>
      <section class="settings-section"><h3><Volume2 /> 扬声器与音效</h3>
        <label class="field">输出设备
          <select :value="draft.outputDevice" @change="controls.change('outputDevice', ($event.target as HTMLSelectElement).value)">
            <option value="">系统默认设备</option>
            <option v-if="draft.outputDevice && !devices.outputs.some(d => d.id === draft.outputDevice)" :value="draft.outputDevice">{{ draft.outputDevice }}</option>
            <option v-for="d in devices.outputs.filter(d => d.id)" :key="d.id" :value="d.id">{{ d.name }}</option>
          </select>
        </label>
        <label class="range-label" for="audio-output-gain"><span>播放音量</span><output>{{ draft.outputGain }}%</output>
          <input id="audio-output-gain" :value="draft.outputGain" type="range" min="0" max="150" @input="controls.change('outputGain', ($event.target as HTMLInputElement).valueAsNumber, true)" @change="controls.flush()" />
        </label>
        <label class="switch-row">麦克风降噪<input :checked="draft.noiseSuppression" type="checkbox" role="switch" @change="controls.change('noiseSuppression', ($event.target as HTMLInputElement).checked)" /></label>
        <label class="switch-row">回声消除<input :checked="draft.echoCancellation" type="checkbox" role="switch" @change="controls.change('echoCancellation', ($event.target as HTMLInputElement).checked)" /></label>
      </section>
    </div>
</template>

<style scoped>
.settings-section h3 {
  margin: 0 0 18px;
  font-size: 15px;
  font-weight: 650;
  letter-spacing: -.01em;
}
.settings-section + .settings-section {
  padding-top: 26px;
  margin-top: 26px;
}
.field {
  gap: 8px;
  margin: 16px 0;
}
.field select, .field input {
  min-height: 44px;
  border-color: #e4e4e4;
  border-radius: 10px;
  background: #fafafa;
  transition: border-color 160ms ease, background-color 160ms ease;
}
.field select:hover, .field input:hover {
  border-color: #bdbdbd;
  background: #fff;
}
.range-label {
  margin: 18px 0;
}
.range-label input {
  cursor: pointer;
}
.level-panel {
  margin-top: 18px;
  padding: 14px 16px;
  border-color: #ededed;
  border-radius: 12px;
  background: #fafafa;
}
.level-panel small {
  font-variant-numeric: tabular-nums;
}
.switch-row {
  min-height: 48px;
  padding: 12px 0;
  margin: 0;
}
.switch-row + .switch-row {
  border-top: 1px solid var(--line);
}
.switch-row input {
  flex: none;
  transition: background-color 160ms ease;
}
.switch-row input::after {
  transition: left 160ms ease;
}
@media (prefers-reduced-motion: reduce) {
  .switch-row input, .switch-row input::after { transition: none; }
}
</style>
