<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { AudioLines, SlidersHorizontal, VolumeX, Users, X } from 'lucide-vue-next'
import { invoke } from '@tauri-apps/api/core'
import type { Member, PeerSettings, Snapshot } from '../model'
import PlayerAvatar from './PlayerAvatar.vue'
const props = defineProps<{ snapshot: Snapshot; speakers: string[]; peers: Record<string, PeerSettings> }>()
const emit = defineEmits<{ saved: [id: string, settings: PeerSettings]; error: [error: string]; editProfile: [] }>()
const editId = ref('')
const edit = ref<PeerSettings>({ volume: 100, muted: false })
const busy = ref(false)
const visible = computed(() => props.snapshot.members.filter(m => !props.snapshot.channelId || m.channelId === props.snapshot.channelId))
watch(visible, list => { if (!list.some(m => m.id === editId.value)) editId.value = '' })
function select(member: Member) {
  editId.value = member.id === editId.value ? '' : member.id
  edit.value = { ...(props.peers[member.id] ?? { volume: 100, muted: false }) }
}
async function save() {
  const id = editId.value
  const settings = { ...edit.value }
  busy.value = true
  try {
    await invoke('set_peer', { id, ...settings })
    emit('saved', id, settings)
  } catch (reason) { emit('error', String(reason)); edit.value = { ...(props.peers[id] ?? { volume: 100, muted: false }) } }
  finally { busy.value = false }
}
</script>

<template>
  <aside class="members"><h3><Users />{{ snapshot.channelId ? '频道成员' : '大厅成员' }} <span class="number">{{ visible.length }}</span></h3>
    <ul><li v-for="member in visible" :key="member.id">
      <div class="member-row">
        <PlayerAvatar :name="member.name" :avatar="member.avatar" :editable="member.id === snapshot.selfId"
          :class="{ mine: member.id === snapshot.selfId, speaking: speakers.includes(member.id) }" @edit="emit('editProfile')" />
        <div class="member-info"><strong>{{ member.name }}</strong>
          <small v-if="speakers.includes(member.id)" class="speech"><AudioLines /> 正在发言</small>
          <small v-else-if="peers[member.id]?.muted" class="peer-muted"><VolumeX /> 本地已静音</small>
          <small v-else>{{ member.id === snapshot.selfId ? '我自己' : (snapshot.channels.find(c => c.id === member.channelId)?.name ?? '在大厅') }}</small>
        </div>
        <button v-if="member.id !== snapshot.selfId" :disabled="busy" class="peer-button" :title="'调节' + member.name + '的音量'" :aria-label="'调节' + member.name + '的音量'" :aria-expanded="editId === member.id" @click="select(member)"><SlidersHorizontal /><span>音量</span></button>
      </div>
      <div v-if="editId === member.id" class="peer-panel">
        <div class="peer-head"><b>个人收听</b><button class="icon-button" aria-label="收起音量设置" @click="editId = ''"><X /></button></div>
        <label class="range-label"><span>音量</span><output>{{ edit.volume }}%</output><input v-model.number="edit.volume" type="range" min="0" max="200" :disabled="busy" @change="save" /></label>
        <label class="switch-row">我不听此人<input v-model="edit.muted" type="checkbox" role="switch" :disabled="busy" @change="save" /></label>
      </div>
    </li></ul>
  </aside>
</template>

<style scoped>
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
  font-size: 11px;
  line-height: 1.4;
}
</style>
