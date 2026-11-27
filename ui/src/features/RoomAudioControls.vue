<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from 'vue'
import { Mic, MicOff, Volume2, VolumeX } from 'lucide-vue-next'
import { audioGainLimits, type AudioControls } from './useAudioControls'

const props = defineProps<{ controls: AudioControls }>()
const audio = computed(() => props.controls.effective)
const items = [
  { gain: 'inputGain', mute: 'muted', name: '麦克风音量', muteLabel: '麦克风静音', off: '关闭麦克风', on: '取消麦克风静音', icon: Mic, mutedIcon: MicOff },
  { gain: 'outputGain', mute: 'outputMuted', name: '扬声器音量', muteLabel: '扬声器静音', off: '关闭扬声器', on: '恢复扬声器声音', icon: Volume2, mutedIcon: VolumeX },
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
  <div class="call-toggles" role="group" aria-label="通话声音控制">
    <div v-for="item in items" :key="item.gain" class="gain-control"
      @pointerenter="show(item.gain, true)" @pointerleave="leave"
      @focusin="show(item.gain)" @focusout="blur(item.gain, $event)" @keydown.esc.stop.prevent="dismiss">
      <button type="button" class="icon-button" :disabled="!audio" :aria-label="item.muteLabel"
        :aria-pressed="!!audio?.[item.mute]" :title="audio?.[item.mute] ? item.on : item.off"
        @click="audio && controls.change(item.mute, !audio[item.mute])">
        <component :is="audio?.[item.mute] ? item.mutedIcon : item.icon" aria-hidden="true" />
      </button>
      <div v-if="audio && open === item.gain" class="volume-popover">
        <div class="volume-card" :style="{ '--volume-level': audio[item.gain] / audioGainLimits[item.gain] }">
          <input :id="'call-' + item.gain" type="range" min="0" :max="audioGainLimits[item.gain]" :value="audio[item.gain]"
            :aria-label="item.name" aria-orientation="vertical" :aria-valuetext="audio[item.gain] + '%'"
            @pointerdown="startDrag"
            @pointerup="endDrag(item.gain, $event)" @pointercancel="endDrag(item.gain, $event)"
            @input="controls.change(item.gain, ($event.target as HTMLInputElement).valueAsNumber, true)" @change="controls.flush()" />
          <span class="volume-value" aria-hidden="true">{{ audio[item.gain] }}%</span>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.call-toggles { display: flex; gap: 2px; margin-inline-start: auto; }
.gain-control { position: relative; }
.icon-button { width: 36px; height: 36px; }
.gain-control:has(.volume-popover) > .icon-button[aria-pressed=false] { color: var(--accent); background: var(--accent-soft); }
.icon-button[aria-pressed=true] { color: var(--danger); background: #fbf1f2; }
.icon-button[aria-pressed=true]:hover { background: #f7e5e7; }
.volume-popover { position: absolute; z-index: 3; inset-inline: 0; bottom: 100%; padding-bottom: 5px; display: grid; justify-items: center; }
.volume-card {
  --volume-fill: color-mix(in srgb, var(--accent) 82%, white);
  --volume-track-length: 96px;
  --volume-thumb-size: 14px;
  --volume-padding: 8px;
  /* Native range thumb centers move between the inset endpoints of the track. */
  --volume-thumb-center: calc(var(--volume-thumb-size) / 2 + (var(--volume-track-length) - var(--volume-thumb-size)) * var(--volume-level));
  position: relative;
  width: 30px;
  padding: var(--volume-padding) 0;
  border: 1px solid var(--line);
  border-radius: 11px;
  background: #fff;
  box-shadow: 0 3px 10px #44365014;
}
.volume-card input {
  display: block;
  width: 100%;
  height: var(--volume-track-length);
  margin: 0;
  appearance: none;
  writing-mode: vertical-lr;
  direction: rtl;
  background: transparent;
  cursor: pointer;
}
.volume-card input::-webkit-slider-runnable-track {
  width: 4px;
  border-radius: 3px;
  background: linear-gradient(to top, var(--volume-fill) var(--volume-thumb-center), #e2dfe7 var(--volume-thumb-center));
}
.volume-card input::-webkit-slider-thumb {
  appearance: none;
  width: var(--volume-thumb-size);
  height: var(--volume-thumb-size);
  margin-left: -5px;
  border: 2px solid var(--volume-fill);
  border-radius: 50%;
  background: #fff;
  box-shadow: 0 1px 2px #44365020;
}
.volume-value {
  position: absolute;
  inset-inline-end: calc(100% + 5px);
  bottom: calc(var(--volume-padding) + var(--volume-thumb-center));
  transform: translateY(50%);
  padding: 3px 6px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: #fff;
  color: var(--accent);
  font-size: 11px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  pointer-events: none;
}
@media (forced-colors: active) {
  .volume-card input { appearance: auto; }
  .volume-card input::-webkit-slider-thumb { appearance: auto; margin-left: 0; }
}
</style>
