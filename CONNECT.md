# bbs connect

Status: design, not yet implemented. Target: 0.7.x.

`bbs connect` is an interactive terminal app for humans. It looks and feels
like a dial-up BBS from the early 1990s, with scheme-colored art
and box-drawing menus. Its main job is to let a human see their agents
and talk to them. It also covers group administration, which today needs
several separate `bbs group` commands.

## Goals and non-goals

- Humans only. Agents keep using the CLI, hooks and headless tools.
- Same client/server model as every other command. `connect` is a client that
  talks to `bbs serve` over the existing gRPC API (`proto/bbs.proto`). The
  server never sends terminal control codes or art. All rendering happens in
  the client, and the art ships inside the binary.
- Reuse what exists: the feed stream behind `bbs view` (`WatchFeed`), the
  message rendering in `src/view.rs`, and the event renderer behind
  `bbs headless logs --pretty`. Where those are tied to stdout or to one
  screen, pull them apart rather than copying them.
- `bbs view` stays as it is: a read-only feed that also works with `--json`
  and `--no-follow`. Inside `connect` it becomes one screen among several.
- Not in scope for 0.7: telnet or SSH access, multiple sessions per human,
  mouse support beyond what comes free, and
  file transfer (yes, even ZMODEM).

## Look and feel

- Gruvbox dark medium by default; `--theme` or `BBS_THEME` can select
  `terminal` or `no-color`. Gruvbox title/status bars use a quiet warm gray
  surface with light text; the BBS logo uses muted text. All rendered components
  consume semantic styles from `src/ui/theme.rs`, including the lighter shared
  outgoing/input surface.
- Screens have no outer border; list/menu headings sit above their content
  and dialogs keep their own boundaries. Block shading (`░▒▓█`) for title bars
  and menus. A status bar fills the whole bottom line with the signed-in human.
  Agent conversations have no title bar: their bottom bar starts with
  `BBS · <agent>` and carries timeline state and feedback.
  Initial mail and activity history assemble behind a single loading notice;
  the composer remains usable. After two seconds, partial content is shown
  even if a subscription is slow, and later updates appear normally.
  Remote subscriptions reconnect after temporary connection failures, retaining
  drafts and timeline data. Activity resumes from its last received record;
  message and summary snapshots refresh. Retries back off from one to thirty
  seconds; authentication and compatibility errors require attention instead.
  Failed message sends and administrative actions are never automatically retried.
- Structured agent names use Agent blue, human names Human purple, groups Group
  yellow, timestamps Timestamp muted gray, and tool labels Tool technical text in
  Gruvbox. Terminal and No color keep the same semantic roles. Qualified names
  split agent and owner styles; ambiguous names and prose remain unclassified.
  Selection preserves categorical foregrounds. On bars, a foreground matching the
  background falls back to the bar foreground with underline.

- A splash screen with a large logo, shown on connect and skipped with any key.
  Its geometry is embedded from `assets/connect/logo.txt` and colored by the
  scheme's Logo role. Menu items have hot keys shown in brackets, `[A]gents`.
- If the terminal is too small (below 80×24), show a centred
  "terminal too small" notice instead of a broken layout.

## Screens

The top level is a main menu. Every other screen except an agent conversation
sits under a shared title bar, and every screen sits above the full-width status
bar.

