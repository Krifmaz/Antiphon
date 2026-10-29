# Push wake-ups on phones

Spec §10 asks for push notifications, and open question 3 asks for the
encrypted-payload design to be settled before iOS is built on it. This is
that design. The format is [`crates/crypto/src/push.rs`](../crates/crypto/src/push.rs);
the Android receiving side is [`crates/android`](../crates/android), the
phone's shared handling is [`crates/mobile/src/push.rs`](../crates/mobile/src/push.rs),
and the sending side is [`crates/push`](../crates/push) (endpoints, rate
limits, UnifiedPush and APNs), used by the desktop
([`crates/client/src/push_wake.rs`](../crates/client/src/push_wake.rs)) and
the mailbox node ([`crates/relay/src/mailbox.rs`](../crates/relay/src/mailbox.rs)).

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
| Android | **UnifiedPush**: any distributor app the user installs (ntfy, NextPush, or an FCM-backed one) | Built both ways: the phone publishes its endpoint; desktops and mailbox nodes post to it |
| Android | FCM directly | Not built: needs a Firebase project and Google's closed SDK in the APK |
| iOS | APNs: PushKit (VoIP) for calls, alert pushes for the rest | Receiving side in `crates/ios` (VoIP only so far). Sent by a mailbox node whose operator configured an Apple key (`--features apns`) |

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

## Telling contacts how to wake the phone

The phone publishes `(push key, endpoint)` inside its **signed route
record**, the record that already says where to leave its mail. It goes to
each contact as the `routes` field of the `Hello` at the start of every
call, and the contact keeps it only if it is signed by the identity key
they pinned for that phone and is newer than the one they hold. So a
contact never seals to a key someone else supplied.

Each target is one route in that record:

```
kind    = "push:" || push public key, 64 hex digits
address = the endpoint's text form:
            https://...            a UnifiedPush endpoint
            apns-voip:<hex token>  a PushKit token (calls only)
            apns-alert:<hex token> an APNs alert token (everything else)
```

The key rides in the kind so the address keeps the record's whole
256-byte limit for the endpoint; FCM-backed UnifiedPush distributors hand
out long URLs. Older builds keep unknown kinds untouched and never mistake
one for a mailbox, whose address always ends in `#label`.

On the phone ([`crates/mobile/src/push.rs`](../crates/mobile/src/push.rs)):
the push key is made on first start and kept in the app's private storage.
`register_push()` runs on every start; when `PushRegistered` arrives (a URL
on Android, a hex PushKit token on iOS, which becomes `apns-voip:`), the
phone signs a record with `direct` and the push target, saves it, and puts
it in every later `Hello`. The same endpoint reported again re-sends the
same bytes, so contacts are not handed a new sequence on every launch; a
restored identity re-signs it.

## What the phone does with a wake-up

`PlatformEvent::Push(bytes)` is opened with the push key, then dropped
silently unless it is fresh (a minute for calls, an hour for anything
else), its id has not been seen, the sender is a saved contact, and that
contact is within the phone's own limit (three calls in a row then one per
20 seconds; five other wake-ups then one a minute). Then:

- **Call**: start listening for calls, if not already, so the dial that
  follows lands. The call arriving is what rings, through the same path as
  any incoming call; on iOS CallKit is already showing it, since PushKit
  reported it before anything else ran. Nothing is started without
  microphone permission.
- **Message**: the phone keeps no mailbox yet, so there is nothing to
  fetch; it shows "left you a message" under the contact's name.
- **File**: "wants to send you a file".

## Who sends

**The desktop** ([`push_wake.rs`](../crates/client/src/push_wake.rs)):

- *Calls*: dialing a saved contact starts a two-second timer. If the call
  has not connected when it fires, the phone was not reachable, so a `Call`
  wake-up goes out and the dial carries on; if it connects first, nothing
  is sent.
- *Mail*: after a message is left at one of the contact's mailboxes, a
  `Message` wake-up.
- Both are limited per contact (calls: three then one per 20 seconds;
  mail: one a minute, since one fetch collects everything).
