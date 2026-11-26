<script setup lang="ts">
import { onBeforeUnmount, ref } from 'vue'
import { HeadphoneOff, Headphones, Mic, MicOff } from 'lucide-vue-next'
import type { AudioSettings } from '../model'
import type { AudioControls } from './useAudioControls'

defineProps<{ audio?: AudioSettings; controls: AudioControls }>()
const items = [
  { gain: 'inputGain', mute: 'muted', name: '麦克风音量', muteLabel: '麦克风静音', off: '关闭麦克风', on: '取消麦克风静音', icon: Mic, mutedIcon: MicOff, max: 200 },
  { gain: 'outputGain', mute: 'outputMuted', name: '频道音量', muteLabel: '频道声音静音', off: '关闭频道声音', on: '恢复频道声音', icon: Headphones, mutedIcon: HeadphoneOff, max: 150 },
] as const
type Gain = (typeof items)[number]['gain']
const open = ref<Gain>()
let dragging = false
let hoverTimer: ReturnType<typeof setTimeout> | undefined

function show(gain: Gain, hovering = false) {
  if (dragging) return
  clearTimeout(hoverTimer)
  // A diagonal move to the open slider briefly crosses the adjacent button.
  if (hovering && open.value && open.value !== gain) {
    hoverTimer = setTimeout(() => { open.value = gain }, 150)
  } else {
    open.value = gain
  }
}
function closeSoon(gain: Gain) {
  if (dragging || open.value !== gain) return
  clearTimeout(hoverTimer)
  hoverTimer = setTimeout(() => { open.value = undefined }, 180)
}
function leave(event: PointerEvent) {
  if (open.value && !(event.currentTarget as HTMLElement).querySelector(':focus-visible')) closeSoon(open.value)
}
function blur(gain: Gain, event: FocusEvent) {
  if (!(event.currentTarget as HTMLElement).contains(event.relatedTarget as Node | null)) closeSoon(gain)
}
function startDrag(event: PointerEvent) {
  clearTimeout(hoverTimer)
  dragging = true
  const slider = event.currentTarget as HTMLInputElement
  slider.setPointerCapture(event.pointerId)
}
function endDrag(gain: Gain, event: PointerEvent) {
  dragging = false
  const group = (event.currentTarget as HTMLElement).closest('.gain-control')
  if (!group?.contains(document.elementFromPoint(event.clientX, event.clientY))) closeSoon(gain)
}
function dismiss(event: KeyboardEvent) {
  const group = event.currentTarget as HTMLElement
  dragging = false
  group.querySelector('button')?.focus()
  clearTimeout(hoverTimer)
  open.value = undefined
}
onBeforeUnmount(() => clearTimeout(hoverTimer))
</script>

<template>
  <div class="call-toggles" role="group" aria-label="频道语音控制">
    <div v-for="item in items" :key="item.gain" class="gain-control"
      @pointerenter="show(item.gain, true)" @pointerleave="leave"
      @focusin="show(item.gain)" @focusout="blur(item.gain, $event)" @keydown.esc.stop.prevent="dismiss">
      <button type="button" class="icon-button" :disabled="!audio" :aria-label="item.muteLabel"
        :aria-pressed="!!audio?.[item.mute]" :title="audio?.[item.mute] ? item.on : item.off"
        @click="audio && controls.change(item.mute, !audio[item.mute])">
        <component :is="audio?.[item.mute] ? item.mutedIcon : item.icon" aria-hidden="true" />
      </button>
      <div v-if="audio && open === item.gain" class="volume-popover">
        <div class="volume-card">
          <div class="volume-label">
            <label :for="'call-' + item.gain">{{ item.name }}</label>
            <span aria-hidden="true">{{ audio[item.gain] }}%</span>
          </div>
          <input :id="'call-' + item.gain" type="range" min="0" :max="item.max" :value="audio[item.gain]"
            :aria-valuetext="audio[item.gain] + '%'" @pointerdown="startDrag"
            @pointerup="endDrag(item.gain, $event)" @pointercancel="endDrag(item.gain, $event)"
            @input="controls.change(item.gain, ($event.target as HTMLInputElement).valueAsNumber, true)" @change="controls.flush()" />
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.call-toggles { display: flex; gap: 2px; margin-inline-start: auto; }
.gain-control { position: relative; }
.icon-button { width: 36px; height: 36px; }
.icon-button[aria-pressed=true] { color: var(--danger); background: #fbf1f2; }
.icon-button[aria-pressed=true]:hover { background: #f7e5e7; }
.volume-popover { position: absolute; z-index: 3; inset-inline-end: 0; bottom: 100%; width: 180px; padding-bottom: 5px; }
.volume-card { padding: 12px 14px 9px; border: 1px solid var(--line); border-radius: 12px; background: #fff; box-shadow: 0 4px 16px #00000012; }
.volume-label { display: flex; align-items: baseline; justify-content: space-between; font-size: 12px; }
.volume-label label { font-weight: 600; }
.volume-label span { color: var(--muted); font-variant-numeric: tabular-nums; }
.volume-card input { display: block; width: 100%; height: 20px; margin: 5px 0 0; accent-color: var(--accent); cursor: pointer; }
</style>
