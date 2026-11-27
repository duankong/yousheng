<script setup lang="ts">
import { computed, ref } from 'vue'
import { UserRound } from 'lucide-vue-next'

const props = defineProps<{ name: string; avatar?: string; editable?: boolean; person?: boolean }>()
const emit = defineEmits<{ edit: [] }>()
const failedSource = ref('')
const source = computed(() => props.avatar?.startsWith('data:image/jpeg;base64,') && props.avatar !== failedSource.value ? props.avatar : '')
const initial = computed(() => Array.from(props.name.trim())[0] || '我')
</script>

<template>
  <component :is="editable ? 'button' : 'span'" class="avatar player-avatar"
    :type="editable ? 'button' : undefined" :aria-label="editable ? '编辑我的资料' : undefined"
    :title="editable ? '编辑我的资料' : undefined" :aria-hidden="editable ? undefined : true"
    @click="editable && emit('edit')">
    <img v-if="source" :src="source" alt="" @error="failedSource = source" />
    <UserRound v-else-if="person || editable" aria-hidden="true" />
    <template v-else>{{ initial }}</template>
  </component>
</template>

<style scoped>
.player-avatar { overflow: hidden; padding: 0; }
button.player-avatar { border: 0; cursor: pointer; }
button.player-avatar:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
.player-avatar img { display: block; width: 100%; height: 100%; object-fit: cover; }
</style>
