<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { X } from 'lucide-vue-next'

const props = withDefaults(defineProps<{ title: string; wide?: boolean; dismissible?: boolean }>(), { dismissible: true })
const emit = defineEmits<{ close: [] }>()
function dismiss() { if (props.dismissible) emit('close') }
const dialog = ref<HTMLDialogElement>()
onMounted(() => dialog.value?.showModal())
</script>

<template>
  <dialog ref="dialog" class="dialog" :class="{ wide }" aria-labelledby="dialog-title"
    @cancel.prevent="dismiss" @click="($event.target === dialog) && dismiss()">
    <div class="dialog-body">
      <header class="dialog-head"><h2 id="dialog-title">{{ title }}</h2>
        <button v-if="dismissible" class="icon-button" aria-label="关闭" @click="dismiss"><X /></button>
      </header>
      <slot />
    </div>
  </dialog>
</template>