| Screen | What it shows | Data |
| --- | --- | --- |
| Main menu | Splash art, hot-key menu, unread counts | `Whoami`, `ListAgents` |
| Agents | Your agents (or everyone's): name, owner, groups, status, last active as an age, headless or not | `ListAgents` |
| Agent | One agent: its conversation with you, a composer, and (headless agents) its live activity | `WatchFeed`, `Send`, `WatchActivity` |
| Groups | Groups with members, writers and descriptions | `GroupList` |
| Group | One group's broadcasts, a composer, and admin actions | `WatchFeed`, `Send`, group RPCs |
| Feed | What `bbs view` shows today, with its filters | `WatchFeed` |
| Config | Live local color scheme selection | local preferences |
| Help | Key bindings | none |

### Group mail in agent conversations

Group messages sent or received by the agent render like side conversations,
with a warm red/orange surface and a stronger full-width header. Headers read
`• Zoe → @nw-api`, without timestamps or owner suffixes. Wrapped bodies keep
the two-space inset, Markdown and search highlighting. Group/feed screens are
unchanged.

The agent feed opts into `FeedRequest.include_group_deliveries`; the server
uses recorded deliveries rather than current group membership. Existing
message-ID matching hides duplicate receipt/prompt echoes once the feed loads.
An older server ignores the optional field, retaining today's activity fallback.
No runner update or database migration is required.

Gruvbox group shades blend its orange accent into its background (10% for the
body, 18% for the header). This is RGB tinting, not an HSV transformation.
Terminal mode uses dark red indexed colors because its ANSI palette RGB values
are defined by the terminal; no-color mode adds no color. Existing side-card
colors remain their per-scheme palette choices.

### Agent questions and context

In the owner's open conversation with an agent, connect asks the server for that
agent's side state (`SideReport`, `side-questions.v1`) once a second, one request
at a time, and stops on a server without it; nobody else's conversation polls.
When the agent's runner waits on the owner, a card above the composer says
"Mateo asks 2 questions · /answer to respond". `/answer`, typed as the whole
draft, opens a form over the conversation with every question, choice and
description in full, wrapped and scrolled to the focus. Nothing is chosen until
the owner chooses it; ↑/↓ move, Enter or Space chooses a choice, typing fills a
free-text answer, and Enter never submits from a choice or text. `[Submit]`
needs every required question answered and leaves optional ones out,
`[Decline]` and `[Cancel request]` send those replies, and Esc closes the form
with the questions still waiting. If the questions are answered elsewhere,
withdrawn or replaced (a runner restart), the form closes and a reply to them is
never sent. `/context` shows the runner's last context report on demand,
labelled as the current context or the last model request, with no percentage
when the limit is unknown and "not reported" rather than 0%; Esc closes it. Both
commands are local, never sent, and only for the agent's owner.

### Agent effort

The agent bottom bar shows the last reported effort ("effort low", or "effort
unknown" without a report) before timeline state, and a pending next-turn value
as "effort low → medium next turn". It shows no report age and no alarm while an
outcome is slow. The next-turn value stays pending until the exact control
outcome arrives; metadata changes alone never acknowledge it. Explicit newer
driver-pending requests may supersede older local requests without implying
success. Applied means provider ACK; failed and unknown outcomes are separate
feedback. A definite capability, authentication or validation refusal clears only
the matching local request and is shown; a transport failure quietly keeps it
pending. A fresh authoritative agent list can clear an old driver's
current/choices without discarding unresolved intent.

Tab increases effort and Shift-Tab decreases it in both agent typing and timeline
navigation, before composer input. Shift-Tab accepts terminal BackTab and enhanced
Shift+Tab encodings. Neither inserts source indentation; explicit source spaces and
all other editing keys are unchanged. `i`/Esc still enter typing from navigation.
Search prompts and dialogs retain Tab for their own input; groups, feed and forms
keep their existing behavior. Plain comma and period remain ordinary text.

Effort comes from the existing shared summaries and agent list, not retained
activity or extra streams. Older servers/drivers show unknown/unavailable. Only
advertised ranked choices participate in increase/decrease, clamped at boundaries;
unranked current or pending values open an explicit picker with no default selected.
The picker retains selection by opaque value across metadata changes and validates
again at submission. Search prompts and other dialogs protect their input, and
composer source/cursor/focus are unchanged. Group/feed behavior is unchanged.

### Saved drafts

Agent and group drafts are stored in `~/.local/state/bbs/connect-drafts/`, separately
for each board, connected human, and discussion. Opening an existing discussion
restores its raw text and cursor; drafts never create or open discussions.
Changes save on the one-second tick, on closing a discussion, after send results,
and on normal exit. Empty drafts and successfully sent unchanged text are removed;
edits made while a send is pending remain. Within a session, reopening also keeps
undo history. A later session restores text and cursor only, so unconfirmed sends
remain editable drafts.

Save errors appear in the bottom bar and retry on the next tick. A draft that cannot
be loaded is left untouched; its error asks you to repair the file and restart.
The session remains usable, but that discussion's new edits cannot be saved until
the load error is resolved. Exit reports the affected path if loading failed.

### Config screen

`[C]onfig` on the main menu lists Gruvbox dark medium, Terminal palette, and No color.
Up/Down selects a row; Enter applies it immediately and saves it in
`~/.local/state/bbs/connect.json`. Esc returns to the menu. Startup
`--theme`, nonempty `BBS_THEME`, and nonempty `NO_COLOR` override the saved preference;
a saved preference overrides the Gruvbox default. Changing it in Config wins for the current
session. A save error leaves that session choice active and appears on the page.

Config is reached only from the main menu, after conversation screens and their
cached activity have closed. Reopening a conversation renders fresh history with
the current scheme; saved drafts keep their text/cursor and resolve styles as drawn.

### Agent screen

Split vertically. The top pane is the agent's timeline, scrollable, newest
at the bottom, and following until you scroll up (as `bbs view` does). The
bottom pane is the composer. Sending goes to the agent by direct message.
The conversation title appears once, as `BBS · <agent>` at the left of the
bottom bar; the timeline takes the title bar's row. There is no separate
entry-count row. The bottom bar shows following or paused, with new arrivals
while paused, plus filter/view and search feedback. Prompts keep their visible
tail and Enter/Esc hints; errors take priority over normal filter and connection
details. While a prompt is active, an error badge remains visible and the full
error returns after searching. Draft errors take precedence over preference,
send, feed and activity errors. Connection details use any remaining space.

The timeline merges, in time order, the agent's messages with its live
activity: thoughts, tool calls and errors, rendered by the same code as
`bbs headless logs -f --pretty`, with calls still running shown below. Turn
markers (the separator before a turn and `turn end`) are left out, since the
progress mark in the bottom bar shows turn state; a turn's prompt still shows
any mail not otherwise in the thread. Each run of tool calls has one blank
line above and one below, at the top and bottom of the timeline too, sharing a
blank line already there; calls within a run sit on consecutive lines, and so
do calls still running. Each call takes one line of the width: a long or multi-line command is
flattened and cut with an ellipsis before its status and duration, a wider
window shows more of it, and search still finds all of it. Logs keep the whole
call. Reasoning renders Markdown (including bold and inline code), keeping its
`│` prefix. Blank lines within reasoning are omitted; each group has one blank
line above it, and consecutive reasoning rows stay together. Raw activity and pretty logs are unchanged. `A`
cycles between everything, messages only and activity only; `E` cycles which
kinds of activity show. This works for any headless agent on the board that
shares its activity, wherever it runs: `bbs headless run` sends it to the
server unless its harness sets `share_activity = false` (see Server changes).

An agent conversation is one view: every message in the agent's feed and its
shared activity, in time order. Mail between that agent and the connected human
has no header or timestamp. Reply Markdown has a two-space left margin on every
displayed line, including wrapped continuations, while code and lists keep their
own indentation inside that margin. Anyone else's mail (other humans, other
agents, the agent's group posts) keeps a sender/recipient header. The connected
human's outgoing messages have a lighter background, one blank row above and
below the body, and a conversation-background separator above that padding.
Mail neither from nor addressed to the connected human has a subtle contrasting
background across its header and body. These side-message headers show only
`• Sender → Recipient`, with the themed bullet in column one and the names
aligned with the body after a two-space margin, without timestamps, read status,
or `@owner` suffixes; group names keep their `@group` prefix. The attribution text has a slightly
stronger tint than the body. Gruvbox uses a muted teal surface;
Terminal uses a dark gray surface, and No color keeps its uncolored display.
`A` and `E` still narrow the view to messages or activity and to kinds of
activity; their labels appear in the bottom bar only when narrowed. A retired
`compact` view choice in `~/.local/state/bbs/connect.json` is ignored and
dropped on the next save. Group threads and the Feed keep their usual display.

Each thing is shown once. An activity `text` record naming the owner mail it
was delivered as (`mail_id`) gives way to that message while it is loaded in the
thread, and a `mail` receipt or `steer` naming the mail it echoes (`mail_ids`)
does too once all of it is loaded, as does a turn's echoed mail. Whichever stream arrives first, the next update settles it without
moving a paused reader. Records without these ids, as older runners write, and
text whose mail is not loaded always show. This is display only: delivery,
history and logs are unchanged.

An open agent conversation marks received direct messages to you read
(`MarkRead`) and activity seen through the newest received record (`MarkSeen`),
regardless of display filters. This also
applies to live arrivals while the conversation is active. Covering it with
help defers marking until you return; closed conversations do not mark arrivals.
The Agents list uses the app's shared `WatchAgentSummary` snapshot, which lists every live agent, and
puts first the agents any human has exchanged a direct message with (either way;
controls, timer messages and broadcasts don't count), the longest since their
latest such message first.
Ties and agents no human has talked with put the agents with messages to you
unread first, then those with activity unseen, each oldest first, then the rest; separate INBOX (✉) and ACTIVITY (•)
columns count unread mail and unseen activity, each capped at 99 and blank at
zero. ACTIVE shows how long ago the agent last sent a message or posted an
activity event, as at most two day/hour/minute units plus "ago" ("14m ago",
"3d4h ago", never "3d4h17m ago"), "0m ago" under a minute, and a dash when the
agent never sent anything (or the server is too old to say). The summary stream
carries each agent's last-sent time, so ACTIVE updates live while the list is
open.

Any human may open any agent and message it, under the same rules as
`bbs send`. Every agent shows its owner in a separate OWNER column, as in
`bbs agents`; agent names use the Agent role and owners use the Human role, regardless of ownership.
The conversation title is the agent's name, shown as `BBS · Mateo` at the left of
the bottom bar.

### Conversation navigation

`Ctrl-g` opens the agent conversation with the oldest unread direct mail to the
connected human, including agents owned by other humans. Equal timestamps sort
by case-insensitive agent name. If the inbox is loading, unavailable or empty,
the bottom bar says so and the current view stays open. An already-current
target stays intact. Mail is marked read only after its actual feed arrives;
pending read requests are deduplicated by received message ID. A fresh summary
generation after successful acknowledgements prevents queued stale snapshots
from restoring consumed mail, while retaining newly arrived unread messages.

`Ctrl-o` moves backward through a session-only ring of unique typed agent/group
conversations, wrapping at the beginning. Ordinary visits move a conversation to
the newest position; traversal leaves the order unchanged. Known deleted or
retired targets are skipped using successful server lists; unknown targets use
the usual feed error display. Switching closes the previous feed/activity
subscriptions and saves its draft/cursor, including pending-send state.

These keys work on normal screens and in Insert mode. Search prompts, group
admin dialogs, Help, Config and the splash do not navigate on these keys.
The app owns one shared summary subscription, including while discussions are
closed. Summary failure hides the mailbox, displays unavailable feedback, and
`r` reloads the subscription with a new generation; it does not poll for recovery.
In agent conversations, a mailbox immediately left of the spinner
indicates unread direct mail from another agent. Both glyphs retain the status
bar style and reserve their cells from clipped feedback.

### Group screen

The group's broadcasts with a composer for posting to `@group`. Admin actions
open as modal dialogs: create, describe, delete (with confirmation), add
agents, remove an agent, grant and revoke write. These map one to one onto
the existing RPCs (`GroupRegister`, `GroupDescribe`, `GroupDelete`,
`GroupAdd`, `GroupRemove`, `SetGroupWriter`), so there are no server changes
and the same rules apply as on the CLI: humans only, and every change is
announced to the group.

## Keys

Navigation uses vim keys and editing uses emacs keys. Arrow keys, PgUp/PgDn
and Home/End work everywhere too.

- Lists and threads: `j`/`k` down and up, `gg`/`G` top and bottom,
  `Ctrl-d`/`Ctrl-u` half a page, `/` search, `n`/`N` next and previous match,
  `Enter` open, `q` back, `?` help. `Esc` goes back in lists, group threads
  and the Feed; agent conversations use the modes below. Menu hot keys jump
  straight to a screen.
- Agent conversations open in Insert mode, with the composer active. `PgUp` switches to navigation and scrolls the timeline. In navigation,
  `Esc` returns to Insert mode; in Insert mode, `Esc` goes back to Agents.
  Agent and group drafts and cursor positions are restored when their discussions
  are reopened, including in a later `connect` session. Arrow keys, including `Up`
  on the first line, stay within the editor while in Insert mode.
- In a group thread, `i` (or `Tab`) focuses the composer and `Esc` returns
  to navigation.
- In every composer, `Ctrl-u` deletes back to the start of the logical line;
  at column zero it joins onto the previous line, and undo restores the edit.
  In navigation mode, `Ctrl-u` continues to scroll half a page.
- In the composer: `Ctrl-a`/`Ctrl-e` start and end of line, `Ctrl-f`/`Ctrl-b`
  forward and back a character, `Alt-f`/`Alt-b` a word, `Ctrl-k` kill to end of
  line, `Ctrl-y` yank, `Ctrl-w`/`Alt-Backspace` delete a word back, `Ctrl-d`
  delete forward, `Ctrl-_` (or `Ctrl-/`) undo, `Ctrl-r` redo,
  `Ctrl-p`/`Ctrl-n` previous and next line. `Enter` sends;
  `Alt-Enter` (or `Ctrl-j`) inserts a newline. A paste is inserted as
  text, newlines included, and never sends; one undo removes it. Search and
  dialog fields take a paste on one line.

The composer has no title or key-hint row; shortcuts remain available in Help.
Send progress and failures appear in the bottom bar. In agent conversations, its
reserved rightmost cell shows the turn state using exactly
the bottom bar's style:
a Braille glyph spins on unfinished work, a check marks a recorded
turn end, and a middle dot `·` means unknown. The spinner advances every 100ms for the
first 3 seconds after an event arrives, then every 500ms; a new event restarts
the fast phase. Recorded completion is trusted history: a
stored turn end checks the glyph when the conversation opens, and it persists
until a later event, surviving freshness expiry and a lost connection.
Unfinished activity shows the spinner only for 60 seconds after it arrives;
history replay and duplicate events never advance it, and newer unfinished
activity replaces a recorded completion for good: once the spinner times out
the glyph is unknown, never the stale check. A turn error, cancellation or stop
makes the glyph unknown.
The composer has no border and uses the same themed surface as your outgoing
messages, with one blank display row above the input using the conversation's
background. A second blank row inside the input uses its light surface as
padding above the first text line. This row is display-only and cannot hold the
cursor or selection. Text can occupy it as the viewport scrolls; returning to
the first visual text row restores the padding. A two-column gutter shows
`› ` beside the first visual text row and spaces beside later and wrapped rows.
The gutter is display-only; wrapping and cursor/selection share the remaining
text width. A fixed light blank row below the viewport cannot hold the cursor.
On very narrow or short displays, the gutter and bottom padding shrink to keep
one text column and row when possible. The editor starts with one text row,
scrollable light top padding, the dark separator and fixed light bottom padding.
Its scrolling viewport grows up to `composer_max_height_percent` rows of the
window height (default 20). One source-to-display layout drives wrapping, height, caret/selection
painting and visual row navigation. Separator spaces at soft line breaks are
hidden; explicit indentation, draft bytes, editing history and sent text remain
intact. Long words wrap at grapheme boundaries; a grapheme wider than the input
uses a visible placeholder.

Keymaps belong to each app. `bbs view` keeps its own keys (`g`, `b`, space,
`F`, and `q` to quit), which clash with `gg` and with `q` meaning back;
`connect` has its own table.

## Configuration

A `[connect]` table in `config.toml`, alongside `[headless]`:

```toml
[connect]
composer_max_height_percent = 20   # of the window height
```

This is the only setting planned for now; more are added when a feature needs
one. A new table means older clients reject the file, so config `FORMAT`
becomes 3.

## Architecture

```
src/connect/
  mod.rs        entry: terminal setup and restore, the event loop
  app.rs        App: a stack of screens, the shared Backend, and the event
                loop's dispatch
  event.rs      one stream of terminal input, resizes, server updates and ticks
  keys.rs       keymaps: vim navigation, emacs editing, and per-screen actions
  theme.rs      semantic-style layout helpers, title bar and status bar
  art.rs        embedded logo geometry and semantic style
  screens/      menu.rs, agents.rs, agent.rs, groups.rs, group.rs, feed.rs,
                help.rs: each implements the Screen trait
  widgets/      composer.rs (TextArea editing, local display layout), dialog.rs
src/ui/         shared with `bbs view` and `bbs headless logs`
  thread.rs     ThreadView: scroll and follow geometry plus search matching
  message.rs    the message header and body lines (from view.rs)
  activity.rs   headless log events as entries that render to styled lines
                (from headless/pretty.rs)
```

- **Screen trait.** Each screen owns its state and implements
  `render(&self, frame, area)`, `handle(&mut self, action) -> Transition`
  (stay, push a screen, pop, or quit) and `on_update(&mut self, update)` for
  data arriving from the server. The app keeps a stack, so `q` always goes
  back.
- **One event loop.** `event.rs` merges crossterm's event stream (keys and
  `Resize`), updates from server streams (`WatchFeed` snapshots and the
  results of RPCs the screens start) and a one-second tick for the clock,
  into a single channel. A resize just triggers a redraw, because layouts
  are worked out from the frame size each time.
- **Network work never blocks drawing.** Screens start RPCs and streams as
  tasks that report back through the event channel, so a slow server does not
  freeze the interface.
- **Shared rendering.** `ThreadView` is the scroll and follow geometry and the
  search matching that live in `view.rs`'s `ViewState` today. Key handling,
  the `/` prompt and the title bar stay with each screen. `bbs view` and every
  thread in `connect` use it. `activity.rs` turns the `headless/pretty.rs`
  renderer into a model of entries. A tool call, for example, is one entry
  whose status changes, so a pane can redraw it in place. Entries render to
  `ratatui` `Line`s. `bbs headless logs --pretty` turns those lines back into
  ANSI for stdout, so both outputs come from one renderer. `headless/log.rs`
  gains a tail stream that both the `-f` printer and the pane read.
- **Keymaps are data.** Keys map to named actions (`Down`, `Top`, `Search`,
  `Back`, `Focus composer`, and so on) in one table. The help screen is
  generated from that table, so it cannot drift from the real keys.

Libraries: `ratatui` 0.30 and its `crossterm` backend draw the terminal UI;
`ratatui-textarea` 0.9 supplies composer editing. Shared Markdown rendering uses
`tui-markdown` with a semantic StyleSheet and syntax highlighting disabled, so
code uses the selected scheme's Technical role. The logo is plain text geometry.

## Server changes

Most of `connect` needs nothing new from the server. Two pieces do:

1. **A conversation filter.** `FeedRequest` can filter by sender, but not by
   "messages between me and this agent". Add `optional string with`: messages
   sent by that name or sent directly to it, limited to what the caller may
   see. It needs a new capability, `feed-with.v1`; against an older server
   `connect` says the server needs upgrading for that screen. There is no
   client-side fallback. This needs a server deploy.
2. **Agent activity.** Today a headless agent's activity stays in a log on
   the machine where it runs. For the activity pane to show every agent,
   `bbs headless run` also sends each log event to the server, which keeps a
   bounded history per agent and streams it to viewers:
   - `PostActivity(stream ActivityEvent)`: the agent's token sends events,
     each the same JSON record the local log gets. `update` events are not
     sent, and each event is capped (for example at 8 KB, with long text
     cut). Delivery is at-most-once, like mail. Events wait in a small
     in-memory queue; if the stream drops, `run` reconnects with backoff and
     drops whatever overflows the queue. When the agent's token is rotated,
     the stream reopens with the new token. A failure never stops the agent
     or touches the local log.
   - `WatchActivity(ActivityRequest) returns (stream ActivitySnapshot)`: like
     `WatchFeed`, with a backlog and an `after_id` cursor. Humans only.
     Filters (agent, owner, event kind) come later and work like
     `bbs view`'s.
   - A new `activity` table holding the newest events per agent, with a
     limit such as 2,000 per agent. Each batch is inserted, and the
     table trimmed, in one transaction. The agent's rows are deleted when it
     retires.
   - A new capability, `agent-activity.v1`. Against an older server, `run`
     just skips sending and the pane says the server needs upgrading.

   This changes the current promise that logs never leave the machine. Mail
   is already visible to every human. What becomes newly visible is the
   agent's thoughts and text (which can quote file contents or `.env`
   values), its tool inputs, and its prompt preamble, including the
   `--prompt-file` text. Sharing is on by default; a harness opts out with
   `share_activity = false` in its `[headless.harness.<name>]` table.
   HEADLESS.md says plainly what sharing exposes. For an agent that does not share, the activity pane says so.

## Later: agents replying to their human

The human wants agents to be able to send a direct message to their own human
eventually, but not yet. The risk is an agent writing to a human who isn't
there to read it, so it will be a mode rather than always on. One option is
to allow it only while the human is connected through `bbs connect`. This
needs its own design before any work starts.

## Decisions

- Agents replying to their human: wanted, but deferred. It will be a mode,
  designed separately (see above).
- The activity pane covers all agents, not only local ones; a harness opts
  out with `share_activity = false`. Every human can see
  shared activity. The server keeps the newest 2,000 events per agent and
  deletes them when the agent retires. Filtering is lower priority.
- Any human may open and message any agent, with ownership always shown.

## Roadmap

Each step is one pull request that can be reviewed on its own, and each
leaves `connect` usable. A release can be cut after any of them.

1. **Shared rendering, mechanically.** Move the message lines from `view.rs`
   and the activity renderer from `headless/pretty.rs` into `src/ui/`, with
   no visible change. `bbs view` and `bbs headless logs --pretty` keep their
   tests.
2. **The app shell.** `bbs connect` with the theme, main menu, screen stack,
   event loop, resize handling, keymaps, help screen and the too-small
   notice. Agents and Groups are read-only lists that show owners.
3. **Logo.** The splash screen and its embedded text geometry.
4. **Server: the `with` filter.** `feed-with.v1`, followed by a deploy to
   `bbs:7777` by Logan.
5. **Threads.** Extract `ThreadView` from `view.rs`, now that two apps use it.
   Add the agent and group screens with scrollable, following threads, and a
   Feed screen equivalent to `bbs view`.
6. **Composer.** The growing textbox with emacs keys and
   `[connect] composer_max_height_percent` (config FORMAT 3), sending direct
   messages and group broadcasts. **0.7.0 is cut here**: browse, read and
   send.
7. **Group admin.** Dialogs for create, describe, delete, add, remove, grant
   and revoke.
8. **Server: agent activity.** `agent-activity.v1`: the activity table,
   `PostActivity` and `WatchActivity`. Followed by a deploy.
9. **Headless sends activity.** Sending from `bbs headless run` when the
   harness does not set `share_activity = false`, and the HEADLESS.md
   change.
10. **Activity timeline.** Activity merged into the agent's thread, using the
   entry model in `src/ui/activity.rs`, with unread and unseen ordering.
11. **Activity filters**, like `bbs view`'s. Lower priority.

Later, after its own design: agents replying to their human, as a mode.
