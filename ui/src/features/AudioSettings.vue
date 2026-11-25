<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { Mic, Headphones, AudioWaveform } from 'lucide-vue-next'
import { modes, type AudioSettings, type Devices } from '../model'

const props = defineProps<{ audio: AudioSettings; level: number; speaking: boolean; connected: boolean; visible: boolean }>()
const emit = defineEmits<{ close: []; saved: [audio: AudioSettings]; saving: [busy: boolean] }>()
const draft = ref<AudioSettings>({ ...props.audio })
const devices = ref<Devices>({ inputs: [], outputs: [] })
const error = ref('')
const busy = ref(false)
const monitoring = ref(false)
const monitorError = ref('')
const modeKeys = Object.keys(modes) as (keyof typeof modes)[]
const meter = computed(() => Math.max(0, Math.min(100, props.level)))
const meterStatus = computed(() => {
  if (monitorError.value) return '麦克风不可用'
  if (!monitoring.value) return '正在打开麦克风'
  if (draft.value.mode === 'activation') return meter.value >= draft.value.threshold ? '达到阈值' : '低于阈值'
  if (props.connected && props.speaking) return '正在发言'
  return '试音中'
})

watch(() => props.visible, (visible, _, onCleanup) => {
  if (!visible) return
  let current = true
  monitoring.value = false
  monitorError.value = ''
  onCleanup(() => {
    current = false
    void invoke('monitor_microphone', { active: false }).catch(() => {})
  })
  void invoke('monitor_microphone', { active: true })
    .then(() => { if (current) monitoring.value = true })
    .catch(reason => { if (current) monitorError.value = String(reason) })
  void invoke<Devices>('list_devices')
    .then(value => { if (current) devices.value = value })
    .catch(reason => { if (current) error.value = String(reason) })
}, { immediate: true })

async function save() {
  busy.value = true
  emit('saving', true)
  error.value = ''
  try {
    await invoke('set_audio', { audio: { ...draft.value } })
    emit('saved', { ...draft.value })
  } catch (reason) { error.value = String(reason) }
  finally { busy.value = false; emit('saving', false) }
}
</script>

<template>
    <form @submit.prevent="save">
      <section class="settings-section">
        <h3><Mic /> 麦克风</h3>
        <label class="field">输入设备
          <select v-model="draft.inputDevice">
            <option value="">系统默认设备</option>
            <option v-if="draft.inputDevice && !devices.inputs.some(d => d.id === draft.inputDevice)" :value="draft.inputDevice">{{ draft.inputDevice }}</option>
            <option v-for="d in devices.inputs.filter(d => d.id)" :key="d.id" :value="d.id">{{ d.name }}</option>
          </select>
        </label>
        <label class="range-label"><span>输入音量</span><output>{{ draft.inputGain }}%</output>
          <input v-model.number="draft.inputGain" type="range" min="0" max="200" />
        </label>
        <span class="field-name">发言方式</span>
        <div class="segmented" role="group" aria-label="发言方式">
          <button v-for="mode in modeKeys" :key="mode" type="button" :aria-pressed="draft.mode === mode" @click="draft.mode = mode">{{ modes[mode] }}</button>
        </div>
        <label v-if="draft.mode === 'ptt'" class="field">全局发言快捷键
          <input v-model.trim="draft.shortcut" required placeholder="例如 F8" maxlength="60" />
        </label>
        <label v-if="draft.mode === 'activation'" class="range-label"><span>激活阈值</span><output>{{ draft.threshold }}%</output>
          <input v-model.number="draft.threshold" type="range" min="0" max="100" />
        </label>
        <div class="level-panel"><div><span><AudioWaveform /> 实时输入电平</span>
          <small>{{ meterStatus }}</small></div>
          <div class="meter" role="meter" aria-label="输入电平" :aria-valuenow="meter" aria-valuemin="0" aria-valuemax="100">
            <i :style="{ width: meter + '%' }"></i>
            <b v-if="draft.mode === 'activation'" :style="{ left: draft.threshold + '%' }"></b>
          </div>
        </div>
        <p v-if="draft.inputDevice !== audio.inputDevice || draft.inputGain !== audio.inputGain" class="section-description">当前电平来自已保存的设备和输入音量，保存后使用新设置。</p>
        <p v-if="monitorError" class="form-error" role="alert">{{ monitorError }}</p>
      </section>
      <section class="settings-section"><h3><Headphones /> 播放与音效</h3>
        <label class="field">输出设备
          <select v-model="draft.outputDevice">
            <option value="">系统默认设备</option>
            <option v-if="draft.outputDevice && !devices.outputs.some(d => d.id === draft.outputDevice)" :value="draft.outputDevice">{{ draft.outputDevice }}</option>
            <option v-for="d in devices.outputs.filter(d => d.id)" :key="d.id" :value="d.id">{{ d.name }}</option>
          </select>
        </label>
        <label class="range-label"><span>播放音量</span><output>{{ draft.outputGain }}%</output>
          <input v-model.number="draft.outputGain" type="range" min="0" max="150" />
        </label>
        <label class="switch-row">麦克风降噪<input v-model="draft.noiseSuppression" type="checkbox" role="switch" /></label>
        <label class="switch-row">回声消除<input v-model="draft.echoCancellation" type="checkbox" role="switch" /></label>
      </section>
      <p v-if="error" class="form-error" role="alert">{{ error }}</p>
      <footer class="dialog-actions"><button type="button" class="button" @click="emit('close')" :disabled="busy">取消</button>
        <button class="button primary" :disabled="busy">{{ busy ? '正在保存…' : '保存设置' }}</button></footer>
    </form>
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
.dialog-actions {
  margin-top: 24px;
}
@media (prefers-reduced-motion: reduce) {
  .switch-row input, .switch-row input::after { transition: none; }
}
</style>
