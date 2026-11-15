# tinybot live surface

A Makepad app that shows what an agent describes: text, toggles, steppers,
text fields, readouts and rows of buttons. The description arrives over a local Unix socket, and the user's
taps wait in the app until the agent's next call collects them. The app
opens no network socket. It is tinybot's own app, not for upstream.

- `src/main.rs`: the app and its two widgets (`SurfaceList`, `ButtonRow`).
- `src/wire.rs`: the protocol's data and the check of a description.
- `src/socket.rs`: the socket thread.
- `src/bin/tb-surface.rs`: the agent's command, the package's second binary
  `tb-surface` (Rust standard library only; nothing of Makepad is linked in).
- `tb-surface.py`: the same command in Python (standard library only), for a
  host that has python3 and no build.
- `src/bin/tb-panel/`: the panel runner, the package's third binary `tb-panel`
  (Rust standard library only): [tb-panel](#tb-panel-panels-of-tries) below.
- `systemd/`: example units for the app and the runner.
- `check-release.sh`: fails on a binary built with the test feature.

## Run

```sh
cargo build --release -p makepad-example-tinybot-surface
./target/release/makepad-example-tinybot-surface            # add --remote to drive it in tests
./target/release/tb-surface set surface.json
./target/release/tb-surface wait --timeout 60
./target/release/tb-surface clear
```

The socket is `$XDG_RUNTIME_DIR/tinybot/surface.sock`. The app takes
`--socket PATH` or `TINYBOT_SURFACE_SOCK` for another place, and `tb-surface`
takes the same two.

## tb-surface: one call per turn

The agent does not hold the socket open. Each command connects, does one
thing, prints the app's answer as JSON lines and exits.

| Command | Does | Prints |
|---|---|---|
| `set FILE [--surface ID] [--rev N]` | Shows the surface in FILE (`-`: stdin) | `ack` or `err` |
| `wait [--timeout S]` | Collects the taps that wait; with none, blocks for the next one. `--timeout 0` never blocks | one `tap` line per press |
| `--runner` (before the command) | As root, with no runner connected: act on root's layer as the runner does. Without it every client's page is an agent's page | `err` and exit 1 when the role is refused |
| `clear` | Back to the idle line | `ack` |
| `status` | What is shown and how many taps wait | `status` |
| `voice` | As root: every line of stdin is the voice strip's text from then on; an empty line takes the strip away, and so does the end of the input | nothing, or `err` and exit 1 when refused |
| `talk` | As root, as the voice client: every line of stdin is sent as it is (`put`, `view`, `voice`), until the end of the input. For trying [state and views](#state-and-views) by hand | every line the app sends: answers and `event` lines; `err` and exit 1 when the role is refused |

Exit status: 0 done; 1 the app refused (the `err` line is on stdout); 2 `wait`
ended without a tap; 3 the app cannot be reached; 64 FILE cannot be read.

A turn is usually `set`, then (the same turn or a later one) `wait`, then the
next `set` or `clear`.

The binary and `tb-surface.py` take the same commands and options and give
the same lines on stdout and the same exit status. They differ in the wording
on stderr, and in how `set` passes FILE on: Python reads and rewrites it, the
binary checks it and sends its members as written (escapes, number spelling
and repeated keys included), which the app reads to the same surface.

## What a surface is

```json
{"surface":"deploy-42","rev":1,"title":"Deploy trixie-18?","items":[
  {"kind":"heading","text":"Ready to flash"},
  {"kind":"text","text":"Idle floor check passed off-cable."},
  {"kind":"toggle","id":"reboot","text":"Reboot phone after flash","value":false},
  {"kind":"divider"},
  {"kind":"row","buttons":[
    {"id":"confirm","text":"Deploy","style":"primary"},
    {"id":"cancel","text":"Cancel"}
  ]}
]}
```

- `surface` (a non-empty string) names what is being asked; `rev` (an
  integer from 1) counts its versions. `title` is optional.
- Items, top to bottom:

| `kind` | Fields | Shows |
|---|---|---|
| `heading` | `text` | a larger line |
| `text` | `text` | wrapped text |
| `toggle` | `id`, `text`, `value` (optional, true or false) | a switch |
| `stepper` | `id`, `text`, `values` (a non-empty array of strings), `value` (optional, one of them) | `‹ value ›`: one of the strings, one step per press, repeating while held |
| `number` | `id`, `text`, `value` (optional, a string) | a text field |
| `readout` | `id`, `text`, `value` (optional, a string) | a label and a monospace value on one line |
| `row` | `id` (optional), `buttons`: a non-empty array of `{id, text, style, confirm, lock, dim}` | buttons side by side, three to a line |
| `button` | `id`, `text`, `style`, `confirm`, `lock`, `dim` | a row of one button |
| `pick` | `id`, `text`, `note` (optional), `dot` (optional: `green`, `amber`, `red` or `off`), `selected` (optional, true or false) | a full-width row to pick: a status dot, the text, the note under it; the selected row is drawn as a primary button |
| `divider` | | a thin line |

- `style` is `primary`, `secondary` (the default) or `danger`; anything else
  is drawn as `secondary`.
- `confirm: true`: the first tap only arms the button, which then reads
  `tap again: <text>` in the danger style. A second tap within 3 s is the
  tap; without one the button is as before. A tap on another button also
  ends the wait. This happens in the app; the agent sees one tap or none.
- `lock: false`: a tap on this button does not lock its row, and the button
  stays live when another button has locked the row.
- `dim: true`: the button is drawn faded and still takes taps (the runner
  uses it while another button is the better answer).
- A `pick` is for a list to choose from (agents to send to, under a
  `heading` per host). A tap anywhere on its row is a tap with its `id`. It
  locks nothing and asks for no second tap; what the pick did, the agent
  shows with an `update` of the row's `text`, `note`, `dot` or `selected`
  (the row that is chosen now). The dot is a
  shape the app draws (`off` is grey; without `dot` there is none), not a
  character of a font.
- A stepper's strings are the values: the app does not read them as
  numbers, and a stepper stops at its first and last value. A number is sent
  exactly as typed; the app does not parse it or hold it to a range.
- `id` is required on toggles, steppers, numbers, readouts and buttons and
  optional elsewhere. Every id given must be unique in the surface. A heading
  or text needs one only to be changed by `update`.
- A `set` replaces the whole surface. With the same `surface` id, a toggle,
  stepper or number whose `value` is left out keeps what the user set (a
  stepper only when its new `values` still hold it); a new `surface` id
  starts each from its `value` (or off, the first of `values`, empty).

## What the app does with it

- A tapped row locks: its buttons dim, stop taking presses and a line under
  them says `<button> · working…`. It stays locked until the next `set` or
  `clear`, or until an `update` arms it. Other rows and the toggles, steppers
  and numbers stay live.
- Each tap is queued with the surface id, the rev and the value of every
  toggle, stepper and number at that moment. Taps wait whether or not a client is connected, and
  a later `set` or `clear` does not drop them: the agent sees from `surface`
  and `rev` what a tap answered.
- `clear` shows the idle line (`tinybot · idle`); the app stays full screen
  as tinybot's home.
- A description that fails the check changes nothing on screen: the last
  good surface stays as it was, locks and toggle values included.
- An `update` changes labels and readouts in place and arms rows. It leaves
  what the user set, an armed `confirm` button and the scroll position alone.
- At rest the app draws nothing and the socket thread sleeps in `poll`. Two
  timers exist, and each runs only for a moment: while a stepper's button is
  held, and while a `confirm` button is armed.

## Wire protocol (version 2)

One UTF-8 JSON object per line, in both directions. A request line is at most
64 KiB. The app speaks first.

On connect the app sends

```json
{"type":"hello","v":2,"surface":"deploy-42","rev":1,"privileged":false,"front":"agent","agent":{"surface":"deploy-42","title":"Deploy trixie-18?","from":"tb-surface (uid 1000)"},"pending":0}
```

The app holds two surfaces, root's and the agent's (see
[Two layers](#two-layers)). `surface` and `rev` are those of the client's own
layer (`null` and 0 when it has none); `pending` is the number of taps that
wait for this client; `privileged` says whether root has a page; `front` is
the layer on screen (`"root"`, `"agent"`, `"view"` or `null` for the idle
line); `agent` is the agent's page, with who set it, or `null`. While an
owner holds views ([State and views](#state-and-views)) a member
`"views":[{"name":…,"title":…,"from":"voice"}]` follows `agent`, here and in
`status` and `layers`; with none it is left out.

| Request | Answer |
|---|---|
| `{"type":"set","surface":…,"rev":…,"title":…,"items":[…]}` | `{"type":"ack","surface":…,"rev":…,"shown":B,"pending":N}` (`shown`: the client's layer is the one on screen) |
| `{"type":"update","surface":S,"rev":R,"title":…,"items":{ID:{"text":…,"value":…,"note":…,"dot":…,"selected":…},…},"arm":[ID,…]}` | `{"type":"ack","surface":S,"rev":R,"pending":N}` |
| `{"type":"clear"}` | `{"type":"ack","surface":null,"rev":0,"pending":N}` |
| `{"type":"status"}` | `{"type":"status","v":2,"surface":…,"rev":…,"privileged":…,"title":…,"items":N,"pending":N,"waiting":N}` |
| `{"type":"taps","wait":false}` | every waiting `tap` line, then `{"type":"taps","count":N,"seq":LAST}` |
| `{"type":"taps","wait":true}` | the same; with no tap waiting, the answer comes when one arrives |
| `{"type":"taps","wait":true,"layers":true}` | the same, and also answered when the layers change, with a line `{"type":"layers","privileged":…,"front":…,"agent":…}` first |
| `{"type":"show","layer":"agent"}` (or `"root"`) | the runner only: puts that layer on screen; `ack` |
| `{"type":"hello","role":"runner"}` | a root client takes the runner's role: the `hello` of root's layer |
| `{"type":"done","seq":LAST}` | `{"type":"ok","dropped":N,"pending":N}` |
| `{"type":"hello","role":"voice"}` | a root client takes the voice strip: a `hello` line |
| `{"type":"voice","text":TEXT}` | the voice client only: TEXT is the strip's text; `{"type":"ok"}` |
| `{"type":"voice"}` | the voice client only: no strip; `{"type":"ok"}` |
| `{"type":"put","state":{KEY:VALUE,…}}` | the voice client only: its values; `{"type":"ok"}` |
| `{"type":"view","name":NAME,"title":…,"tree":[…]}` | the voice client only: a view of its values; without `tree` the view is taken away; `{"type":"ok"}` |
| `{"type":"show","layer":"view","name":NAME}` | the runner only: puts that view on screen; `ack` |

An `update` is for exactly the surface and rev on screen (`stale_rev`
otherwise) and does not change the rev. `title` is a new header line. `items` names items by id: `text`
for a heading, text, readout or pick, `value` for a readout, `note`,
`dot` and `selected` for a pick. `arm` names rows to
unlock, each by the row's own id or the id of one of its buttons. Both are
optional. One id that does not fit (`bad_item`) refuses the whole request
and nothing changes. It is the cheap way to show a result or a reading: no
item is made or removed, so nothing the user set is touched.

A tap:

```json
{"type":"tap","seq":3,"surface":"deploy-42","rev":1,"id":"confirm","values":{"reboot":true,"gain":"-40Db","level":"1.2"}}
```

`values` holds every toggle (true or false), stepper and number (strings) in
item order.

`seq` counts taps from 1 for the life of the app. A tap stays queued until a
`done` names its `seq` or a later one, so a client that dies between reading
and using a tap gets it again on the next call. `tb-surface wait` sends `done`
after it has printed the taps. To time out, a waiting client closes its
connection.

A refused request is answered with `{"type":"err","code":…,"msg":…}`:

| `code` | When |
|---|---|
| `bad_json` | the line is not UTF-8 or not a JSON object |
| `bad_type` | `type` is missing or unknown |
| `bad_request` | `taps`, `done` or `update` with a wrong field |
| `bad_set` | `surface`, `rev`, `title` or `items` of a `set` is missing or of the wrong type; `surface` or `rev` of an `update` is |
| `bad_item` | an item or button is malformed, of an unknown `kind`, lacks a required `id`, or repeats an id; an `update` names an id it cannot change or arm |
| `stale_rev` | a `set` for the surface on screen with a lower `rev`; an `update` for another surface or rev than the one on screen |
| `privileged` | `show` from a client that is not the runner; the runner's role asked for by a client that is not root, or while another runner is connected; the same for the voice's role; `voice`, `put` or `view` from a client that has not got it |
| `bad_put` | `state` of a `put` is not an object; a key is malformed or not under the owner's name; a value is of no kind a value can be |
| `bad_view` | a `view` with a wrong name, a node of an unknown kind or with an unknown field, or a binding to a key that is not the owner's |
| `too_big` | a `put` or a `view` over one of the sizes in [State and views](#state-and-views) |
| `too_fast` | more than 60 `put` lines in a second on one connection: this one was not taken |
| `too_long` | a line over 64 KiB; the app then closes the connection |

A `set` with the same `surface` and `rev` as on screen is accepted again
(the surface is replaced and its rows are armed).

## Two layers

The app holds two surfaces and shows one of them:

- **root's layer**, the panel runner's: set by the one root client that
  says it is the runner, with the request `{"type":"hello","role":"runner"}`
  (answered with the `hello` of that layer; `privileged` for a client that
  is not root, and for a second root process while a runner is connected);
- **the agent's layer**, set by every other client: an agent of the app's
  own user, such as Claude Code on the phone, or one that runs as root.
  Being root is not being the runner: a root agent's page is an agent's
  page, with `(uid 0)` in its band.

Each client sets, updates and clears only its own layer, and gets only the
taps made on it (`taps`, `done` and `pending` each count one layer's taps).
So no process of the user can change or imitate the runner's page, read its
taps, or confirm them away, and the runner does the same to no agent.

- While root has a page, an agent's `set` is kept and not shown
  (`"shown":false`): it never covers root's page. Root puts it on screen
  with `show` (the runner does when the user opens it from Home). With no
  root page an agent's page comes up by itself.
- An agent's page is always under a band the app draws, which the page can
  neither style nor leave out: `agent · <comm> (uid N)`, in its own colour.
  `comm` is the name the setting process gives itself, so it can be
  anything; `agent ·` and the uid are the app's and the kernel's. With a
  root page there the band has "‹ Home", which goes back to it in the app,
  with no round trip.
- Root's first page takes the screen. When a layer is cleared the other has
  the screen, or the idle line.
- Two dots sit at the right end of the header of every page of either layer:
  `● mic` while `/proc/asound/card1/pcm1c/sub0/status` reads
  `state: RUNNING`, `● out` for `pcm0p`. They are looked at once a second
  while `/sys/kernel/tinybot_panel/state` does not read `off`; without the
  status files (CONFIG_SND_VERBOSE_PROCFS) no dot is drawn and no timer
  runs. No page can cover or draw them.

## The voice strip

One more thing can be on screen, under the page of either layer or the idle
line: a band with what a voice service hears, as it comes.

- It belongs to one connection of a root client that asks for it with
  `{"type":"hello","role":"voice"}`. A second one is refused (`privileged`)
  until the first has closed. The runner's role and the voice's are apart:
  the voice client has no page and gets no taps for the strip.
- `{"type":"voice","text":TEXT}` shows TEXT or replaces what is shown;
  `{"type":"voice"}` takes the strip away; so does closing the connection.
  Every request is answered with one line, which the client has to read.
- The band is the app's: a tag `voice` and two lines of text, the newest
  words (the end of TEXT as the band's width breaks it). Line ends and tabs
  in TEXT count as spaces, other control characters are left out, and of a
  long TEXT the last 600 bytes are kept.
- The band takes its 58 points from the foot of the page: the list above
  gets shorter and its rows stay where they are. Nothing is drawn over a
  page. The band takes no touch; a row under it is reached by scrolling.
- While a `confirm` button is armed the band neither comes nor goes: the
  button stays where it is and in sight until it is tapped or gives up
  (3 s), and the strip follows then.
- Texts may come as fast as the client likes. The band changes at most
  about eight times a second and shows the newest; nothing queues up. Each
  change is one frame.
- A text that is not replaced or sent again for 15 s is taken away by the
  app, so a client that hangs leaves no words standing. Sending the same
  text again keeps it.
- While `/sys/kernel/tinybot_panel/state` reads `off` nothing is drawn; the
  newest text shows when the panel is on again, unless its 15 s have passed.

By hand, as root (the socket is the app's user's): five seconds of text,
gone when the input ends.

```sh
(echo "hello from the voice strip"; sleep 5) | tb-surface --socket /run/user/UID/tinybot/surface.sock voice
```

## State and views

The voice client can also hold **state**, named values it pushes, and
**views**, pages that say which values they show. The app draws a view from
the state; when a value changes, the rows that show it change in place. A
tap goes back to the owner on the connection it holds. Nothing is a file
and nothing is polled. In this version the voice client is the only owner,
its keys start with `voice/`, and the pieces a view can use are fixed in
the app.

### put: the owner's values

```json
{"type":"put","state":{"voice/level":0.42,"voice/default":"opt@linux-node","voice/agents":[{"id":"opt@linux-node","name":"tiny-monolith-optimizer","host":"linux-node","note":"idle","dot":"green"}]}}
```

Answer: `{"type":"ok"}`, or `err` with `privileged`, `bad_put`, `too_big`
or `too_fast`. A refused `put` changes nothing, and it is always answered.

- A key is 1 to 64 of `a-z 0-9 _ . -` in parts joined by `/`, and starts
  with the owner's name: `voice/`. One key that does not refuses the whole
  `put`.
- A value is a text, a number, `true` or `false`, or a list of records (a
  record is an object of texts, numbers and true/false). `null` takes the
  key away. Only the keys named change. A value equal to the one held
  changes nothing and draws nothing.
- Sizes: 16 keys in one `put`, 64 keys held, a text 2048 bytes, a list 200
  records, a record 12 fields of 512 bytes each, and the line 64 KiB as
  every line. Over one of them: `too_big`.
- Rate: 60 `put` lines a second on the connection; the ones over it are
  answered `too_fast` and not taken. The strip's `voice` text is not
  counted.
- The values and the views go when the connection closes.

### view: a page of bound pieces

```json
{"type":"view","name":"agents","title":"Agents","tree":[
 {"kind":"text","text":"Tap an agent to make it the default for voice."},
 {"kind":"each","of":"voice/agents","group":"host","item":
  {"kind":"pick","id":{"field":"id"},"text":{"field":"name"},"note":{"field":"note"},"dot":{"field":"dot"},
   "selected":{"is":"voice/default"},"set":"voice/default"}}]}
```

(One line on the wire.) Answer: `{"type":"ok"}`, or `err` with
`privileged`, `bad_view` or `too_big`. Sending a name again replaces that
view; without `tree` it is taken away.

- `name` is 1 to 32 of `a-z 0-9 _ -`; `title` is the header line (the app
  adds `· voice`, which a view can neither style nor leave out).
- `tree` is a list of up to 64 nodes. A node is a piece: `heading` (`text`),
  `text` (`text`, `id`), `readout` (`id`, `text`, `value`), `pick` (`id`,
  `text`, `note`, `dot`, `selected`, `set`), `button` (`id`, `text`,
  `style`). Or it is `{"kind":"each","of":KEY,"group":FIELD,"item":PIECE}`:
  one piece per record of the list at KEY, with a heading wherever the
  record's FIELD is another than the one before (`group` is optional).
- A field of a piece is a text as written, `{"bind":KEY}` (the value at
  KEY, as text), or inside `each` `{"field":NAME}` (the record's field).
  `selected` is `true`, `false` or `{"is":KEY}`: selected while the value at
  KEY is this pick's id. `"set":KEY` on a pick: a tap writes the pick's id
  to KEY in the app at once, so the selection moves in the same frame; the
  owner's next `put` of KEY says what is true.
- Every key a view names is the owner's own. A view has no `toggle`,
  `stepper`, `number` or `row`, and no `confirm`: a button that asks twice
  is the app's own and only root's layer has it.
- A record whose id is empty, or the same as an earlier one's, is left
  out. A page shows 400 items at most; an owner holds 8 views.
- Where a `put` leaves the same rows, their texts, notes, dots, values and
  selection change in place. Where rows come or go the page is set again
  and stays scrolled where it was.

### event: what the owner hears

Lines the app sends to the owner by itself, between its answers:

```json
{"type":"event","view":"agents","id":"bld@linux-node","kind":"tap"}
{"type":"event","view":"agents","kind":"shown"}
{"type":"event","view":"agents","kind":"hidden"}
```

`tap`: a pick or a button of the view was tapped; `id` is its id. `shown`
and `hidden`: the view came to the screen or left it. A view's taps are
not in any client's `taps` queue.

### Where a view is shown

- With no page of root's or an agent's, the owner's first view comes up by
  itself. Otherwise it waits: the runner lists it on Home ("View: TITLE",
  from voice) and puts it on screen with
  `{"type":"show","layer":"view","name":NAME}` when its button is tapped.
- Over a root page the view's band has "‹ Home", as an agent's page has.
- A `put` for a view that is not on screen is kept and builds nothing.
  (Not built yet: holding the view on screen back while the panel is off.
  Today a `put` for it is built and drawn then too, as a runner's `update`
  is.)
- The voice strip and the two dots stay over a view as over any page.

### The level: a wave in the strip

`voice/level` is the one key the app itself reads: the peak of a block of
sound as a number from 0 to 1, put about 20 times a second while the key
is held.

```json
{"type":"put","state":{"voice/level":0.42}}
```

- The app clamps it to 0..1 (a value that is no number is no level), keeps
  the last 64, and draws them as bars behind the strip's text, the newest at
  the right: one shader, one draw call, a bar's height and lightness its
  level.
- One level is one frame, and the frame repaints the strip's rectangle
  only. Nothing is drawn from a level while the strip is not on screen or
  the panel is off.
- With no level for 300 ms the bars sink (every 50 ms to half their
  height, about six steps) and then no more frames are asked for. While
  levels come the only timer is that one timeout of 300 ms, started again
  by each level.
- The wave goes with the strip, and a strip that comes starts with none.

### By hand

As root, without a voice service (stop it first: there is one voice client
at a time). The first two lines give the Agents view, the third the strip,
the loop five seconds of levels; a tap on an agent prints its `event`
line. The view and the strip go when the input ends.

```sh
S=/run/user/UID/tinybot/surface.sock
(
echo '{"type":"view","name":"agents","title":"Agents","tree":[{"kind":"text","text":"Tap an agent to make it the default for voice."},{"kind":"each","of":"voice/agents","group":"host","item":{"kind":"pick","id":{"field":"id"},"text":{"field":"name"},"note":{"field":"note"},"dot":{"field":"dot"},"selected":{"is":"voice/default"},"set":"voice/default"}}]}'
echo '{"type":"put","state":{"voice/agents":[{"id":"opt","name":"tiny-monolith-optimizer","host":"linux-node","note":"idle","dot":"green"},{"id":"bld","name":"tinybot-building","host":"linux-node","note":"processing","dot":"amber"},{"id":"m2","name":"tinybot-m2","host":"mac-host","note":"ended","dot":"off"}],"voice/default":"opt"}}'
echo '{"type":"voice","text":"listening"}'
i=0; while [ $i -lt 100 ]; do i=$((i+1)); echo "{\"type\":\"put\",\"state\":{\"voice/level\":0.$(( (i * 37 + i * i) % 90 + 10 ))}}"; sleep 0.05; done
sleep 20
) | tb-surface --socket $S talk
```

With the runner connected the view is on Home as "View: Agents"; without
it the view is on screen at once. The strip's text goes after its 15 s, as
any text that is not sent again. `examples/view-by-hand.sh SOCKET [SECONDS]`
is the same as a script.

## The socket

- The app binds and owns it; agents connect. With the default path the app
  makes `$XDG_RUNTIME_DIR/tinybot` with mode 0700 (and refuses a directory
  owned by someone else).
- The socket is made under umask 077 and set to 0600. Every accepted
  connection is checked with `SO_PEERCRED`: the app's own user and root are
  let in (root for the panel runner, which starts actions as root), a peer
  of any other user is dropped.
- A socket file found at start is probed: when something answers, the app
  logs and exits 1 without touching it; a dead one is replaced. The file is
  removed on shutdown.
- What a client can touch is its own layer and nothing else: see
  [Two layers](#two-layers).
- After an app restart the screen is idle and the tap queue is empty; the
  agent sees `"surface":null` in `hello` or `status` and sends its `set` again.
- The surface socket is the app's only socket. Makepad's own remote-control
  endpoint (`/tmp/makepad-remote-<pid>.sock` and its thread), which every
  Makepad app opens by default, stays shut here: the app sets
  `MAKEPAD_REMOTE=0` for itself before `main` unless it is started with
  `--remote` or the environment already sets `MAKEPAD_REMOTE` (Linux with
  glibc; elsewhere set `MAKEPAD_REMOTE=0` in the launcher).

## What the app switches off for itself

Set before `main` (Linux with glibc), each only when the environment does not
already set the variable, so a launcher can turn any of them back on:

| Variable | Default here | Leaves out |
|---|---|---|
| `MAKEPAD_REMOTE` | `0`, unless started with `--remote` | Makepad's remote-control endpoint |
| `MAKEPAD_GAMEPAD` | `0` | the game controller reader, which scans `/dev/input` every second |
| `NO_AT_BRIDGE` | `1` | the AT-SPI accessibility bridge, which retries a missing bus every five seconds |

The same function limits glibc's malloc to one arena (`mallopt(M_ARENA_MAX, 1)`),
which is where most of the app's memory at rest went on the phone.
`TB_SURFACE_ARENA_MAX=N` chooses another limit (`0`: glibc's own choice); a
`GLIBC_TUNABLES` that sets `glibc.malloc.arena_max`, or `MALLOC_ARENA_MAX`,
is left to decide.

## Drawing and the driver: what can be switched off

From Makepad's `work` branch (dc110e5) on, three things reach the GL driver that
the earlier base did not use. Each has a switch for a try by hand:

| What | Off with | Notes |
|---|---|---|
| Only the changed parts of a window are repainted and handed to the display (the back buffer's age, `eglSwapBuffersWithDamage`, `eglSetDamageRegionKHR` where EGL lists them) | `TINYBOT_PARTIAL_REPAINT=0` (the app's own switch): whole frames, plain `eglSwapBuffers` | `MAKEPAD_HAL_LOG=1` prints which of the three EGL paths were found: `[hal-gl] partial repaint: buffer_age=… set_damage_region=… swap_with_damage=…` |
| Linked shader programs are kept on disk (`glGetProgramBinary`) and loaded at the next start | `MAKEPAD_HAL_CACHE_DIR=/dev/null` (a place no directory can be made in; one `[hal-gl] program cache …` line on stderr) | written only under `$MAKEPAD_HAL_CACHE_DIR/makepad-hal/gl-HASH/`, else `$XDG_CACHE_HOME`, else `$HOME/.cache`; a unit can keep it in the app's own directory with `CacheDirectory=tinybot-surface` and `Environment=MAKEPAD_HAL_CACHE_DIR=%C/tinybot-surface` |
| A frame's GPU time is measured where the driver lists `GL_EXT_disjoint_timer_query` | no switch | `MAKEPAD_HAL_LOG=1` says when it is off and why |

The EGL window config is now the first RGBA8 one without depth, stencil or
samples (the window only receives a blit); there is no switch for that.

## tb-panel: panels of tries

Hardware tries as pages the user taps. A panel file describes a page and
what its buttons run; one runner, root, shows Home, the panels and agents'
questions on the surface, runs each try as a transient systemd unit and
writes what happened to state files and records. The runner knows no device:
it passes on what was chosen and typed, and every limit that protects
hardware is the tool's.

```sh
tb-panel run [--dirs DIR:DIR…] [--home] [--key DEVICE:CODE[,CODE…]]   # default /usr/share/tinybot/panels:/etc/tinybot/panels
tb-panel check FILE                       # 0: the runner would take it; 64 and the reason: not
tb-panel show [NAME]                      # a panel's state file, or the runner's own
tb-panel ask FILE                         # queue a question, prints its qid
tb-panel ask --wait QID [--timeout S]     # prints the answer
```

### A panel file

`NAME.json`, in a panel directory. The file and its directory are root's and
not writable by group or others, or the file is left out. A later directory
wins for the same NAME (`/etc/tinybot/panels` over `/usr/share/tinybot/panels`);
a file that fails is left off Home and named there and in the runner's
state, and the panel of that name in an earlier directory is not used
instead. The files are read again each time Home or a panel is opened.

```json
{"panel":"sound","title":"Sound tries","summary":"A tone per tap","order":2,
 "stop":{"argv":["/usr/local/sbin/tb-audio-off"],"timeout_s":5},
 "items":[
  {"kind":"stepper","id":"path","text":"Path","values":["receiver","speaker"]},
  {"kind":"number","id":"level","text":"Level (dBFS)","value":"-40"},
  {"kind":"readout","id":"bat","text":"Battery mA","poll":{"file":"/sys/class/power_supply/battery/current_now"},"every_s":2},
  {"kind":"readout","id":"res","text":"Last try","value":"-"},
  {"kind":"row","buttons":[
    {"id":"play","text":"Play 1 s","style":"primary",
     "action":{"argv":["/usr/local/sbin/tb-sound","--path","{path}","--level","{level}"],
               "as":"root","timeout_s":10,"stop_post":["/usr/local/sbin/tb-audio-off"],"result":"res",
               "ask":["Heard","Faint","Nothing","Pop-bad"],"on_answer":{"Pop-bad":"stop"},
               "checks":[{"name":"pin159","argv":["/usr/local/sbin/tb-pin","159","low"]}]}},
    {"id":"more","text":"Microphone","open":"mic"}
  ]}
 ]}
```

- `panel` is the NAME (the file's base name; A-Z a-z 0-9 _ -, at most 32,
  not starting with `_`). `title`, `summary` (one line) and `order` (lower
  first, then by title) are for Home.
- `items` are the surface's items, with these fields for the runner:
  - a readout's `poll`: `{"file":PATH}` (read in the runner: open, up to 256
    bytes, close) or `{"argv":[…]}` (run directly as the app's user, killed
    at 2 s, its last stdout line), every `every_s` seconds (at least 1,
    default 2);
  - a button's `action` (below) or `open` (another panel's NAME). A button
    with neither is a record of the tap and nothing more;
  - a `feed` (below): items another tool publishes in a file.
- `stop` is the panel's all-off: the runner adds the button "Stop / all off"
  and a line under it for its result. Without `stop` there is neither.
- `key`: `{"code":523,"button":ID}`, or an array of such for several
  codes, binds a key (its evdev code) to a button's action, which must take
  no placeholders: a key press has no values. Panels that bind the same
  code are all left out. The button may be `_stop`: the key is then the
  panel's Stop, as live as its button (on a locked panel, while a try
  runs), and its record names the key.
- A field the runner does not know is refused, and so is a key given twice.
  Ids that start with `_` are the runner's own.

An action:

| Field | Meaning |
|---|---|
| `argv` | the program (an absolute path) and its arguments. `{id}` in an argument is the tap's value of the toggle, stepper or number `id`; `{{` is a literal brace. No shell, no PATH: each value stays inside its one argument whatever it holds. |
| `as` | `user` (the default: the app's user, with `XDG_RUNTIME_DIR=/run/user/UID` and nothing else of the user's environment) or `root` |
| `timeout_s` | required: the unit's RuntimeMaxSec |
| `stop_post` | an argv without placeholders, the unit's ExecStopPost: it runs however the try ends |
| `result` | the id of the readout, text or heading that shows `rc N · last line` |
| `live` | the id of the readout, text or heading that shows the tool's `LIVE ` lines while it runs |
| `ask` | words asked for after the try, shown as a row; the word tapped is recorded |
| `ask_text` | what is asked, shown above that row |
| `retry` | `{"4":{"text":"Play it","add":["--confirm"],"confirm":false}}`: follow-ups by exit status (1 to 255, not 3), see below |
| `short` | `true`, for an action a key runs: beside the tries, see below |
| `on_answer` | `{"WORD":"stop"}`: this answer runs Stop and locks the panel |
| `checks` | `[{"name":…,"argv":[…]}]`: run directly as root after each try, 2 s each; a status other than 0 locks the panel |

`stop.argv`, `stop_post`, `checks` and `poll.argv` take no placeholders: what
they run never depends on what was typed.

### A feed: items from a file

```json
{"kind":"feed","id":"who","file":"/run/tinybot/voice/agents.json",
 "action":{"argv":["/usr/bin/tb-voice","default","{_id}"],"as":"root","timeout_s":5,"result":"res"}}
```

A feed's items are not in the panel file. Another tool (root's) writes them
to `file`, and the runner shows them where the feed stands:

```json
{"items":[
 {"kind":"heading","text":"linux-node"},
 {"kind":"pick","id":"tiny-monolith-optimizer@linux-node","text":"tiny-monolith-optimizer","note":"idle","dot":"green","selected":true},
 {"kind":"readout","id":"seen","text":"Last look","value":"12:04"},
 {"kind":"button","id":"refresh","text":"Look again"}
]}
```

- **The kinds** are the surface's own, those that need no way back for a
  value: `heading` (`text`), `text` (`text`, `id` optional), `readout`
  (`id`, `text`, `value`), `pick` (`id`, `text`, `note`, `dot`, `selected`)
  and `button` (`id`, `text`, `style`). Nothing in a feed arms or asks: no
  `confirm`, no answer row. What needs a second tap or a check stays in a
  panel file. Steppers, numbers and toggles are not feed items yet.
- **The file** is root's alone to write (as a panel file), at most 64 KiB,
  and is replaced at once (written beside and renamed). Ids are unique; an
  unknown kind or field is refused. Of more than 200 items the first 200
  are shown, fewer when the page would not fit in one line to the app, with
  a line `… N more not shown`.
- **A file that is not a feed** (missing, too large, not JSON, a wrong kind
  or field, writable by others) is one line on the page, `feed unavailable:
  WHY`; the rest of the page stays and the runner goes on.
- **When it is read**: as the panel's page goes to the screen, and from
  then on when it changes, while that page is the one on screen and
  `/sys/kernel/tinybot_panel/state` does not read `off`. The runner watches
  the file's directory (inotify), so a file that is replaced shows at once
  and the page costs no wakeup while nothing changes; every 10 s it looks
  once more by itself, for a watch that was lost. It reads the file only
  when its inode, size or time of change is another. A change while the
  panel is off is read when the panel is on again (the runner then looks
  every 2 s until it is). Where the directory is not there yet, or the
  kernel gives no watch, the file is looked at once a second. On Home or
  another page nothing is watched and the runner sleeps.
- **How the page follows**: the same items with other texts, values, notes,
  dots or another selected pick are changed in place (`update`). Items that
  came or went (or a button that reads otherwise) are a `set` of the same
  surface: what the user set and where the page is scrolled to stay.
- **A tap** on a pick or a button runs the feed's `action` with `{_id}` =
  the item's id, one argv element (the argv may take values of the page's
  inputs too). It is an action like a button's: one at a time (`busy`), not
  on a locked panel (`locked`; the items stay on the page), recorded as a
  try with `"button"` = the feed's id and `"_id"` among its values. A tap
  that runs nothing leaves a `{"t":"dropped","feed":…,"id":…,"why":…}`
  record. The action takes no `ask`, `retry` or `short`.
- **A tapped pick is drawn as the selected one at once**, before its action
  has run: the tap is seen in the next frame, whatever the action takes.
  What is true is the file: it is read when the action rewrites it and, in
  any case, when the action has ended, and a pick the action did not select
  is put back then.
- **An id** is 1 to 64 of letters, digits and `. _ @ : -`, not starting
  with `-`. A pick or button with any other id is shown as a line of text
  that says so and cannot be tapped: nothing a file holds becomes an
  argument unchecked. (A text's or readout's id must be such a name too.)
- The owner of the feed learns whether its page is the one on screen from
  the runner's state file: `"page":NAME` and `"front":"root"`.

`examples/agents.json` and `examples/agents-feed.json` are such a panel file
and feed file.

### What the runner does

- **A tap.** The tap is confirmed to the app before anything runs, and taps
  that waited in the app before the runner's page was shown are dropped (and
  recorded as dropped): nothing runs twice or late. The values are checked
  again (a stepper's value is one of its `values`; a number is 1 to 64 bytes
  without NUL or newline), else the result reads `refused: stale value`.
- **A try** runs as
  `systemd-run --unit=tb-panel-NAME-SEQ --wait --pipe --quiet --expand-environment=no -p RuntimeMaxSec=T -p TimeoutStopSec=2 [-p ExecStopPost=…] [--uid=UID -p Environment=XDG_RUNTIME_DIR=/run/user/UID] -- argv…`.
  Without `--expand-environment=no` systemd-run replaces `$NAME` in the
  arguments. The all-off in ExecStopPost runs under `systemd-cat`, so that
  its output goes to the journal and not into the try's stdout.
- **The result** is `rc N · <the last non-empty stdout line>`, at most 120
  characters on the page and whole in the record. One try runs at a time;
  a tap on another action meanwhile reads `busy`.
- **Live output.** With `live`, each stdout line that starts with `LIVE `
  replaces the text of that item (without the prefix) while the tool runs:
  at most ten a second, a newer line taking the place of one that waits.
  Such a line is never the result.
- **A key-bound action and its time limit.** Give it a `timeout_s` well
  above the tool's own limit (for a recorder with `--max-seconds N`, N plus
  some seconds for it to finish), so that a long hold ends as the tool's
  exit 0 and not as a time limit, which locks the panel.
- **A held key.** With `--key DEVICE:CODE[,CODE…]` the runner reads that
  input device (a plain read, no grab; it sleeps until a key moves). Key down
  starts the action a panel binds to the code, whatever page is on screen
  and without taking the screen; key up sends SIGTERM to the tool
  (`systemctl kill --kill-whom=main`), which ends by itself, and the try
  ends like any other. A key held past `timeout_s` ends as a time limit. A
  key down while a try runs or the panel is locked runs nothing and is
  recorded as dropped. The kernel's repeat is ignored.
- **A short action** (`"short": true`, `timeout_s` at most 5, no `ask`,
  `checks` or `retry`) is for keys like the volume keys: it runs while a
  try runs, takes no part in one-at-a-time, never locks, and its key
  coming up means nothing. A locked panel holds it back like any action (a
  record `dropped`, `why` `locked`): a lock means a person must look first.
  It is a record with `"short":true`.
- **A follow-up on offer.** When a try ends with a status its `retry`
  names (a tool's "only if you mean it"), the page shows, above the
  action's row: the tool's last line, then ONE primary button with the
  offer's `text` and the try's values; the row's own action buttons are
  dimmed (still live) with the line "or use '<text>' above". The offer's
  button runs the same action with the values of the try it answers, from
  its record, plus the `add` arguments (fixed strings); `confirm: true`
  makes that button need the double tap. The follow-up is a try of its own
  with `"parent"` in its record. The offer ends after 30 s, when another
  action starts, or when its button is tapped after a value it would run
  with was changed on the page (then nothing runs); each end is recorded.
  A try that ended with such a status is not asked about (`ask`).
- **Stop** is live while a try runs: `systemctl stop` on the try's unit,
  then the panel's `stop` argv as a unit of its own. Its result shows under
  the Stop button, and on Home as "last Stop".
- **A lock.** A panel locks when a try ends with status 3 (Stop then runs at
  once), runs into its time limit (the unit's `Result` is `timeout`), fails
  a check, or gets an answer with `on_answer` stop. Status 1, 2 and 64 do
  not lock. A locked panel shows `locked: <reason>` and a `confirm` button
  Unlock in place of its action buttons; the lock outlives the runner
  within a boot.
- **Home** has a button per panel, the count of waiting questions, an
  agent's page when there is one ("Agent: <title>", from whom), each view
  an owner holds ("View: <title>", from whom; a tap shows it, and none
  comes up by itself over the runner's pages), and while a
  try runs `running: NAME/BUTTON` with that panel's Stop. An agent's new
  page comes straight up only while Home is on screen and nothing runs
  (`--agent-direct=no`: never by itself); over a panel or a try it waits on
  Home. Behind an agent's page the runner reads no readouts. Every
  panel has "‹ Home"; leaving a panel does not end its try.
- **Readouts** are polled for the page on screen only, not while the surface
  is away, and not while `/sys/kernel/tinybot_panel/state` reads `off`. A
  value that did not change is not sent.
- **The surface away** is a state (`"surface":"waiting for the surface"` in
  the runner's state file): the runner connects again, backing off to 5 s,
  and shows its page again. On SIGTERM it stops a running try, clears the
  surface and exits 0.

### State and records

- `/run/tinybot/panel/runner.json`: the panels and their locks, the files
  left out and why, the bound keys, the agent's page, the views and which
  layer is in front (`root`, `agent` or `view`), what runs, the questions that wait, the runner's own error if it
  has one. `/run/tinybot/panel/NAME.json`: the panel's last try
  and its lock. Both are replaced whole.
- `/var/lib/tinybot/tries/NAME.jsonl`, one object per line, rotated once to
  `.1` at 1 MiB:
  - `{"t":"try","seq","panel","button","values","unit","rc","line","state","checks":[{"name","rc"}],"t_tap","t_start","t_end","stopped"}`,
    with `"result"` (systemd's word, e.g. `timeout`) when the unit did not
    end with status 0, `"key"` when a key started it, `"short"` for a short
    action and `"parent"` for a follow-up. A Stop is a try of its own, button
    `_stop`.
  - `{"t":"answer","seq","word","t_answer"}`: the word for try `seq`;
    `unlock` for the unlock of the lock that try set.
  - `{"t":"dropped","tap","t_seen"}`: a tap from before the runner's page;
    `{"t":"dropped","key","button","why","t_seen"}`: a key down that ran nothing.
  - `{"t":"expired","seq","why","t_expired"}`: the offer after try `seq` ended
    unused (`30 s`, `another action`, `a value changed`).
- `/var/lib/tinybot/tries/_questions.jsonl`:
  `{"t":"question","qid","from","text_sha256","text","covers":[{"path","sha256"}],"choices","t_asked","peer":{"uid","comm"},"answer","t_answered"}`.
- Times are Unix seconds with milliseconds. Nothing decides on a log line.

### Questions

`tb-panel ask FILE`, from any user, with FILE
`{"from":…,"text":…,"covers":[{"path":…,"sha256":…}],"choices":["Yes","No"]}`.
The socket (`/run/tinybot/panel/ask.sock`, mode 0666) takes a question and
a wait for its answer and nothing else. `from` is the asker's word; the
runner adds `peer`, the uid and process name the socket gives, and the
question's page shows both.
The runner hashes the text and each covered file itself; a file that is not
what the question says is refused (`file changed`, exit 1). A question never
takes the screen: it is a count on Home and in the header until the user
opens it. If a covered file changed between the question and the answer, the
answer is void (`"answer":null,"void":"file changed: …"`).
`tb-panel ask --wait QID [--timeout S]` prints the answer: exit 0 for the
first choice, 1 for any other answer or a void one, 2 for no answer in time,
3 for no runner or no such question.

### What a tool does to have a panel

- Exit status: 0 done; 1 refused, nothing changed; 2 failed, set back; 3 NOT
  set back (the runner runs Stop and locks); 64 wrong use.
- The last non-empty stdout line is the result a person reads.
- Optionally one stdout line `TBSTATE {json}`: the state the tool read back.
  It goes into the try's record and is not the result line.
- Optionally stdout lines `LIVE <text>` for the page while it runs. A tool
  bound to a key ends on SIGTERM: it finishes, prints its result line and
  exits 0.
- Its limits are its own, the runner has none. Its panel file ships as
  `/usr/share/tinybot/panels/<tool>.json` with a `stop` for hardware.

### Units, and a test build

`systemd/tinybot-surface.service` (user, `Type=notify`: the app sends
`READY=1` once its socket is bound and its first frame is drawn) and
`systemd/tb-panel.service` (system, root) are examples.

The cargo feature `test-userns-root` is for tests on a build host only: with
it the app takes a client that is root of a user namespace (`unshare -r`)
for root, and says `TEST BUILD` on stderr. It does not compile for aarch64
Linux, and `check-release.sh BINARY…` fails on a binary that has it.

## Driving it in a test

```sh
MAKEPAD_HIDE_WINDOWS=1 ./target/release/makepad-example-tinybot-surface --remote --socket /path/test.sock
./target/release/makepad-remote app $PID ui find q=role=button
./target/release/makepad-remote app $PID ui act node=ID press wait=1
./target/release/makepad-remote app $PID settled
```
