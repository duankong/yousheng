<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { AudioLines, SlidersHorizontal, UserRoundPlus, VolumeX, X } from 'lucide-vue-next'
import type { Member, PeerSettings, Snapshot } from '../model'
import type { AudioControls } from './useAudioControls'
import PlayerAvatar from './PlayerAvatar.vue'
const props = defineProps<{ snapshot: Snapshot; speakers: string[]; peers: Record<string, PeerSettings>; controls: AudioControls; busy: boolean }>()
const emit = defineEmits<{ save: [id: string, settings: PeerSettings]; editProfile: []; invite: [] }>()
const editId = ref('')
const edit = ref({ ...props.controls.peerSettings(editId.value) })
const visible = computed(() => [...props.snapshot.members].sort((a, b) => Number(b.id === props.snapshot.ownerId) - Number(a.id === props.snapshot.ownerId)))
watch(visible, list => { if (!list.some(m => m.id === editId.value)) editId.value = '' })
watch([editId, () => props.controls.peerSettings(editId.value)], ([, settings]) => { edit.value = { ...settings } })
function role(member: Member) {
  if (member.id === props.snapshot.selfId) return member.id === props.snapshot.ownerId ? '我（房主）' : '我'
  return member.id === props.snapshot.ownerId ? '房主' : '队友'
}
function select(member: Member) {
  editId.value = member.id === editId.value ? '' : member.id
}
function save() {
  emit('save', editId.value, { ...edit.value })
}
</script>

<template>
  <aside class="members" aria-labelledby="member-title">
    <header class="member-heading"><h2 id="member-title">成员</h2><span class="number" role="status">{{ visible.length }} 人</span></header>
    <button class="button tinted member-invite" :disabled="busy" aria-haspopup="dialog" @click="emit('invite')"><UserRoundPlus aria-hidden="true" />邀请好友</button>
    <ul><li v-for="member in visible" :key="member.id">
      <div class="member-row">
        <PlayerAvatar :name="member.name" :avatar="member.avatar" :editable="member.id === snapshot.selfId"
          :class="{ mine: member.id === snapshot.selfId, speaking: speakers.includes(member.id) }" @edit="emit('editProfile')" />
        <div class="member-info"><strong>{{ member.name }}</strong>
          <small v-if="speakers.includes(member.id)" class="speech"><AudioLines /> 正在发言</small>
          <small v-else-if="peers[member.id]?.muted" class="peer-muted"><VolumeX /> 本地已静音</small>
          <small v-else>{{ role(member) }}</small>
        </div>
        <button v-if="member.id !== snapshot.selfId" :disabled="busy" class="peer-button" :title="'调节' + member.name + '的音量'" :aria-label="'调节' + member.name + '的音量'" :aria-expanded="editId === member.id" @click="select(member)"><SlidersHorizontal aria-hidden="true" /><span>音量</span></button>
      </div>
      <div v-if="editId === member.id" class="peer-panel">
        <div class="peer-head"><b>个人收听</b><button class="icon-button" aria-label="收起音量设置" @click="editId = ''"><X /></button></div>
        <label class="range-label"><span>音量</span><output>{{ edit.volume }}%</output><input v-model.number="edit.volume" type="range" min="0" max="200" :disabled="busy || controls.peerSaving(member.id)" @change="save" /></label>
        <label class="switch-row">我不听此人<input v-model="edit.muted" type="checkbox" role="switch" :disabled="busy || controls.peerSaving(member.id)" @change="save" /></label>
      </div>
    </li></ul>
  </aside>
</template>

<style scoped>
.members { min-width: 0; min-height: 0; border-inline-start: 1px solid var(--line); background: #fafaf9; padding: 16px 12px; overflow: auto; }
.member-heading { display: flex; align-items: center; justify-content: space-between; gap: 6px; }
.member-heading h2 { font-size: 14px; }
.number { flex: none; border-radius: 5px; background: white; padding: 2px 6px; font-size: 12px; color: var(--muted); font-variant-numeric: tabular-nums; }
.member-invite { width: 100%; margin-top: 12px; }
.members ul { list-style: none; margin: 9px 0 0; padding: 0; }
.member-row { display: flex; align-items: center; gap: 8px; min-height: 46px; padding-block: 6px; }
.member-info { min-width: 0; flex: 1; }
.member-info strong { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 13px; }
.peer-button {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 4px;
  flex: none;
  width: auto;
  min-height: 34px;
  padding: 0 5px;
  border-radius: 8px;
  color: #626262;
  font-size: 12px;
}
.peer-button:hover:not(:disabled), .peer-button[aria-expanded="true"] {
  color: #222;
  background: #f0f0f0;
}
.member-info small {
  display: block;
  margin-top: 2px;
  color: var(--muted);
  font-size: 11px;
  line-height: 1.4;
}
.member-info small svg { width: 11px; height: 11px; vertical-align: -2px; }
.member-info small.speech { color: var(--live); }
.member-info small.peer-muted { color: var(--danger); }
.peer-button svg { width: 14px; height: 14px; }
.peer-panel { padding: 9px; margin: 2px 0 7px; border: 1px solid var(--line); border-radius: 8px; background: white; font-size: 12px; }
.peer-head { display: flex; align-items: center; justify-content: space-between; }
.peer-head .icon-button { height: 23px; width: 23px; }
.peer-head .icon-button svg { width: 13px; }
.peer-panel .range-label, .peer-panel .switch-row { font-size: 11px; }
.peer-panel .switch-row input:checked { background: var(--danger); }
</style>
