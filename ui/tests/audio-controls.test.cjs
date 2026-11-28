const assert = require('node:assert/strict')
const { readFileSync } = require('node:fs')
const { join } = require('node:path')
const { setImmediate } = require('node:timers/promises')
const { test } = require('node:test')
const { ModuleKind, ScriptTarget, transpileModule } = require('typescript')
const vue = require('vue')

const source = readFileSync(join(__dirname, '../src/features/useAudioControls.ts'), 'utf8')
const compiled = transpileModule(source, {
  compilerOptions: { module: ModuleKind.CommonJS, target: ScriptTarget.ES2022 },
}).outputText

function audioControls() {
  const calls = []
  let timer
  const preferences = vue.ref({ audio: {
    inputGain: 100, outputGain: 100, outputMuted: false, outputDevice: '',
  } })
  const exports = {}
  const ipc = {
    invoke(command, args) {
      const deferred = Promise.withResolvers()
      calls.push({ command, args, ...deferred })
      return deferred.promise
    },
  }
  const load = new Function('require', 'exports', 'setTimeout', 'clearTimeout', compiled)
  load(name => {
    if (name === '@tauri-apps/api/core') return ipc
    return require(name)
  }, exports, callback => { timer = callback; return 1 }, () => { timer = undefined })
  return { controls: exports.useAudioControls(preferences), calls, tick: () => timer() }
}

test('a successful final gain clears the failed intermediate gain and plays one preview', async () => {
  const { controls, calls, tick } = audioControls()
  controls.change('outputGain', 110, true)
  assert.equal(calls.length, 0)
  tick()
  await setImmediate()
  controls.change('outputGain', 120, true)
  const preview = controls.finishGain('outputGain')
  const flushed = controls.flush()
  calls[0].reject(new Error('temporary write failure'))
  await setImmediate()
  assert.equal(calls[1].args.audio.outputGain, 120)
  calls[1].resolve()
  assert.equal(await flushed, true)
  assert.equal(controls.current.outputGain, 120)
  assert.equal(controls.error, '')
  assert.deepEqual(calls.slice(2).map(({ command, args }) => [command, args]), [
    ['preview_speaker_volume', { gain: 120 }],
  ])
  calls[2].resolve()
  await preview
})

test('a later microphone drag cannot defer the speaker gain past its explicit flush', async () => {
  const { controls, calls, tick } = audioControls()
  controls.change('outputGain', 110, true)
  tick()
  await setImmediate()
  controls.change('outputGain', 120, true)
  const preview = controls.finishGain('outputGain')
  const flushed = controls.flush()
  controls.change('inputGain', 115, true)
  calls[0].resolve()
  await setImmediate()
  assert.equal(calls[1]?.args.audio.outputGain, 120)
  assert.equal(calls[1].args.audio.inputGain, 115)
  calls[1].resolve()
  assert.equal(await flushed, true)
  await setImmediate()
  assert.deepEqual(calls.slice(2).map(({ command, args }) => [command, args]), [
    ['preview_speaker_volume', { gain: 120 }],
  ])
  calls[2].resolve()
  await preview
})

test('a successful speaker gain previews even when a coalesced microphone edit failed', async () => {
  const { controls, calls, tick } = audioControls()
  controls.change('inputGain', 115, true)
  controls.change('outputGain', 110, true)
  tick()
  await setImmediate()
  assert.equal(calls[0].args.audio.inputGain, 115)
  assert.equal(calls[0].args.audio.outputGain, 110)
  controls.change('outputGain', 120, true)
  const preview = controls.finishGain('outputGain')
  const flushed = controls.flush()
  calls[0].reject(new Error('microphone settings failed'))
  await setImmediate()
  assert.equal(calls[1].args.audio.inputGain, 100)
  calls[1].resolve()
  assert.equal(await flushed, false)
  assert.equal(controls.current.outputGain, 120)
  assert.match(controls.error, /microphone settings failed/)
  assert.deepEqual(calls.slice(2).map(({ command, args }) => [command, args]), [
    ['preview_speaker_volume', { gain: 120 }],
  ])
  calls[2].reject(new Error('speaker preview failed'))
  await preview
  assert.match(controls.error, /microphone settings failed/)
})

test('a failed final gain rolls back and allows a later flush with no work', async () => {
  const { controls, calls } = audioControls()
  controls.change('outputGain', 120, true)
  const preview = controls.finishGain('outputGain')
  const flushed = controls.flush()
  await setImmediate()
  calls[0].reject(new Error('write failed'))
  assert.equal(await flushed, false)
  await preview
  assert.equal(controls.effective.outputGain, 100)
  assert.match(controls.error, /write failed/)
  assert.equal(await controls.flush(), true)
  assert.equal(calls.length, 1)
})

test('an unrelated change does not clear an unresolved microphone error', async () => {
  const { controls, calls } = audioControls()
  controls.change('inputGain', 115)
  const failed = controls.flush()
  await setImmediate()
  calls[0].reject(new Error('microphone settings failed'))
  assert.equal(await failed, false)
  controls.change('outputDevice', 'speakers')
  const changed = controls.flush()
  assert.match(controls.error, /microphone settings failed/)
  await setImmediate()
  calls[1].resolve()
  assert.equal(await changed, false)
  assert.match(controls.error, /microphone settings failed/)
  controls.change('inputGain', 115)
  const recovered = controls.flush()
  await setImmediate()
  calls[2].resolve()
  assert.equal(await recovered, true)
  assert.equal(controls.error, '')
})

test('a gain preview does not use the old device after the intended switch fails', async () => {
  const { controls, calls } = audioControls()
  controls.change('outputDevice', 'headset')
  await setImmediate()
  controls.change('outputGain', 120, true)
  const preview = controls.finishGain('outputGain')
  calls[0].reject(new Error('device switch failed'))
  await setImmediate()
  calls[1].resolve()
  await setImmediate()
  assert.equal(controls.current.outputGain, 120)
  assert.equal(controls.current.outputDevice, '')
  assert.equal(calls.length, 2)
  await preview
})

test('a newer drag supersedes a gain waiting to preview', async () => {
  const { controls, calls } = audioControls()
  controls.change('outputGain', 110, true)
  const first = controls.finishGain('outputGain')
  await setImmediate()
  controls.change('outputGain', 120, true)
  const second = controls.finishGain('outputGain')
  calls[0].resolve()
  await setImmediate()
  calls[1].resolve()
  await first
  assert.deepEqual(calls.slice(2).map(({ command, args }) => [command, args]), [
    ['preview_speaker_volume', { gain: 120 }],
  ])
  calls[2].resolve()
  await second
})

for (const [name, gain, mute] of [['zero gain', 0, false], ['muted output', 120, true]]) {
  test(`${name} suppresses the preview`, async () => {
    const { controls, calls } = audioControls()
    controls.change('outputGain', gain, true)
    if (mute) controls.change('outputMuted', true)
    const preview = controls.finishGain('outputGain')
    await setImmediate()
    calls[0].resolve()
    await preview
    assert.equal(calls.length, 1)
  })
}

test('switching output invalidates a late failure from an older preview', async () => {
  const { controls, calls } = audioControls()
  controls.change('outputGain', 120, true)
  const preview = controls.finishGain('outputGain')
  await setImmediate()
  calls[0].resolve()
  await setImmediate()
  assert.equal(calls[1].command, 'preview_speaker_volume')
  controls.change('outputDevice', 'headset')
  const switched = controls.flush()
  await setImmediate()
  calls[2].resolve()
  await switched
  calls[1].reject(new Error('preview failed'))
  await preview
  assert.equal(controls.error, '')
})
