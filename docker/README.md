# Test peer in Docker

Antiphon has no server: a call goes straight from one peer to the other.
For testing, this image runs a second peer that never sleeps.
`antiphon-cli echo` answers every call and sends the caller's audio
straight back, so you hear yourself about one round trip later. It needs
no microphone, speaker or display.

## Run it

```bash
docker run -d --name antiphon-echo -p 5000:5000/udp -v antiphon-data:/data \
  ghcr.io/krifmaz/antiphon-echo:latest
```

Or build it from this checkout with Compose:

```bash
docker compose -f docker/compose.yaml up -d
```

`docker logs antiphon-echo` prints the peer's identity key at startup and
a verification code for each call.

## Call it

From the desktop app or the CLI, call `<host>:5000`:

```bash
antiphon-cli call --remote 203.0.113.10:5000 --no-ptt
```

The host must be reachable on UDP 5000. On a LAN or a VPS with that port
open that is all it takes. Behind a home router, forward UDP 5000 to the
machine running the container.

## Behaviour

- One call at a time. A second caller waits until the current call ends.
- There is no hang-up message on the wire. A call counts as over after 5
  seconds without audio (`--idle-secs`), and then the next call is
  answered. So after hanging up, wait that long before calling again.
- Only audio is echoed. Chat messages and file transfers are dropped.
- The identity key lives in `/data`. Keep the volume, or every recreated
  container shows clients a key-change warning.

To change the port or the timeout, pass the arguments after the image:

```bash
docker run -d -p 6000:6000/udp -v antiphon-data:/data \
  ghcr.io/krifmaz/antiphon-echo:latest echo --port 6000 --idle-secs 10
```
