export interface Channel {
  id: string
  name: string
  ownerId: string
  canDelete: boolean
}

export interface Member {
  id: string
  name: string
  avatar?: string
  channelId: string | null
}

export interface Snapshot {
  type: 'snapshot'
  selfId: string
  admin: boolean
  channels: Channel[]
  members: Member[]
  channelId: string | null
  messages: { id: string; authorId: string; authorName: string; text: string; time: number }[]
}

export type LobbyEvent = Snapshot | { type: 'error' | 'offline'; message: string }
export interface Session { invite: string; isHost: boolean }
export type Action = { type: 'create'; name: string }
  | { type: 'leave' }
  | { type: 'join' | 'delete'; channelId: string }
  | { type: 'chat'; channelId: string; text: string }

export interface AudioSettings {
  mode: 'open' | 'activation' | 'ptt'
  muted: boolean
  threshold: number
  inputGain: number
  outputGain: number
  noiseSuppression: boolean
  echoCancellation: boolean
  inputDevice: string
  outputDevice: string
  shortcut: string
}

export interface PeerSettings { volume: number; muted: boolean }
export interface Preferences { nickname: string; avatar?: string; audio: AudioSettings; peers: Record<string, PeerSettings>; shortcutError?: string | null; profileSyncError?: string }
export interface Devices { inputs: { id: string; name: string }[]; outputs: { id: string; name: string }[] }
export interface VoiceStatus { state: string; message?: string }
export const modes = { open: '持续开麦', activation: '语音激活', ptt: '按键说话' } as const
