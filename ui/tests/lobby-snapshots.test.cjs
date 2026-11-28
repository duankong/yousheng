const assert = require('node:assert/strict')
const { readFileSync } = require('node:fs')
const { join } = require('node:path')
const { test } = require('node:test')
const { ModuleKind, ScriptTarget, transpileModule } = require('typescript')
const vue = require('vue')

const source = readFileSync(join(__dirname, '../src/features/useLobby.ts'), 'utf8')
const compiled = transpileModule(source, {
  compilerOptions: { module: ModuleKind.CommonJS, target: ScriptTarget.ES2022 },
}).outputText

function feature(name, modules) {
  const text = name === 'useLobby' ? compiled : transpileModule(
    readFileSync(join(__dirname, `../src/features/${name}.ts`), 'utf8'),
    { compilerOptions: { module: ModuleKind.CommonJS, target: ScriptTarget.ES2022 } },
  ).outputText
  const exports = {}
  new Function('require', 'exports', text)(name => modules[name], exports)
  return exports
}

async function createLobby(overrides = {}) {
  const handlers = new Map()
  const openings = []
  const chats = []
  const commands = {
    get_preferences: () => ({ nickname: '' }),
    take_invite: () => null,
    list_room_history: () => [],
    host_lobby: open,
    join_lobby: open,
    lobby_action(args) {
      const reply = Promise.withResolvers()
      chats.push({ args, ...reply })
      return reply.promise
    },
    leave_lobby() {},
    ...overrides,
  }
  function open({ sessionId }) {
    const reply = Promise.withResolvers()
    openings.push({ id: sessionId, reply })
    return reply.promise
  }
  let mounted
  const modules = {
    vue: { ...vue, onMounted: callback => { mounted = callback }, onBeforeUnmount() {} },
    '@tauri-apps/api/core': {
      isTauri: () => true,
      invoke: async (command, args) => commands[command](args),
    },
    '@tauri-apps/api/event': {
      listen: async (name, callback) => {
        handlers.set(name, payload => callback({ payload }))
        return () => handlers.delete(name)
      },
    },
  }
  const { useLobby } = feature('useLobby', modules)
  let preserveChat = () => {}
  const lobby = useLobby(() => {}, room => preserveChat(room))
  await mounted()
  lobby.preferences.value.nickname = 'Guest'
  return {
    lobby,
    chats,
    chat() {
      const { useChat } = feature('useChat', modules)
      const chat = useChat(lobby.session, lobby.snapshot, () => !lobby.active.value || lobby.busy.value, lobby.error)
      preserveChat = chat.preserve
      return chat
    },
    open(invite) {
      const connected = lobby.connect(invite)
      const { id, reply } = openings.at(-1)
      return {
        id,
        connected,
        accept: snapshot => reply.resolve({ id, isHost: !invite, invite: invite ?? 'local', snapshot }),
        reject: reply.reject,
      }
    },
    emit: (sessionId, event) => handlers.get('lobby-event')({ ...event, sessionId }),
    push: (name, payload) => handlers.get(name)(payload),
  }
}

function message(index) {
  return { id: String(index), authorId: 'host', authorName: 'Host', text: `message ${index}`, time: index }
}

function snapshot(messages, members = [{ id: 'host', name: 'Host', avatar: 'picture' }]) {
  return {
    type: 'snapshot', roomProtocol: 2, roomId: 'room', roomName: 'Room',
    ownerId: 'host', ownerNickname: 'Host', selfId: 'host',
    members, messages,
  }
}

test('early room events merge chat and avatars before the command returns', async () => {
  const { lobby, open, emit } = await createLobby()
  const opening = open()
  const baseline = snapshot([message(1)])
  emit(opening.id, baseline)
  emit(opening.id, {
    ...snapshot([message(2)], [{ id: 'host', name: 'Renamed' }]),
    messagesAfter: '1', retainedMessageCount: 2,
  })
  opening.accept(baseline)
  assert.equal(await opening.connected, true)
  assert.deepEqual(lobby.snapshot.value.messages, [message(1), message(2)])
  assert.deepEqual(lobby.snapshot.value.members, [{ id: 'host', name: 'Renamed', avatar: 'picture' }])
})

test('a reply-first room tolerates its full event and reuses chat on metadata-only updates', async () => {
  const { lobby, open, emit } = await createLobby()
  const opening = open()
  const baseline = snapshot([message(1)])
  opening.accept(baseline)
  assert.equal(await opening.connected, true)
  emit(opening.id, baseline)
  const messages = lobby.snapshot.value.messages
  emit(opening.id, {
    ...snapshot([], [{ id: 'host', name: 'Host', avatar: '' }]),
    messagesAfter: '1', retainedMessageCount: 1,
  })
  assert.equal(lobby.snapshot.value.messages, messages)
  assert.equal(lobby.snapshot.value.members[0].avatar, '')
})

