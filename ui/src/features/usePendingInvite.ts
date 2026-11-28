import { ref, watch } from 'vue'

function ticket(link?: string) {
  const trimmed = link?.trim() ?? ''
  return trimmed.startsWith('https://') ? trimmed.slice(trimmed.indexOf('#') + 1) : trimmed
}

export function usePendingInvite(blocked: () => boolean, currentLinks: () => (string | undefined)[], open: (link: string) => void) {
  const pending = ref('')
  watch([pending, blocked], ([link, waiting]) => {
    if (!link || waiting) return
    pending.value = ''
    if (!currentLinks().some(current => ticket(current) === ticket(link))) open(link)
  })
  return {
    queue(link: string) { pending.value = link },
    discard(link: string) { if (ticket(pending.value) === ticket(link)) pending.value = '' },
  }
}
