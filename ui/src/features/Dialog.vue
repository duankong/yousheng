<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { X } from 'lucide-vue-next'

defineProps<{ title: string; wide?: boolean }>()
const emit = defineEmits<{ close: [] }>()
const dialog = ref<HTMLDialogElement>()
onMounted(() => dialog.value?.showModal())
</script>

<template>
  <dialog ref="dialog" class="dialog" :class="{ wide }" aria-labelledby="dialog-title"
    @cancel.prevent="emit('close')" @click="($event.target === dialog) && emit('close')">
    <div class="dialog-body">
      <header class="dialog-head"><h2 id="dialog-title">{{ title }}</h2>
        <button class="icon-button" aria-label="关闭" @click="emit('close')"><X /></button>
      </header>
      <slot />
    </div>
  </dialog>
</template>