- UnifiedPush endpoints are posted directly over HTTPS (ureq with rustls).
  APNs targets are handed to the mailbox nodes on the user's own route
  list, in order, for one with an Apple key to pass on.

**A mailbox node** (`antiphon-relay --mailbox --mailbox-push`):

- An inbox's owner may register a push endpoint and push key against
  their label (`RegisterPush`, authorised by the fetch secret like a
  fetch). When new mail is deposited, the node seals a `Message` wake-up
  to that key, signed with the node's own key (kept in
  `--mailbox-push-key`, printed at start-up), and posts it; at most one
  per inbox per minute. The reply to `RegisterPush` carries the node's
  key so the owner's phone can recognise its wake-ups and treat them as
  mail notices only; a node never rings anyone.
- Senders may hand the node an already sealed wake-up to pass on (`Wake`).
  This is how a desktop reaches an iPhone, and also keeps the sender's IP
  address from the push service. Limited per endpoint (as a phone limits
  calls) and per sending address (ten, then one per six seconds); a
  members-only node asks for its code, as it does to register.

**What a node learns.** For `RegisterPush`: the endpoint and push key *of
a label it already holds*. It still learns no identity, no sender and no
contents, and the push service learns the node's address instead of a
sender's. The one new linkage: two nodes holding inboxes for the same
phone could match them by the shared endpoint or push key, which per-node
labels otherwise prevent. So a phone should register its endpoint with
one node, or a separate UnifiedPush endpoint per node (distributors allow
several per app). For `Wake`: an endpoint and a blob it cannot open, from
an address; nothing to tie the two to a person beyond what the push
service itself knows.

**Posting safely.** An endpoint URL comes from someone else, so a sender
refuses plain `http://`, refuses hosts that resolve to loopback, private,
link-local or other non-public addresses, and follows no redirects. The
name is resolved once for the check and again by the HTTP client, so a
name that changes its answer in between can still get past the address
check; the scheme and redirect rules hold regardless.

## APNs: only whoever runs a node

Apple authenticates senders with a token signed by a `.p8` key from the
app's developer account (ES256, renewed every 40 minutes). Anyone holding
that key can push to every install of the app, so it can never ship inside
the app, and no app user has one. The operator of a node does:

```
cargo build --release -p antiphon-relay --features apns
antiphon-relay --mailbox --mailbox-push \
    --apns-key AuthKey_ABC123DEFG.p8 --apns-key-id ABC123DEFG \
    --apns-team-id TEAM123456 --apns-topic org.antiphon.app [--apns-sandbox]
```

(or `ANTIPHON_APNS_KEY`, `ANTIPHON_APNS_KEY_ID`, `ANTIPHON_APNS_TEAM_ID`,
`ANTIPHON_APNS_TOPIC`, `ANTIPHON_APNS_SANDBOX`). The `apns` feature is off
by default because APNs is HTTP/2 only, which brings in `h2`, and nobody
without the key can use it. A VoIP push goes to `<topic>.voip` with
`apns-push-type: voip`, expiring after 30 seconds; an alert push to
`<topic>` with a fixed generic alert. Without a key, APNs endpoints are
refused at registration rather than accepted and dropped.

Still missing on iOS: the app registers only a PushKit (VoIP) token, so
mail and file wake-ups have no alert token to go to, and there is no
notification service extension to open them. Calls work end to end once a
node with a key passes them on.

## The desktop does not need push

Spec §10 lists "persistent socket on desktop" for push. A running desktop
already is one: with the door open (Settings, or on start with
`door_on_start`) it listens on its port, re-listens after every call,
refreshes its relay registration every 20 seconds and renews its router
mapping, and collects mail every minute. A desktop that is not running
cannot be woken by anything, and a push service could not change that.

## On Android today

`register_push()` asks the user's UnifiedPush distributor for an endpoint;
the endpoint arrives as `PlatformEvent::PushRegistered`, and each wake-up as
`PlatformEvent::Push(bytes)`, still sealed. If the app is not running when a
wake-up arrives, the receiver shows a plain "tap to open" notification
(it cannot open the seal without the app's state) and the wake-up is queued
for the app. With no distributor installed, `register_push()` logs that and
does nothing; the app keeps working while open.
