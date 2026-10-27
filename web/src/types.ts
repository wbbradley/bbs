// The JSON forms of proto/bbs.proto messages the web API carries: proto field
// names, absent optionals as null, enums as their numbers.

export const Kind = { Agent: 1, Human: 2 } as const;
export const Control = { Cancel: 1, SetEffort: 2 } as const;
export const BlockerStatus = { Open: 1, Answered: 2, Cancelled: 3, Superseded: 4 } as const;
export const InputStatus = { Pending: 1, Answered: 2, Declined: 3, Cancelled: 4, Stale: 5 } as const;
export const AnswerAction = { Submit: 1, Decline: 2, Cancel: 3 } as const;
export const SideStatus = {
  Open: 1,
  Completed: 2,
  Failed: 3,
  Cancelled: 4,
  Interrupted: 5,
  Ended: 6,
} as const;
export const ContextSource = { Current: 1, LastRequest: 2 } as const;
export const EffortResult = { Applied: 1, Failed: 2, Unknown: 3 } as const;

export interface Principal {
  kind: number;
  id: number;
  name: string;
  owner: string | null;
}

export interface ServerInfo {
  require_human_token: boolean;
  protocol_epoch: number | null;
  capabilities: string[];
  release: { version: string; revision: string; dirty: boolean | null } | null;
}

export interface Message {
  id: number;
  created_at: string;
  from_kind: number;
  from_name: string;
  to_group: string | null;
  to_name: string | null;
  read_at: string | null;
  body: string;
  from_owner: string | null;
  reader_owner: string | null;
  to_owner: string | null;
  control: number | null;
  timer_id: number | null;
  auto_reply: boolean | null;
}

export interface EffortChoice {
  value: string;
  label: string;
  rank: number | null;
}

export interface EffortReport {
  state: {
    supported: boolean;
    reason: string;
    choices: EffortChoice[];
    current: string | null;
  } | null;
  pending: { request_id: number; value: string } | null;
  answered: { request_id: number; result: number; detail: string } | null;
}

export interface Agent {
  id: number | null;
  name: string;
  status: string | null;
  pwd: string;
  hostname: string;
  active_at: string;
  retired: boolean;
  groups: string[];
  owner: string | null;
  last_sent_at: string | null;
  effort: EffortReport | null;
  effort_reported_at: string | null;
  last_human_conversation_at: string | null;
  blocker_count: number | null;
}

export interface Human {
  name: string;
  active_at: string;
}

export interface AgentSummary {
  agent: string;
  unread_messages: number;
  oldest_unread_message: string | null;
  unseen_stream: number;
  oldest_unseen_stream: string | null;
  last_sent_at: string | null;
  effort: EffortReport | null;
  effort_reported_at: string | null;
  last_human_conversation_at: string | null;
  blocker_count: number | null;
  agent_id: number | null;
}

export interface Group {
  name: string;
  members: string[];
  description: string | null;
  writers: { names: string[] } | null;
}

export interface Activity {
  id: number;
  created_at: string;
  agent: string;
  owner: string | null;
  record: string;
}

export interface BlockerOutcome {
  status: number;
  actor: Principal | null;
  created_at: string;
  answer: string | null;
  message_id: number | null;
}

export interface Blocker {
  id: number;
  group_id: number;
  group: string;
  creator: Principal | null;
  body: string;
  created_at: string;
  root_id: number;
  replaces: number | null;
  replaced_by: number | null;
  status: number;
  outcome: BlockerOutcome | null;
  can_answer: boolean;
  can_cancel: boolean;
}

export interface ListBlockersResponse {
  blockers: Blocker[];
  active_count: number;
  has_more: boolean;
  next_after_root_id: number | null;
}

export interface AgentNote {
  agent_id: number;
  agent: string;
  retired: boolean;
  body: string;
  revision: number;
  updated_at: string;
}

export interface InputOption {
  value: string;
  label: string | null;
  description: string | null;
}

export interface InputQuestion {
  id: string;
  header: string;
  text: string;
  options: InputOption[];
  allow_free_text: boolean;
  required: boolean;
}

export interface OwnerInput {
  request_id: number;
  status: number;
  questions: InputQuestion[];
  answers: { question_id: string; value: string }[];
  updated_at: string;
  message: string | null;
  is_blocking: boolean;
}

export interface SideRequest {
  id: number;
  question: string;
  status: number;
  answer: string | null;
  updated_at: string;
  follows: number | null;
  ended: boolean;
  conversation: number | null;
}

export interface SideReport {
  agent: string;
  pending_input: OwnerInput | null;
  usage: { used_tokens: number | null; limit_tokens: number | null; source: number } | null;
  revision: number;
  reported_at: string;
  side: SideRequest | null;
  side_supported: boolean | null;
  side_reason: string | null;
  side_followups: boolean | null;
}

export interface SearchHit {
  message: Message;
  truncated: boolean;
}

export interface BlockerMutationResponse {
  blocker: Blocker | null;
  replayed: boolean;
  message_id: number | null;
}
