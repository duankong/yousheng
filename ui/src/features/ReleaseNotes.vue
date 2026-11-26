<script setup lang="ts">
import { computed } from 'vue'
import MarkdownIt from 'markdown-it'

const props = defineProps<{ notes: string }>()
// Update manifests supply text. Only enable Markdown rules that produce static formatting.
const markdown = new MarkdownIt('zero', { html: false, breaks: true }).enable([
  'heading', 'lheading', 'list', 'blockquote', 'hr', 'code', 'fence',
  'newline', 'backticks', 'emphasis', 'strikethrough', 'escape',
])
const formatted = computed(() => markdown.render(props.notes))
</script>

<template>
  <div class="update-notes" role="region" aria-label="版本更新说明" tabindex="0" v-html="formatted"></div>
</template>

<style scoped>
.update-notes {
  max-height: clamp(140px, 32vh, 260px);
  overflow-y: auto;
  overflow-wrap: anywhere;
  padding-inline-end: 8px;
  margin: 12px 0;
  font-size: 13px;
  line-height: 1.65;
}
.update-notes:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
.update-notes :deep(:is(h1, h2, h3, h4, h5, h6)) { margin: 14px 0 4px; font-size: 14px; font-weight: 650; }
.update-notes :deep(p), .update-notes :deep(ul), .update-notes :deep(ol) { margin: 4px 0 8px; }
.update-notes :deep(:is(ul, ol)) { padding-inline-start: 1.5em; }
.update-notes :deep(li + li) { margin-top: 4px; }
.update-notes :deep(code) { padding: 1px 4px; border-radius: 4px; background: #ececec; font-size: .94em; }
.update-notes :deep(pre) { white-space: pre-wrap; }
.update-notes :deep(> :first-child) { margin-top: 0; }
.update-notes :deep(> :last-child) { margin-bottom: 0; }
</style>