test('chat rolls over at the Rust-supplied length and a full snapshot can resync it', async () => {
  const { lobby, open, emit } = await createLobby()
  const opening = open()
  opening.accept(snapshot(Array.from({ length: 100 }, (_, index) => message(index))))
  await opening.connected
  emit(opening.id, {
    ...snapshot([message(100), message(101)]),
    messagesAfter: '99', retainedMessageCount: 100,
  })
  assert.equal(lobby.snapshot.value.messages.length, 100)
  assert.equal(lobby.snapshot.value.messages[0].id, '2')
  assert.equal(lobby.snapshot.value.messages.at(-1).id, '101')
  emit(opening.id, { ...snapshot([message(102)]), messagesAfter: 'old', retainedMessageCount: 100 })
  assert.equal(lobby.snapshot.value.messages.at(-1).id, '101')
  emit(opening.id, snapshot([message(103)]))
  assert.deepEqual(lobby.snapshot.value.messages, [message(103)])
})

test('the old session cannot update the new one and early offline beats acceptance', async () => {
  const { lobby, open, emit } = await createLobby()
  const old = open()
  old.accept(snapshot([message(1)]))
  await old.connected
  const next = open('another-room')
  next.accept(snapshot([message(2)]))
  await next.connected
  emit(old.id, snapshot([message(3)]))
  emit(old.id, { type: 'offline', message: 'Old room closed' })
  assert.equal(lobby.session.value.id, next.id)
  assert.deepEqual(lobby.snapshot.value.messages, [message(2)])

  const lost = open()
  emit(lost.id, { type: 'offline', message: 'Closed before the reply' })
  lost.accept(snapshot([message(4)]))
  assert.equal(await lost.connected, false)
  assert.equal(lobby.offline.value, 'Closed before the reply')
  assert.equal(lobby.snapshot.value, undefined)
})

test('a cancelled join leaves the active room in place and ignores candidate events', async () => {
  const cancellation = Promise.withResolvers()
  const requested = []
  const { lobby, chat: createChat, open, emit } = await createLobby({
    cancel_join: ({ sessionId }) => { requested.push(sessionId); return cancellation.promise },
  })
  const chat = createChat()
  const current = open()
  current.accept(snapshot([message(1)], [{ id: 'host' }, { id: 'guest' }]))
  await current.connected
  chat.draft.value = 'The current room draft'
  const candidate = open('another-room')
  assert.equal(lobby.joinStatus.value, 'waiting')
  const cancelling = lobby.cancelJoin()
  assert.deepEqual(requested, [candidate.id])
  emit(candidate.id, snapshot([message(3)]))
  candidate.accept(snapshot([message(2)]))
  await vue.nextTick()
  assert.equal(lobby.session.value.id, current.id)

  cancellation.resolve(true)
  assert.equal(await cancelling, true)
  assert.equal(await candidate.connected, false)
  assert.equal(lobby.error.value, '')
  emit(candidate.id, { type: 'offline', message: 'Cancelled candidate' })
  assert.equal(lobby.session.value.id, current.id)
  assert.equal(lobby.snapshot.value.messages[0].id, '1')
  assert.equal(chat.draft.value, 'The current room draft')
  assert.deepEqual(chat.recovery.value, [])
})

test('a join continues when cancellation arrives after the room handoff', async () => {
  const { lobby, open } = await createLobby({ cancel_join: () => false })
  const current = open()
  current.accept(snapshot([]))
  await current.connected
  const candidate = open('another-room')
  assert.equal(await lobby.cancelJoin(), false)
  assert.equal(lobby.joinStatus.value, 'finishing')
  assert.equal(lobby.session.value.id, current.id)
  candidate.accept(snapshot([]))
  assert.equal(await candidate.connected, true)
  assert.equal(lobby.session.value.id, candidate.id)
  assert.equal(lobby.joinStatus.value, undefined)
})

test('explicit leave settles a waiting join and then exits the current room', async () => {
  const calls = []
  const { lobby, open } = await createLobby({
    cancel_join: ({ sessionId }) => { calls.push(['cancel', sessionId]); return true },
    leave_lobby: ({ sessionId }) => { calls.push(['leave', sessionId]) },
  })
  const current = open()
  current.accept(snapshot([]))
  await current.connected
  const candidate = open('another-room')
  const closing = lobby.leave()
  await vue.nextTick()
  assert.deepEqual(calls, [['cancel', candidate.id]])
  candidate.reject('cancelled')
  assert.equal(await closing, true)
  assert.deepEqual(calls, [['cancel', candidate.id], ['leave', current.id]])
  assert.equal(lobby.session.value, undefined)
})

