# Approval modes

Open the model picker and use its Approvals (or harness-provided Mode) section.
The selection is remembered with the chat/model configuration and applied on
the next message. Changing it replaces a live runtime before resuming the
conversation, so a previous full-access process cannot receive the new turn.

- Codex: Ask for approval (`on-request`, workspace-write, user reviewer),
  Approve for me (`on-request`, workspace-write, native `auto_review` reviewer),
  or Full access (`never`, danger-full-access). The reviewer is explicitly
  applied at thread start/resume and each turn so switching back restores user
  review. See [native auto-review](https://learn.chatgpt.com/docs/sandboxing/auto-review).
- Claude: Ask when needed (`default`), Accept edits, Plan, Only pre-approved
  tools (`dontAsk`), or Full access (`bypassPermissions`). Conditional Auto mode
  is not offered because the native connector does not discover its eligibility.
  See [Claude's native permission modes](https://code.claude.com/docs/en/permission-modes).
- ACP agents (including DeepSeek Harness): choices come from the session's
  `configOptions` category `mode`, or its legacy `modes.availableModes` list.
  Wire labels and values are preserved. A selector is shown only when multiple
  choices are advertised. Modes may also include agent/plan personas according
  to the harness. A stale choice or a rejected mode switch stops the run.
- Other connectors retain their existing behavior and receive no invented modes.

The displayed default is sent explicitly to restore it on resumed sessions.
ACP no longer automatically selects a bypass mode. Permissions requested by
the agent appear in the chat's question panel; cancellation denies them.
Unexpected permissions during ACP setup/discovery are cancelled. A native
full-access policy still belongs to the harness: if it sends an exceptional
approval request, Zeron presents it instead of silently granting it.

Voice dictation and read-aloud settings remain independent of these choices.
