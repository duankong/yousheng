<script setup lang="ts">
import { computed, ref } from 'vue'
import { Camera } from 'lucide-vue-next'
import { invoke } from '@tauri-apps/api/core'
import type { Preferences, ProfileUpdate } from '../model'
import Dialog from './Dialog.vue'
import PlayerAvatar from './PlayerAvatar.vue'

const props = defineProps<{ preferences: Preferences; firstRun?: boolean }>()
const emit = defineEmits<{ saved: [profile: ProfileUpdate]; close: [] }>()
const nickname = ref(props.preferences.nickname)
const avatar = ref(props.preferences.avatar ?? '')
const picker = ref<HTMLInputElement>()
const processing = ref(false)
const saving = ref(false)
const error = ref('')
const characterCount = computed(() => Array.from(nickname.value.trim()).length)
const nameValid = computed(() => characterCount.value > 0 && characterCount.value <= 32 && !/[\u0000-\u001f\u007f-\u009f]/u.test(nickname.value.trim()))

async function squarePortrait(file: File): Promise<string> {
  if (!['image/jpeg', 'image/png', 'image/webp'].includes(file.type)) {
    throw new Error('请选择 JPG、PNG 或 WebP 图片。')
  }
  if (!file.size || file.size > 8 * 1024 * 1024) throw new Error('请选择 8 MB 以内的图片。')
  // Load dimensions from the encoded image before asking the canvas to decode pixels.
  const data = await new Promise<string>((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve(String(reader.result))
    reader.onerror = () => reject(new Error('无法读取这张图片。'))
    reader.readAsDataURL(file)
  })
  const image = new Image()
  await new Promise<void>((resolve, reject) => {
    image.onload = () => resolve()
    image.onerror = () => reject(new Error('图片无法打开，请重新选择。'))
    image.src = data
  })
  const width = image.naturalWidth
  const height = image.naturalHeight
  if (!width || !height || width * height > 24_000_000 || Math.max(width, height) > 8192) {
    throw new Error('图片分辨率过大，请先裁剪后再上传。')
  }
  const edge = Math.min(width, height)
  const canvas = document.createElement('canvas')
  canvas.width = canvas.height = 96
  const ctx = canvas.getContext('2d', { alpha: false })
  if (!ctx) throw new Error('头像处理失败，请重新选择。')
  ctx.fillStyle = '#fff'
  ctx.fillRect(0, 0, 96, 96)
  ctx.imageSmoothingQuality = 'high'
  ctx.drawImage(image, (width - edge) / 2, (height - edge) / 2, edge, edge, 0, 0, 96, 96)
  for (const quality of [0.88, 0.7, 0.5, 0.3]) {
    const portrait = canvas.toDataURL('image/jpeg', quality)
    if (portrait.startsWith('data:image/jpeg;base64,') && atob(portrait.slice(portrait.indexOf(',') + 1)).length <= 8192) {
      return portrait
    }
  }
  throw new Error('头像压缩失败，请更换图片。')
}

async function choose(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  if (!file || processing.value || saving.value) return
  processing.value = true
  error.value = ''
  try { avatar.value = await squarePortrait(file) }
  catch (reason) { error.value = reason instanceof Error ? reason.message : String(reason) }
  finally { processing.value = false }
}

async function save() {
  if (!nameValid.value || processing.value || saving.value) return
  saving.value = true
  error.value = ''
  try {
    const profile = await invoke<ProfileUpdate>('set_profile', { nickname: nickname.value.trim(), avatar: avatar.value })
    emit('saved', profile)
  } catch (reason) { error.value = String(reason) }
  finally { saving.value = false }
}
</script>

<template>
  <Dialog :title="firstRun ? '设置玩家昵称' : '编辑个人资料'" :dismissible="!firstRun && !saving" @close="emit('close')">
    <form @submit.prevent="save">
      <div class="portrait-row">
        <button type="button" class="portrait-button" aria-label="更换头像" :disabled="processing || saving" @click="picker?.click()">
          <PlayerAvatar :name="nickname" :avatar="avatar" class="portrait-preview mine" person />
          <span class="portrait-camera"><Camera aria-hidden="true" /></span>
        </button>
        <div class="portrait-actions">
          <button type="button" class="text-button" :disabled="processing || saving" @click="picker?.click()">{{ processing ? '正在处理图片…' : '选择头像图片' }}</button>
          <button v-if="avatar" type="button" class="text-button reset-portrait" :disabled="processing || saving" @click="avatar = ''; error = ''">恢复默认头像</button>
        </div>
        <input ref="picker" type="file" hidden accept="image/jpeg,image/png,image/webp" aria-label="头像图片" @change="choose" />
      </div>
      <label class="field">玩家昵称
        <input v-model="nickname" type="text" autofocus autocomplete="nickname" placeholder="输入队友看到的名字" required :disabled="saving" aria-describedby="nickname-hint" />
      </label>
      <small id="nickname-hint" class="nickname-hint" :class="{ invalid: characterCount > 32 }">1–32 个字符</small>
      <p v-if="error" class="form-error" role="alert">{{ error }}</p>
      <footer class="dialog-actions">
        <button v-if="!firstRun" class="button" type="button" :disabled="saving" @click="emit('close')">取消</button>
        <button class="button primary" :disabled="!nameValid || processing || saving">{{ saving ? '正在保存…' : (firstRun ? '保存并继续' : '保存') }}</button>
      </footer>
    </form>
  </Dialog>
</template>

<style scoped>
.portrait-row { display: flex; align-items: center; gap: 16px; margin: 20px 0; }
.portrait-button { padding: 0; border: 0; border-radius: 18px; background: transparent; position: relative; }
.portrait-preview { width: 68px; height: 68px; border-radius: 18px; font-size: 24px; }
.portrait-preview :deep(svg) { width: 28px; height: 28px; }
.portrait-camera { position: absolute; right: -3px; bottom: -3px; display: grid; place-items: center; width: 24px; height: 24px; background: white; border: 1px solid var(--line); border-radius: 50%; color: var(--accent); }
.portrait-camera svg { width: 14px; height: 14px; }
.portrait-actions { display: grid; justify-items: start; gap: 6px; }
.reset-portrait, .nickname-hint { color: var(--muted); }
.nickname-hint { display: block; margin-top: -6px; font-size: 12px; }
.nickname-hint.invalid { color: var(--danger); }
</style>
