export interface Member {
  id: string
  name: string
  avatar?: string
}

export interface Snapshot {
  type: 'snapshot'
  roomProtocol: 2
  roomId: string
  roomName: string
  ownerId: string
  ownerNickname: string
  selfId: string
  members: Member[]
  messages: { id: string; authorId: string; authorName: string; text: string; time: number }[]
}

export type LobbyEvent = (Snapshot | { type: 'error' | 'offline'; message: string }) & { sessionId: string }
export interface Session { id: string; invite: string; isHost: boolean }
export interface RecentRoom { hostId: string; nickname: string; invite: string; lastJoinedAt: number }

export interface AudioSettings {
  mode: 'open' | 'activation' | 'ptt'
  muted: boolean
  outputMuted: boolean
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
export interface ProfileUpdate { nickname: string; avatar: string; profileSyncError?: string }
export interface Preferences { nickname: string; avatar?: string; audio: AudioSettings; autoUpdate: boolean; peers: Record<string, PeerSettings>; shortcutError?: string | null }
export interface Devices { inputs: { id: string; name: string }[]; outputs: { id: string; name: string }[] }
export interface VoiceStatus { state: string; message?: string }
export const modes = { open: '持续开麦', activation: '语音激活', ptt: '按键说话' } as const