test('external invites wait for settings and transitions and keep only the latest', async () => {
  const { usePendingInvite } = feature('usePendingInvite', { vue })
  const panel = vue.ref('audio')
  const transitioning = vue.ref(false)
  const current = vue.ref('duipin://join/current#key')
  const consumed = []
  const { queue } = usePendingInvite(
    () => transitioning.value || !!panel.value,
    () => [current.value],
    link => consumed.push(link),
  )
  queue('first')
  panel.value = 'updates'
  await vue.nextTick()
  queue('second')
  panel.value = 'diagnostics'
  await vue.nextTick()
  assert.deepEqual(consumed, [])

  transitioning.value = true
  panel.value = ''
  queue('latest')
  await vue.nextTick()
  assert.deepEqual(consumed, [])
  transitioning.value = false
  await vue.nextTick()
  assert.deepEqual(consumed, ['latest'])
  queue('https://example.com/#duipin://join/current#key')
  await vue.nextTick()
  assert.deepEqual(consumed, ['latest'])

  transitioning.value = true
  queue('duipin://join/current#key')
  await vue.nextTick()
  current.value = 'duipin://join/next#key'
  transitioning.value = false
  await vue.nextTick()
  assert.deepEqual(consumed, ['latest', 'duipin://join/current#key'])
})

test('cancelling a join discards a queued copy of that invite', async () => {
  const { usePendingInvite } = feature('usePendingInvite', { vue })
  const open = vue.ref(true)
  const consumed = []
  const { queue, discard } = usePendingInvite(() => open.value, () => [], link => consumed.push(link))
  queue('https://example.com/#duipin://join/cancelled#key')
  discard('duipin://join/cancelled#key')
  open.value = false
  await vue.nextTick()
  assert.deepEqual(consumed, [])
})

test('an unexpected disconnect keeps only unsent or unconfirmed originals for manual recovery', async () => {
  const { lobby, chat: createChat, open, emit, chats } = await createLobby()
  const chat = createChat()
  const current = open()
  current.accept({ ...snapshot([message(1)], [{ id: 'host' }, { id: 'guest' }]), roomName: 'Old room' })
  await current.connected

  chat.draft.value = 'confirmed'
  const confirmed = chat.sendChat()
  chats[0].resolve()
  await confirmed
  chat.draft.value = ' failed \n'
  const failed = chat.sendChat()
  chats[1].reject('No receipt')
  await failed
  chat.draft.value = 'pending'
  const pending = chat.sendChat()
  chat.draft.value = ' draft \n'
  emit(current.id, { type: 'offline', message: 'Lost connection' })
  assert.equal(lobby.snapshot.value, undefined)
  assert.deepEqual(chat.recovery.value.map(({ original, roomName, kind }) => [original, roomName, kind]), [
    [' draft \n', 'Old room', 'draft'],
    [' failed \n', 'Old room', 'unconfirmed'],
    ['pending', 'Old room', 'unconfirmed'],
  ])
  assert.equal(chat.draft.value, '')
  assert.equal(chat.outbox.value.length, 0)
  chats[2].resolve()
  await pending
  assert.deepEqual(chat.recovery.value.map(entry => entry.original), [' draft \n', ' failed \n'])

  const next = open('different-room')
  next.accept({ ...snapshot([]), roomName: 'New room' })
  await next.connected
  assert.equal(chat.draft.value, '')
  assert.equal(chat.outbox.value.length, 0)
  assert.equal(chats.length, 3)
  chat.draft.value = 'intentional leave'
  await lobby.leave()
  assert.deepEqual(chat.recovery.value.map(entry => entry.original), [' draft \n', ' failed \n'])
  assert.equal(chat.draft.value, '')
})

test('preview errors stay separate from live voice status', async () => {
  const { lobby, push } = await createLobby()
  push('voice-status', { state: 'connected' })
  push('voice-preview-error', 'Microphone unavailable')
  assert.equal(lobby.previewError.value, 'Microphone unavailable')
  assert.equal(lobby.inVoice.value, true)
  assert.deepEqual(lobby.voice.value, { state: 'connected' })
  push('voice-preview-error', '')
  assert.equal(lobby.previewError.value, '')
  assert.equal(lobby.inVoice.value, true)
})
