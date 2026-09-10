/**
 * Wire shapes (camelCase, per `app/docs/API.md` §5). Parsing is deliberately
 * tolerant: unknown fields are ignored, nullable fields are typed `| null`,
 * and optional enrichments (`lastMessage`, `sender`, `replyCount`, …) may be
 * absent entirely — `undefined` is omitted from server JSON.
 */

export interface HealthResponse {
  status: string;
  uptime?: number;
}

export interface ChallengeResponse {
  challengeId: string;
  message: string;
  expiresAt: string;
}

export interface User {
  walletAddress: string;
  username: string | null;
  publicKey?: string | null;
  publicKeySig?: string | null;
  createdAt?: string | null;
  updatedAt?: string | null;
  [extra: string]: unknown;
}

export interface LoginResponse {
  user: User;
  token: string;
  fruitnationWallet?: string;
  encryptionSalt?: string;
  [extra: string]: unknown;
}

export interface Room {
  id: string;
  name: string;
  description?: string | null;
  kind?: string;
  currentKeyVersion?: number;
  keyRotationPending?: boolean;
  createdAt?: string | null;
  [extra: string]: unknown;
}

export interface RoomMemberWithUser {
  userAddress?: string;
  walletAddress?: string;
  username?: string | null;
  [extra: string]: unknown;
}

export interface RoomWithMembers extends Room {
  members?: RoomMemberWithUser[];
  admins?: string[];
  memberCount?: number;
  unreadCount?: number;
  lastReadSerial?: number;
  lastMessage?: MessageWithSender;
}

export interface Message {
  id: string;
  roomId: string;
  senderAddress: string;
  content: string;
  msgHash: string;
  isEncrypted: boolean;
  iv?: string | null;
  hmac?: string | null;
  encVer?: number;
  keyVersion?: number;
  msgType?: string;
  msgSerial: number;
  messageTimestamp: number;
  isDeleted?: boolean;
  txHash?: string | null;
  editedAt?: string | null;
  parentMessageId?: string | null;
  [extra: string]: unknown;
}

export interface MessageWithSender extends Message {
  sender?: User;
  replyCount?: number;
  lastReplyAt?: number | string;
}

export interface SendMessageBody {
  content: string;
  msgHash: string;
  isEncrypted?: boolean;
  iv?: string | null;
  hmac?: string | null;
  encVer?: number;
  keyVersion?: number;
  parentMessageId?: string;
}

export interface LoginRequestBody {
  walletAddress: string;
  challengeId: string;
  signature: string;
  username?: string;
  publicKey?: string;
  publicKeySig?: string;
}
