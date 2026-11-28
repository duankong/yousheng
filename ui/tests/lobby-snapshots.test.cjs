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

async function createLobby() {
  const handlers = new Map()
  const openings = []
  const commands = {
    get_preferences: () => ({ nickname: '' }),
    take_invite: () => null,
    list_room_history: () => [],
    host_lobby: open,
    join_lobby: open,
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
  const exports = {}
  new Function('require', 'exports', compiled)(name => modules[name], exports)
  const lobby = exports.useLobby(() => {})
  await mounted()
  lobby.preferences.value.nickname = 'Guest'
  return {
    lobby,
    open(invite) {
      const connected = lobby.connect(invite)
      const { id, reply } = openings.at(-1)
      return {
        id,
        connected,
        accept: snapshot => reply.resolve({ id, isHost: !invite, invite: invite ?? 'local', snapshot }),
      }
    },
    emit: (sessionId, event) => handlers.get('lobby-event')({ ...event, sessionId }),
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
