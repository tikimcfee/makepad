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
| `clear` | Back to the idle line | `ack` |
| `status` | What is shown and how many taps wait | `status` |

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
| `row` | `id` (optional), `buttons`: a non-empty array of `{id, text, style, confirm, lock}` | buttons side by side, three to a line |
| `button` | `id`, `text`, `style`, `confirm`, `lock` | a row of one button |
| `divider` | | a thin line |

- `style` is `primary`, `secondary` (the default) or `danger`; anything else
  is drawn as `secondary`.
- `confirm: true`: the first tap only arms the button, which then reads
  `tap again: <text>` in the danger style. A second tap within 3 s is the
  tap; without one the button is as before. A tap on another button also
  ends the wait. This happens in the app; the agent sees one tap or none.
- `lock: false`: a tap on this button does not lock its row, and the button
  stays live when another button has locked the row.
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
{"type":"hello","v":2,"surface":"deploy-42","rev":1,"pending":0}
```

`surface` is `null` and `rev` 0 when the idle line is shown; `pending` is the
number of taps that wait.

| Request | Answer |
|---|---|
| `{"type":"set","surface":…,"rev":…,"title":…,"items":[…]}` | `{"type":"ack","surface":…,"rev":…,"pending":N}` |
| `{"type":"update","surface":S,"rev":R,"items":{ID:{"text":…,"value":…},…},"arm":[ID,…]}` | `{"type":"ack","surface":S,"rev":R,"pending":N}` |
| `{"type":"clear"}` | `{"type":"ack","surface":null,"rev":0,"pending":N}` |
| `{"type":"status"}` | `{"type":"status","v":2,"surface":…,"rev":…,"title":…,"items":N,"pending":N,"waiting":N}` |
| `{"type":"taps","wait":false}` | every waiting `tap` line, then `{"type":"taps","count":N,"seq":LAST}` |
| `{"type":"taps","wait":true}` | the same; with no tap waiting, the answer comes when one arrives |
| `{"type":"done","seq":LAST}` | `{"type":"ok","dropped":N,"pending":N}` |

An `update` is for exactly the surface and rev on screen (`stale_rev`
otherwise) and does not change the rev. `items` names items by id: `text`
for a heading, text or readout, `value` for a readout. `arm` names rows to
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
| `too_long` | a line over 64 KiB; the app then closes the connection |

A `set` with the same `surface` and `rev` as on screen is accepted again
(the surface is replaced and its rows are armed).

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

## Driving it in a test

```sh
MAKEPAD_HIDE_WINDOWS=1 ./target/release/makepad-example-tinybot-surface --remote --socket /path/test.sock
./target/release/makepad-remote app $PID ui find q=role=button
./target/release/makepad-remote app $PID ui act node=ID press wait=1
./target/release/makepad-remote app $PID settled
```
