# Local API for scripts and bots

Antiphon can serve a small API to programs on the same computer, so you can
script it or run a bot. It is **local-first**: the API listens on
`127.0.0.1` only, nothing goes through a server, and a bot is just a program
on your machine acting as an Antiphon identity.

## Turning it on

- In the app: **Settings > Network > Scripts and bots > Local API**.
- Or start the app with `--api`.
- For a bot with an identity of its own, with no window:
  `antiphon --profile mybot --headless`. This opens the door and starts the
  API, and prints where the API's details are. What the window would show
  as a notice is printed instead.

While the API is on, the app writes `api.json` to its config folder, and
only your user can read the file:

```json
{ "host": "127.0.0.1", "port": 41234, "token": "9f…", "api": 1 }
```

The port is chosen fresh each time, and so is the token. The file is
removed when the API stops.

## Protocol

The protocol is JSON lines: one JSON object per line, in both directions.
The first line a client sends must be the token, and the app closes the
connection otherwise.

```text
-> {"token":"9f…"}
<- {"ok":true,"result":{"api":1}}
```

After that, the client sends commands. Each command has a `cmd` field and
an optional `id`, which the answer repeats.

```text
-> {"id":1,"cmd":"send","to":"<contact id>","text":"hello"}
<- {"id":1,"ok":true,"result":null}
-> {"id":2,"cmd":"nope"}
<- {"id":2,"ok":false,"error":"unknown command 'nope'"}
```

| Command | Arguments | Result |
|---|---|---|
| `me` | – | `{id, name, status, invite, door_open}` |
| `contacts` | – | `[{id, name, online, verified, unread, last}]` |
| `messages` | `with` (contact id), `limit` (default 50) | `[{id, mine, author, text, time, edited}]`, oldest first |
| `send` | `to`, `text` | `null` once it has been handed to delivery (the outbox if they're offline) |
| `call` | `to` | `null`; the call's progress comes as `call` events |
| `hangup` | – | `null` |
| `mute` | `on` (bool) | `null` |
| `door` | `open` (bool) | `null` |
| `status` | – | `{phase, with, name, code}`, where `phase` is `idle`, `dialing`, `connected` or `ended` |
| `subscribe` | – | `null`, then events on this connection |

A command does exactly what the matching button in the app does.
Everything else still applies, such as blocks, delivery routes,
verification and encryption.

### Events

After `subscribe`, the app also sends lines with no `id`:

```text
<- {"event":"message","from":"<contact id>","name":"Sam","text":"hi","at":1793308716}
<- {"event":"call","phase":"connected","with":"<contact id>"}
```

## Example

[`crates/desktop/examples/echo_bot.rs`](../crates/desktop/examples/echo_bot.rs)
answers every message with the same words. It is about forty lines of plain
Rust with `serde_json`.

## Security

- Only programs running on this computer can connect, and only with the
  token. The token is compared in constant time.
- The token lives in a file only your user can read, so another user on the
  same machine can't drive your app.
- Any program you give the token to can act as you: it can read your
  messages and send messages and calls in your name. Treat the token as you
  would a password, and turn the API off when you aren't using it. It is
  off by default.
- A client may send lines of at most 64 KB, so a broken script can't make
  the app buffer without limit.
