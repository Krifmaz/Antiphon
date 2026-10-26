# Push wake-ups on phones

Spec §10 asks for push notifications, and open question 3 asks for the
encrypted-payload design to be settled before iOS is built on it. This is
that design. The format is [`crates/crypto/src/push.rs`](../crates/crypto/src/push.rs);
the Android receiving side is [`crates/android`](../crates/android).

## Why push at all

A phone in a pocket cannot keep a socket open. Android stops background
network access in Doze, and iOS suspends the app within seconds of it
leaving the screen. The only thing that reliably reaches a sleeping phone is
its platform's push channel, so a caller has to knock there first. The phone
wakes, opens the knock, and then does what it would have done anyway:
listens on the relay, or fetches its mail.

## Which push service

| Platform | Service | Status |
|---|---|---|
| Android | **UnifiedPush**: any distributor app the user installs (ntfy, NextPush, or an FCM-backed one) | Receiving side built |
| Android | FCM directly | Not built: needs a Firebase project and Google's closed SDK in the APK |
| iOS | APNs: PushKit (VoIP) for calls, alert pushes for the rest | Receiving side in `crates/ios`; the same payload, wrapped as under "Transport" |

UnifiedPush keeps the choice of who carries the wake-ups with the user, and a
distributor that relays through FCM still gives Google-reliability delivery
to those who want it, without this app shipping Google's SDK. To the sender
an endpoint is just an HTTPS URL that takes a POST.

## What the push service sees

One opaque 368-byte blob per wake-up, sent to an endpoint URL.

- **Not who is calling.** The sender's identity and signature are inside
  the seal. There is no caller name, no contact id, no notification text.
- **Not what kind of wake-up it is.** A call, a waiting message and a file
  offer are all exactly `SEALED_LEN` bytes; the hint inside is padded to its
  full length.
- **Not a stable fingerprint.** A fresh ephemeral key per wake-up, so two
  wake-ups from the same caller share no bytes.
- **What it does see:** that this endpoint got a wake-up, when, and from
  which IP address the sender posted it. The service already knows which
  device an endpoint belongs to; that is unavoidable with any push service
  and is the price of reaching a sleeping phone. Senders that care about the
  IP can post through a relay or mailbox node (below).

## The sealed payload

Sealed to the device's **push key**, an X25519 key pair each device makes for
itself (`PushKeyPair`), separate from the identity and its prekey:

- Each device has its own push endpoint, so the key belongs to the device,
  not the person, and is replaced along with the endpoint.
- The code that opens a push (an iOS notification service extension, an
  Android broadcast receiver) can run before the app has unlocked the
  identity keystore, and should not need to.

```
sealed  = eph_pub (32) || ChaCha20-Poly1305(key, nonce 0, plain (320)) || tag (16)
key     = HKDF-SHA256(salt "antiphon/push/seal/v1", X25519(eph, push_key),
                      info eph_pub || push_key)
plain   = body (91) || sender PublicIdentity (128) || Ed25519 sig (64) || zeros
body    = version 1 || kind (1) || sent_at u64 BE || id (16) || hint_len (1) || hint padded to 64
sig     = sender.sign("antiphon/push/sender/v1" || push_key || eph_pub || body)
```

The signature binds the push key and the ephemeral key, so a wake-up cannot
be lifted and re-sealed to another device as though the sender had called it.
Knowing the caller before ringing matters on iOS in particular: a PushKit
push must be reported to CallKit as a call immediately, with a name, or the
system stops delivering them. It also lets any phone drop a wake-up from a
stranger or a blocked contact without making a sound.

**Kinds:** `Call` (ring), `Message` (mail is waiting at a mailbox; fetch it
quietly), `File` (an offer). An unknown kind from a newer sender is kept as
`Other(n)` rather than refused.

**Hint:** up to 64 opaque bytes saying where to go next: the relay rendezvous
token to listen on, or the mailbox label to fetch.

**Replay:** a push service could resend a blob it has seen. The receiver
checks `Wake::is_fresh(now, max_age)` (a minute is right for calls; longer
for mail) and remembers the 16-byte ids it has accepted for that long.

## Transport

The same 368 sealed bytes go over every service; only the wrapping differs.
Both phone apps expect exactly this:

- **UnifiedPush (Android):** the raw sealed bytes are the HTTP POST body to
  the endpoint URL, `Content-Type: application/octet-stream`, nothing else.
  The distributor hands them to the app unchanged
  (`PlatformEvent::Push(bytes)`).
- **APNs (iOS):** the sealed bytes, base64-encoded, under the payload key
  `"p"`. A `Call` wake-up goes as a **VoIP push** (PushKit), which iOS
  insists is reported to CallKit at once. Every other kind (`Message`,
  `File`) goes as a **plain alert push**, never a VoIP push, since a VoIP
  push that does not ring is a reason for iOS to stop delivering them. The
  alert's visible text is generic and fixed; nothing in it may depend on
  the sender or the content.

## How a sender would send it

Out of scope for now: nothing on the desktop, relay or mailbox side sends
wake-ups yet. When it is built:

1. A phone publishes `(endpoint URL, push public key)` to its contacts
   inside data they already authenticate, its signed route record or device
   list, so a contact never seals to a key someone else supplied.
2. To call, the caller seals a `Call` wake-up whose hint is the pairwise
   relay token, sends it as described under "Transport" below, and then
   dials the relay as usual.
3. A **mailbox node** holding mail for an offline phone can do the same
   with a `Message` wake-up, so the phone fetches its mail now instead of on
   its next poll. The node needs only the endpoint and the push key, not the
   phone's identity, and since the sealed mail it holds already names nobody,
   adding a wake-up tells it nothing new. Having the node post rather than
   the sender also keeps the sender's IP address from the push service.

## On Android today

`register_push()` asks the user's UnifiedPush distributor for an endpoint;
the endpoint arrives as `PlatformEvent::PushRegistered`, and each wake-up as
`PlatformEvent::Push(bytes)`, still sealed. If the app is not running when a
wake-up arrives, the receiver shows a plain "tap to open" notification
(it cannot open the seal without the app's state) and the wake-up is queued
for the app. With no distributor installed, `register_push()` logs that and
does nothing; the app keeps working while open.
