//! Group chats and communities (spec §9 "Group chats up to ~100, no
//! server required", "Communities with channels, roles, permissions",
//! "Threads and replies", "per-community ban lists").
//!
//! A group chat and a community are the same thing here -- a [`Space`]
//! -- told apart by their manifest's [`Kind`]: a group chat is one
//! conversation where anyone may invite, a community has channels and the
//! Admin / Moderator / Member roles. Each is:
//!
//! - **A signed manifest** ([`antiphon_crypto::community`]): who is in it,
//!   with what role, its channels, its ban list, its nodes. Every change
//!   is a new version signed by its author and checked by every member
//!   against the version before -- that check *is* the permission system,
//!   since there is no server to enforce one.
//! - **One MLS group** ([`antiphon_crypto::group`]) for all its channels.
//!   One group rather than one per channel because membership here is
//!   community-wide: one group means one commit per join, leave or ban
//!   instead of one per channel, one epoch to keep in step over mail that
//!   arrives out of order, and one place a ban takes effect. The channel
//!   is a field *inside* each encrypted message, so a mailbox can't even
//!   tell which channel is busy.
//! - **Private channels** ride the same group: a channel the manifest
//!   gives a reader list to is *sent* only to those readers -- every
//!   message, edit and removal in it goes out as a separate [`Out`] to each
//!   reader, sealed to them as all mail is -- so other members never
//!   receive a byte of it, and a reader's app drops anything in it from
//!   someone who isn't one. What everyone can see is that the channel
//!   exists and who may read it, because that is in the manifest. (A
//!   member who never receives a sender's private messages skips ahead in
//!   that sender's MLS key chain on their next public one; MLS allows a
//!   gap of up to 1000 messages per sender per epoch.)
//!
//! # Getting messages there with no server
//!
//! Everything a member sends is a [`Wire`] message to each other member,
//! the way 1:1 mail already goes: over a live call's encrypted control
//! channel when there is one, and otherwise sealed to them and left at the
//! mailboxes *their* route list names (spec §8). Mailboxes take ~1 KB
//! blobs, so a [`Wire`] is cut into [`fragments`] and put back together on
//! arrival. Members learn each other's prekeys and route lists from
//! [`Card`]s -- self-signed, so anyone may pass one along -- rather than
//! having to be each other's contacts.
//!
//! # Keeping MLS in order
//!
//! MLS needs everyone to apply commits in the same order, and there is no
//! delivery service to impose one. So:
//!
//! - Messages from an epoch we haven't reached yet are **held** and
//!   replayed once the commit that leads there arrives.
//! - The manifest says who should be in the group; a commit is only
//!   accepted if it moves the group *towards* that (it adds people the
//!   manifest lists and removes people it doesn't).
//! - Most commits are made by whoever made the change (the inviter, the
//!   moderator who banned); someone *leaving* is removed by the member at
//!   the lowest leaf index still listed, so one person does it.
//! - If two commits for the same epoch still cross in the mail, the one
//!   whose bytes hash lower wins everywhere: whoever applied the other
//!   rolls back to the state it saved just before applying it and applies
//!   the winner, and if the loser was theirs they make their change again
//!   on top.
//!
//! # How you appear, and as whom (spec §7 "per-community identity")
//!
//! Each space has its own [`Appearance`] -- name, status line, a small
//! picture -- carried in your [`Card`] there; empty means your profile's.
//! Separately, you can be in a space under a **separate identity**
//! ([`antiphon_crypto::persona`]), chosen when you create it or accept the
//! invite: a key derived for that space that is your MLS credential,
//! manifest entry, card and mailbox there, so no two spaces (and none of
//! your contacts) can match you up. To keep that true:
//!
//! - An invite is an [`Offer`]: nobody is written into the manifest until
//!   they answer, and then under the key they answer with. Your Join goes
//!   to the inviter sealed with your main key -- they alone see both.
//! - What you send as a separate identity goes by mailbox, sealed as that
//!   identity, and never over a live call (which is with your main key).
//! - The courier collects that identity's own inboxes, and takes only
//!   community traffic there.
//! - Shared files from that space are fetched as that identity.
//!
//! The tradeoffs are spelled out in `antiphon_crypto::persona`.
//!
//! Not handled: a member who misses a commit for good (offline longer than
//! the mailboxes keep mail) is stuck in an old epoch until someone invites
//! them again; nothing detects that for them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use antiphon_crypto::community::{self as cm, perm, Key, Kind, Manifest, SignedManifest};
use antiphon_crypto::group::{GroupMember, Processed};
use antiphon_crypto::{BackupKey, IdentityKeyPair, PublicIdentity, SignedRouteRecord};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::community_board::{self, EpochKey};
use crate::delivery::hex;
use crate::file_transfer::{decode_b64, encode_b64};

/// How many manifest versions a member keeps, for a member who missed
/// some to catch up from, and for settling two competing versions.
const KEEP_MANIFESTS: usize = 64;
/// Messages from future epochs held at once.
const MAX_HELD: usize = 256;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    antiphon_crypto::hex::decode(s)
}

fn b64(bytes: &[u8]) -> String {
    encode_b64(bytes)
}

fn unb64(s: &str) -> Result<Vec<u8>, String> {
    decode_b64(s).ok_or_else(|| "malformed message".to_string())
}

fn sha(bytes: &[u8]) -> Key {
    Key(Sha256::digest(bytes).into())
}

// ---- cards ---------------------------------------------------------------

/// How to reach a member: their prekey (signed by their identity) and
/// their route list (signed too). Self-certifying, so it can be relayed by
/// anyone in the group without being trusted.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Card {
    pub key: Key,
    /// `PublicIdentity::encode`, hex.
    pub public: String,
    /// Their signed route list, hex, or empty for "direct only".
    #[serde(default)]
    pub routes: String,
    /// What they call themselves. Shown as theirs, not as a petname.
    pub name: String,
    /// Their status line here (spec §7 "per-community identity").
    #[serde(default)]
    pub status: String,
    /// Their picture here, a small JPEG as base64 (see [`MAX_AVATAR`]).
    /// `None` means "not included" -- cards passed around in bulk (a
    /// Welcome lists everyone's) leave it out to stay small, and a card
    /// without one never clears the picture already known. `Some("")` is
    /// "no picture".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
}

/// The largest picture a card may carry, as base64. A 48 px thumbnail;
/// the whole card travels in ~540-byte mailbox fragments.
pub const MAX_AVATAR: usize = 6 * 1024;

impl Card {
    pub fn mine(identity: &IdentityKeyPair, routes_hex: String, name: &str) -> Self {
        Self {
            key: Key(identity.public().identity_key),
            public: hex(&identity.public().encode()),
            routes: routes_hex,
            name: name.chars().filter(|c| !c.is_control()).take(80).collect(),
            status: String::new(),
            avatar: None,
        }
    }

    /// With a status line and picture for this community.
    pub fn with_profile(mut self, status: &str, avatar: &str) -> Self {
        self.status = status.chars().filter(|c| !c.is_control()).take(120).collect();
        self.avatar = Some(if avatar.len() <= MAX_AVATAR { avatar.to_string() } else { String::new() });
        self
    }

    /// Without the picture, for passing many cards at once.
    pub fn slim(&self) -> Self {
        Self { avatar: None, ..self.clone() }
    }

    fn has_avatar(&self) -> bool {
        self.avatar.as_deref().is_some_and(|a| !a.is_empty())
    }

    /// The prekey, if the card is genuinely signed by the identity it
    /// names.
    pub fn prekey(&self) -> Option<[u8; 32]> {
        let public = PublicIdentity::decode(&unhex(&self.public)?).ok()?;
        public.verify().ok()?;
        (public.identity_key == self.key.0).then_some(public.prekey)
    }

    pub fn record(&self) -> Option<SignedRouteRecord> {
        crate::delivery::accept_record(&self.routes, &self.key.0, None)
    }

    fn valid(&self) -> bool {
        self.prekey().is_some() && (self.routes.is_empty() || self.record().is_some())
    }
}

// ---- what travels --------------------------------------------------------

/// That whoever asks into a shared space is a member of one of the
/// communities sharing it (spec §12 Q4): their key there, and its
/// signature over the shared space and the key they ask with. The member
/// it goes to checks that key against their own copy of that community.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Proof {
    /// The community they come from.
    pub community: Key,
    /// Their encoded public identity there, hex.
    pub public: String,
    /// Its signature over [`proof_bytes`], hex.
    pub signature: String,
}

/// What a [`Proof`] signs: it is for this shared space and this asker
/// only, so it can't be replayed into another or by someone else.
fn proof_bytes(shared: &Key, community: &Key, asker: &[u8; 32]) -> Vec<u8> {
    let mut b = b"stoop-shared-space-v1".to_vec();
    b.extend_from_slice(&shared.0);
    b.extend_from_slice(&community.0);
    b.extend_from_slice(asker);
    b
}

impl Proof {
    /// `home` (as the identity we are in `community` with) vouching that
    /// `asker` may ask into `shared` on its behalf.
    pub fn make(home: &IdentityKeyPair, community: Key, shared: &Key, asker: &[u8; 32]) -> Self {
        let signature = home.sign(&proof_bytes(shared, &community, asker));
        Self { community, public: hex(&home.public().encode()), signature: hex(&signature) }
    }

    /// The key it proves membership for, if the signature holds for this
    /// shared space and asker.
    pub fn check(&self, shared: &Key, asker: &[u8; 32]) -> Option<[u8; 32]> {
        let identity = PublicIdentity::decode(&unhex(&self.public)?).ok()?;
        let signature: [u8; 64] = unhex(&self.signature)?.try_into().ok()?;
        antiphon_crypto::verify_signature(&identity.identity_key, &proof_bytes(shared, &self.community, asker), &signature).ok()?;
        Some(identity.identity_key)
    }
}

/// One message between members. Byte fields are base64.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t")]
pub enum Wire {
    /// "Join us": the manifest that lists them, and how to answer.
    Invite { manifest: String, card: Card },
    /// Accepting: a KeyPackage to be added with, and how to reach them.
    Join { community: Key, key_package: String, card: Card },
    /// For the newcomer: the Welcome, the manifest, everyone's cards.
    Welcome { community: Key, welcome: String, manifest: String, cards: Vec<Card> },
    /// For everyone else: a commit, the manifest it brings the group in
    /// line with, and any newcomers' cards.
    Commit { community: Key, commit: String, manifest: String, #[serde(default)] cards: Vec<Card> },
    /// An MLS application message: a [`Post`].
    App { community: Key, data: String },
    /// "I'd like to join": a stranger, from a listed community's join
    /// link, to the member who made it. `card` is how to answer them.
    /// Asking into a shared space, `proof` shows which community they
    /// come from, and a founding community's admin brings the `ticket`
    /// they were given.
    Knock {
        community: Key,
        card: Card,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proof: Option<Proof>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<Key>,
    },
    /// "I have version `since`; send me what came after."
    ManifestRequest { community: Key, since: u64 },
    /// For a newcomer to a community with a board, whose Welcome was too
    /// big for mailboxes: it is on the board after entry `since`, sealed
    /// under the epoch they join in, whose key and token (hex) these are
    /// -- theirs to have, as they are in that epoch from now on.
    BoardWelcome { community: Key, board: cm::BoardRef, epoch: u64, key: String, token: String, since: u64 },
    Manifests { community: Key, list: Vec<String> },
}

impl Wire {
    pub fn community(&self) -> Option<Key> {
        match self {
            Wire::Invite { .. } => None,
            Wire::Join { community, .. }
            | Wire::Knock { community, .. }
            | Wire::Welcome { community, .. }
            | Wire::Commit { community, .. }
            | Wire::App { community, .. }
            | Wire::ManifestRequest { community, .. }
            | Wire::Manifests { community, .. }
            | Wire::BoardWelcome { community, .. } => Some(*community),
        }
    }
}

/// What members say to each other inside the group's encryption.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t")]
pub enum Post {
    Message {
        id: u64,
        channel: u32,
        text: String,
        #[serde(default)]
        reply_to: Option<u64>,
        at_secs: u64,
    },
    Edit { id: u64, text: String },
    /// Withdrawn by its author, or removed by a moderator (who must hold
    /// Manage messages and outrank `author`).
    Delete { id: u64, author: Key },
    /// A reaction put on (`on`) or taken off a message: `id` by `author`,
    /// in `channel`. On and off rather than a toggle, so a reaction
    /// delivered twice still leaves everyone agreeing.
    React { id: u64, author: Key, channel: u32, emoji: String, on: bool },
    /// A file shared in a channel (spec §5), fetchable from its holders.
    File { id: u64, channel: u32, name: String, size: u64, file_id: String, key: String, holders: Vec<String>, at_secs: u64 },
    Card(Card),
    /// A new manifest version that changes nobody's membership.
    Manifest { manifest: String },
    /// Messages reported to the moderators, sent only to them.
    Report { id: u64, channel: u32, reason: String, excerpts: Vec<Excerpt>, at_secs: u64 },
    /// "I'm in this voice channel" (`here`), or "I've left it", with how
    /// to reach me for the call. Sent to the channel's readers.
    Voice { channel: u32, here: bool, invite: String, at_secs: u64 },
}

/// Something to send: `wire` to the member `to`.
#[derive(Clone, Debug, PartialEq)]
pub struct Out {
    pub to: [u8; 32],
    pub wire: Wire,
}

/// What a message did, for the app to record and show.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Message { channel: u32, id: u64, sender: [u8; 32], text: String, reply_to: Option<u64>, at_secs: u64 },
    Edited { id: u64, sender: [u8; 32], text: String },
    /// Already checked: by the author, or by a moderator allowed to.
    Deleted { id: u64, author: [u8; 32] },
    /// `sender` put a reaction on (`on`) or took it off message `id`,
    /// which `author` wrote. Already checked: the emoji is a Unicode emoji
    /// or one of this community's, and the sender may read the channel.
    Reacted { id: u64, author: [u8; 32], sender: [u8; 32], emoji: String, on: bool },
    File { channel: u32, id: u64, sender: [u8; 32], name: String, size: u64, file_id: String, key: String, holders: Vec<String>, at_secs: u64 },
    Joined,
    /// A member reported messages to us as a moderator.
    Reported(Report),
    /// We were taken out of the group: nothing new can be read.
    Removed,
    Changed,
    Notice(String),
}

#[derive(Default, Debug)]
pub struct Handled {
    pub outs: Vec<Out>,
    pub events: Vec<Event>,
}

// ---- one group chat or community -----------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
enum Redo {
    Add { key_package: String, card: Card },
    Remove { keys: Vec<Key> },
}

/// The state just before the last commit we applied, kept until the next
/// one, so a competing commit that wins can replace it.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Previous {
    /// The epoch both commits were made in.
    epoch: u64,
    mls: String,
    commit: Key,
    /// Ours to make again if we lose.
    redo: Option<Redo>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Held {
    from: Key,
    wire: Wire,
}

/// Someone we invited who hasn't answered. They aren't written into the
/// manifest until they do, because they may answer with a separate
/// identity (see [`antiphon_crypto::persona`]) -- and a manifest that had
/// listed their main key first would have told every member who the new
/// key belongs to.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Offer {
    key: Key,
    /// What we call them, for the manifest if they join as themselves.
    name: String,
    at_secs: u64,
    /// They asked, from the join link, rather than being invited.
    #[serde(default)]
    knocked: bool,
    /// They brought their community into this shared space with a
    /// ticket, and become one of its owners when they join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    founder: Option<Key>,
}

/// Most tickets for founding a shared space held at once.
const MAX_TICKETS: usize = 8;
/// How long a ticket for founding a shared space lasts: a week.
const TICKET_LIFETIME_SECS: u64 = 7 * 24 * 60 * 60;

/// Most open invitations made in answer to strangers asking, so a flood
/// of them can't push out the ones members sent.
const MAX_KNOCKS: usize = 50;

const MAX_OFFERS: usize = 200;
/// How long an invitation can be answered: a Join after this is refused
/// as if it had never been sent. Withdrawing one ends it sooner.
pub const OFFER_LIFETIME_SECS: u64 = 14 * 24 * 60 * 60;

/// An invitation we sent that hasn't been answered, as the app lists them
/// for withdrawing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingInvite {
    /// Who it went to (their main key).
    pub key: [u8; 32],
    /// What we call them.
    pub name: String,
    /// When it was sent.
    pub at_secs: u64,
}

/// One member's recent posts, by the time each says it was written, for
/// slow mode and the flood cap. Only the newest [`PACE_KEEP`] are kept.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Pace {
    key: Key,
    /// Oldest first.
    times: Vec<u64>,
}

/// Post times remembered per member.
const PACE_KEEP: usize = 32;
/// The hard flood cap, whatever slow mode says: at most this many posts
/// from one member within [`FLOOD_WINDOW_SECS`] of each other.
pub const FLOOD_MAX: usize = 10;
/// The window the flood cap counts in.
pub const FLOOD_WINDOW_SECS: u64 = 10;
/// A post may say it was written at most this far in the future (clocks
/// drift); later than that and it is dropped.
const FUTURE_SLACK_SECS: u64 = 300;

/// Something a member reported to the moderators: which messages, as the
/// reporter read them, and why.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Random, to tell reports apart.
    pub id: u64,
    /// Who reported it (their key in the space).
    pub from: Key,
    /// The channel the messages are in.
    pub channel: u32,
    /// What the reporter said about it.
    pub reason: String,
    /// The messages, as the reporter's app decrypted them.
    pub excerpts: Vec<Excerpt>,
    /// When it was sent.
    pub at_secs: u64,
}

/// One reported message, as the reporter's app read it. The reporter
/// could have made it up: a moderator who can read the channel compares
/// it with their own copy (`Store::matches_history`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Excerpt {
    /// The message's id.
    pub id: u64,
    /// Who wrote it.
    pub author: Key,
    /// What it said.
    pub text: String,
    /// When it says it was written.
    pub at_secs: u64,
}

/// Someone in a voice channel, by their last word on it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    /// The voice channel.
    pub channel: u32,
    /// Who (their key in the space).
    pub key: Key,
    /// How to reach them for the call: their invite link, which carries
    /// their main identity key and their relay.
    pub invite: String,
    /// When they last said they were there, by their clock.
    pub at_secs: u64,
}

/// How often someone in a voice channel says they still are.
pub const VOICE_HEARTBEAT_SECS: u64 = 120;
/// How long after their last word someone counts as still there: two
/// heartbeats and a little, so one lost one doesn't drop them.
pub const VOICE_TTL_SECS: u64 = 2 * VOICE_HEARTBEAT_SECS + 30;

/// Most messages in one report.
pub const MAX_EXCERPTS: usize = 10;
/// Most reports a moderator keeps waiting.
const MAX_REPORTS: usize = 200;
/// The most different reactions one member keeps on one message. Ten
/// times what anyone uses, and a ceiling on how much one member can grow
/// a message's stored record by reacting.
pub const MAX_REACTIONS_EACH: usize = 20;

#[derive(Serialize, Deserialize)]
struct Saved {
    manifests: Vec<String>,
    mls: String,
    cards: Vec<Card>,
    #[serde(default)]
    held: Vec<Held>,
    #[serde(default)]
    previous: Option<Previous>,
    #[serde(default)]
    joining: Option<Key>,
    #[serde(default)]
    removed: bool,
    /// We are in it under a separate identity, derived from our seed and
    /// its id ([`antiphon_crypto::persona::community_identity`]).
    #[serde(default)]
    persona: bool,
    #[serde(default)]
    offers: Vec<Offer>,
    #[serde(default)]
    pace: Vec<Pace>,
    #[serde(default)]
    reports: Vec<Report>,
    #[serde(default)]
    voice: Vec<Presence>,
    #[serde(default)]
    tickets: Vec<(Key, u64)>,
    #[serde(default)]
    board_keys: Vec<EpochKey>,
}

pub struct Space {
    me: [u8; 32],
    /// Oldest first; the last is the current version.
    chain: Vec<(SignedManifest, Manifest)>,
    mls: GroupMember,
    cards: Vec<Card>,
    held: Vec<Held>,
    previous: Option<Previous>,
    /// Waiting for a Welcome after accepting an invite from this member.
    pub joining: Option<[u8; 32]>,
    /// Taken out of the group, or left.
    pub removed: bool,
    /// In it under a separate identity: `me` is that key, not our main one.
    pub persona: bool,
    offers: Vec<Offer>,
    /// Recent post times per member, for slow mode and the flood cap.
    pace: Vec<Pace>,
    /// Reports moderators were sent and haven't dealt with, newest last.
    reports: Vec<Report>,
    /// Who said they are in which voice channel, and when.
    voice: Vec<Presence>,
    /// Tickets we gave out for another community to join this shared
    /// space, and when.
    tickets: Vec<(Key, u64)>,
    /// The recent epochs' board keys and tokens, oldest first, for
    /// reading and posting to the community's board (kept whether or not
    /// it has one, so they are there the moment it does).
    board_keys: Vec<EpochKey>,
}

/// The identity to act as in a space: `main`, or the separate one derived
/// for it.
pub fn identity_for(main: &IdentityKeyPair, id: &Key, persona: bool) -> IdentityKeyPair {
    let seed = if persona { antiphon_crypto::persona::community_identity(main, &id.0).master_seed() } else { main.master_seed() };
    IdentityKeyPair::from_master_seed(seed)
}

impl Space {
    /// A new group chat or community, with us as its owner. `id` is
    /// chosen first so a separate identity -- derived from it -- can be the
    /// owner from the very first version: `identity` and `card` are
    /// already that identity's when `persona` is set.
    pub fn create(identity: &IdentityKeyPair, id: Key, kind: Kind, name: &str, card: Card, persona: bool) -> Result<Self, String> {
        let mut m = Manifest::genesis(identity, kind, name, &card.name);
        m.id = id;
        let signed = SignedManifest::sign(&m, identity);
        let mut mls = GroupMember::new(identity);
        mls.create().map_err(|e| e.to_string())?;
        Ok(Self {
            me: identity.public().identity_key,
            chain: vec![(signed, m)],
            mls,
            cards: vec![card],
            held: Vec::new(),
            previous: None,
            joining: None,
            removed: false,
            persona,
            offers: Vec::new(),
            pace: Vec::new(),
            reports: Vec::new(),
            voice: Vec::new(),
            tickets: Vec::new(),
            board_keys: Vec::new(),
        })
        .map(|mut space| {
            space.note_epoch();
            space
        })
    }

    /// The key we are in it as.
    pub fn me(&self) -> [u8; 32] {
        self.me
    }

    /// Accepting an invite: the space as far as we know it, and the Join
    /// for the inviter. `identity` and `card` are whoever we join as: our
    /// main identity, or -- with `persona` -- the one derived for this
    /// community. Either way the Join must go to the inviter sealed with
    /// our *main* key, the one they invited.
    pub fn accept(identity: &IdentityKeyPair, invite: &Invite, card: Card, persona: bool) -> Result<(Self, Out), String> {
        let signed = SignedManifest::decode(&unb64(&invite.manifest)?).map_err(|e| e.to_string())?;
        let m = cm::accept_first(&signed).map_err(|e| e.to_string())?;
        let me = identity.public().identity_key;
        if card.key.0 != me {
            return Err("that card isn't yours".into());
        }
        // Listed already (an invite from an older version), or an offer:
        // the inviter adds whichever key we answer with.
        if !m.is_member(&me) && !m.can(&invite.from.0, perm::INVITE, None) {
            return Err("whoever invited you can't let you in".into());
        }
        if m.is_banned(&me) {
            return Err("you're banned from it".into());
        }
        let mut mls = GroupMember::new(identity);
        let key_package = mls.key_package().map_err(|e| e.to_string())?;
        let community = m.id;
        let mut space = Self {
            me,
            chain: vec![(signed, m)],
            mls,
            cards: vec![card.clone()],
            held: Vec::new(),
            previous: None,
            joining: Some(invite.from.0),
            removed: false,
            persona,
            offers: Vec::new(),
            pace: Vec::new(),
            reports: Vec::new(),
            voice: Vec::new(),
            tickets: Vec::new(),
            board_keys: Vec::new(),
        };
        space.learn(invite.card.clone());
        let out = Out { to: invite.from.0, wire: Wire::Join { community, key_package: b64(&key_package), card } };
        Ok((space, out))
    }

    pub fn save(&self) -> Vec<u8> {
        let saved = Saved {
            manifests: self.chain.iter().map(|(s, _)| b64(&s.encode())).collect(),
            mls: b64(&self.mls.save()),
            cards: self.cards.clone(),
            held: self.held.clone(),
            previous: self.previous.clone(),
            joining: self.joining.map(Key),
            removed: self.removed,
            persona: self.persona,
            offers: self.offers.clone(),
            pace: self.pace.clone(),
            reports: self.reports.clone(),
            voice: self.voice.clone(),
            tickets: self.tickets.clone(),
            board_keys: self.board_keys.clone(),
        };
        serde_json::to_vec(&saved).expect("a space always serializes")
    }

    /// `main` is our main identity; a space we are in under a separate one
    /// derives it again here.
    pub fn restore(main: &IdentityKeyPair, bytes: &[u8]) -> Result<Self, String> {
        let saved: Saved = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let mut chain = Vec::new();
        for s in &saved.manifests {
            let signed = SignedManifest::decode(&unb64(s)?).map_err(|e| e.to_string())?;
            // Each was checked on the way in; opening again catches a
            // damaged file rather than re-judging permissions.
            let m = signed.open().map_err(|e| e.to_string())?;
            chain.push((signed, m));
        }
        if chain.is_empty() {
            return Err("no manifest".into());
        }
        let identity = identity_for(main, &chain[0].1.id, saved.persona);
        Ok(Self {
            me: identity.public().identity_key,
            chain,
            mls: GroupMember::restore(&identity, &unb64(&saved.mls)?).map_err(|e| e.to_string())?,
            cards: saved.cards,
            held: saved.held,
            previous: saved.previous,
            joining: saved.joining.map(|k| k.0),
            removed: saved.removed,
            persona: saved.persona,
            offers: saved.offers,
            pace: saved.pace,
            reports: saved.reports,
            voice: saved.voice,
            tickets: saved.tickets,
            board_keys: saved.board_keys,
        })
    }

    pub fn id(&self) -> Key {
        self.manifest().id
    }

    pub fn manifest(&self) -> &Manifest {
        &self.chain.last().expect("a space always has a manifest").1
    }

    fn signed(&self) -> &SignedManifest {
        &self.chain.last().expect("a space always has a manifest").0
    }

    pub fn epoch(&self) -> Option<u64> {
        self.mls.epoch().ok()
    }

    /// The community's board, if it has one.
    pub fn board(&self) -> Option<&cm::BoardRef> {
        self.manifest().nodes.board.as_ref()
    }

    /// The recent epochs' board keys, oldest first; the last is the
    /// current epoch's.
    pub fn board_keys(&self) -> &[EpochKey] {
        &self.board_keys
    }

    /// The board, once every member knows of it: the version before this
    /// one named it too (or we joined with this version, which the others
    /// already had). The version that first names a board still goes by
    /// mail, or nobody would know to look there.
    pub fn settled_board(&self) -> Option<&cm::BoardRef> {
        let board = self.board()?;
        let Some(before) = self.chain.len().checked_sub(2).map(|i| &self.chain[i].1) else { return Some(board) };
        (before.nodes.board.as_ref() == Some(board)).then_some(board)
    }

    /// The board key an entry for `epoch` is sealed under.
    pub fn board_key(&self, epoch: u64) -> Option<&EpochKey> {
        self.board_keys.iter().find(|k| k.epoch == epoch)
    }

    /// Remembers the current epoch's board key. Called whenever the epoch
    /// may have moved, before and after, so a commit can still be sealed
    /// under the epoch it was made in.
    fn note_epoch(&mut self) {
        if self.mls.in_group() {
            let id = self.id();
            community_board::note_epoch(&mut self.board_keys, &self.mls, &id);
        }
    }

    /// The current manifest as a commit or post carries it: in a community
    /// with a board, as the change from the version before (a
    /// [`cm::ManifestDelta`], marked `d:`), which stays small however many
    /// members there are; otherwise whole.
    fn manifest_text(&self) -> String {
        if self.board().is_some() && self.chain.len() >= 2 {
            if let Some(delta) = self.signed().delta_from(&self.chain[self.chain.len() - 2].0) {
                return format!("d:{}", b64(&delta.encode()));
            }
        }
        b64(&self.signed().encode())
    }

    /// Who can read what is sent now: the MLS group's members.
    pub fn readers(&self) -> Vec<[u8; 32]> {
        self.mls.members().map(|m| m.into_iter().map(|(_, k)| k).collect()).unwrap_or_default()
    }

    pub fn card(&self, key: &[u8; 32]) -> Option<&Card> {
        self.cards.iter().find(|c| c.key.0 == *key)
    }

    /// The name someone goes by here: their own card's, or the name the
    /// manifest was given for them.
    pub fn name_of(&self, key: &[u8; 32]) -> String {
        if let Some(c) = self.card(key).filter(|c| !c.name.is_empty()) {
            return c.name.clone();
        }
        self.manifest().member(key).map(|m| m.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| hex(&key[..4]))
    }

    fn learn(&mut self, mut card: Card) {
        if !card.valid() {
            return;
        }
        if card.avatar.as_ref().is_some_and(|a| a.len() > MAX_AVATAR) {
            card.avatar = None;
        }
        if let Some(old) = self.cards.iter().find(|c| c.key == card.key) {
            if card.avatar.is_none() {
                card.avatar = old.avatar.clone();
            }
        }
        match self.cards.iter_mut().find(|c| c.key == card.key) {
            Some(old) => {
                // A route list only moves forward.
                let newer = match (old.record(), card.record()) {
                    (Some(a), Some(b)) => b.record.sequence >= a.record.sequence,
                    _ => true,
                };
                if newer {
                    *old = card;
                }
            }
            None => self.cards.push(card),
        }
    }

    fn others_in_group(&self) -> Vec<[u8; 32]> {
        self.readers().into_iter().filter(|k| *k != self.me).collect()
    }

    fn to_all(&self, wire: Wire, skip: &[[u8; 32]]) -> Vec<Out> {
        self.others_in_group().into_iter().filter(|k| !skip.contains(k)).map(|to| Out { to, wire: wire.clone() }).collect()
    }

    fn push_manifest(&mut self, signed: SignedManifest, m: Manifest) {
        self.chain.push((signed, m));
        let excess = self.chain.len().saturating_sub(KEEP_MANIFESTS);
        self.chain.drain(..excess);
    }

    /// Signs the next version with `change` applied, if every member
    /// would accept it, and makes it current.
    fn change(&mut self, identity: &IdentityKeyPair, change: impl FnOnce(&mut Manifest)) -> Result<SignedManifest, String> {
        if self.removed || self.joining.is_some() {
            return Err("you're not in it".into());
        }
        let mut next = self.manifest().next(self.signed());
        change(&mut next);
        let signed = SignedManifest::sign(&next, identity);
        // The same check everyone else will make, so a change that would
        // be refused is refused here, with a reason, not silently there.
        let m = cm::check_transition(self.manifest(), self.signed(), &signed).map_err(|e| e.to_string())?;
        self.push_manifest(signed.clone(), m);
        Ok(signed)
    }

    fn encrypt(&mut self, post: &Post) -> Result<Wire, String> {
        let data = self.mls.encrypt(&serde_json::to_vec(post).expect("a post serializes")).map_err(|e| e.to_string())?;
        Ok(Wire::App { community: self.id(), data: b64(&data) })
    }

    fn send(&mut self, post: &Post) -> Result<Vec<Out>, String> {
        let wire = self.encrypt(post)?;
        Ok(self.to_all(wire, &[]))
    }

    /// Sends `post`, which belongs to `channel`, only to those who may
    /// read that channel (everyone, for an open one).
    fn send_in(&mut self, channel: Option<u32>, post: &Post) -> Result<Vec<Out>, String> {
        let wire = self.encrypt(post)?;
        let Some(channel) = channel else { return Ok(self.to_all(wire, &[])) };
        let m = self.manifest();
        let to: Vec<[u8; 32]> = self.others_in_group().into_iter().filter(|k| m.can_read(channel, k)).collect();
        Ok(to.into_iter().map(|to| Out { to, wire: wire.clone() }).collect())
    }

    /// A manifest change: tell everyone, then bring the group in line.
    fn publish(&mut self, signed: &SignedManifest) -> Result<Vec<Out>, String> {
        let manifest = if *signed == *self.signed() { self.manifest_text() } else { b64(&signed.encode()) };
        let mut outs = self.send(&Post::Manifest { manifest })?;
        outs.extend(self.converge(true));
        Ok(outs)
    }

    /// Removes from the MLS group anyone the manifest no longer lists.
    /// `mine`: we made the change, so we commit; otherwise only the member
    /// at the lowest leaf index still listed does, so one person does it.
    fn converge(&mut self, mine: bool) -> Vec<Out> {
        let Ok(members) = self.mls.members() else { return Vec::new() };
        let m = self.manifest();
        let gone: Vec<[u8; 32]> = members.iter().filter(|(_, k)| !m.is_member(k)).map(|(_, k)| *k).collect();
        if gone.is_empty() {
            return Vec::new();
        }
        let designate = members.iter().filter(|(_, k)| m.is_member(k)).map(|(i, k)| (*i, *k)).min();
        if !mine && designate.map(|(_, k)| k) != Some(self.me) {
            return Vec::new();
        }
        self.commit_remove(&gone).unwrap_or_default()
    }

    fn snapshot(&self, commit: &[u8], redo: Option<Redo>) -> Option<Previous> {
        Some(Previous { epoch: self.mls.epoch().ok()?, mls: b64(&self.mls.save()), commit: sha(commit), redo })
    }

    fn commit_remove(&mut self, keys: &[[u8; 32]]) -> Result<Vec<Out>, String> {
        // Everyone before the removal hears of it, the removed included,
        // so they know rather than just going quiet.
        let before = self.others_in_group();
        self.note_epoch();
        let state = b64(&self.mls.save());
        let epoch = self.mls.epoch().map_err(|e| e.to_string())?;
        let c = self.mls.remove(keys).map_err(|e| e.to_string())?;
        self.note_epoch();
        self.previous = Some(Previous {
            epoch,
            mls: state,
            commit: sha(&c.commit),
            redo: Some(Redo::Remove { keys: keys.iter().map(|k| Key(*k)).collect() }),
        });
        let wire = Wire::Commit { community: self.id(), commit: b64(&c.commit), manifest: self.manifest_text(), cards: Vec::new() };
        Ok(before.into_iter().map(|to| Out { to, wire: wire.clone() }).collect())
    }

    fn commit_add(&mut self, key_package: &[u8], card: Card) -> Result<Vec<Out>, String> {
        let before = self.others_in_group();
        self.note_epoch();
        let state = b64(&self.mls.save());
        let epoch = self.mls.epoch().map_err(|e| e.to_string())?;
        let c = self.mls.add(&[key_package]).map_err(|e| e.to_string())?;
        self.note_epoch();
        self.previous = Some(Previous {
            epoch,
            mls: state,
            commit: sha(&c.commit),
            redo: Some(Redo::Add { key_package: b64(key_package), card: card.clone() }),
        });
        let manifest = b64(&self.signed().encode());
        let change = self.manifest_text();
        let id = self.id();
        let mut outs: Vec<Out> = before
            .into_iter()
            .filter(|k| *k != card.key.0)
            .map(|to| Out { to, wire: Wire::Commit { community: id, commit: b64(&c.commit), manifest: change.clone(), cards: vec![card.slim()] } })
            .collect();
        if let Some(welcome) = c.welcome {
            outs.push(Out {
                to: card.key.0,
                wire: Wire::Welcome { community: id, welcome: b64(&welcome), manifest, cards: self.cards.iter().map(Card::slim).collect() },
            });
            outs.extend(self.avatar_for(&[card.key.0]));
        }
        Ok(outs)
    }

    // ---- what a member does ----

    /// Invites someone. Nothing is written into the manifest yet: they
    /// are listed when they accept, under whichever key they answer with
    /// (see [`Offer`]), and added to the group then.
    pub fn invite(&mut self, _identity: &IdentityKeyPair, who: [u8; 32], name: &str) -> Result<Vec<Out>, String> {
        if self.removed || self.joining.is_some() {
            return Err("you're not in it".into());
        }
        let m = self.manifest();
        if self.readers().contains(&who) {
            return Err("they're already in it".into());
        }
        if m.is_banned(&who) {
            return Err("they're banned from it".into());
        }
        if !m.can(&self.me, perm::INVITE, None) {
            return Err("you can't invite people here".into());
        }
        if !m.is_member(&who) {
            self.offers.retain(|o| o.key.0 != who);
            if self.offers.len() >= MAX_OFFERS {
                self.offers.remove(0);
            }
            self.offers.push(Offer { key: Key(who), name: name.chars().take(80).collect(), at_secs: now_secs(), knocked: false, founder: None });
        }
        // Slim: the invitee learns the picture once they are in.
        let card = self.card(&self.me).map(Card::slim).ok_or("no card of your own")?;
        Ok(vec![Out { to: who, wire: Wire::Invite { manifest: b64(&self.signed().encode()), card } }])
    }

    /// Answering a Join: if `from` was invited and isn't listed yet,
    /// lists `card.key` -- them, or the separate identity they chose.
    fn admit(&mut self, identity: &IdentityKeyPair, from: [u8; 32], card: &Card) -> Result<(), String> {
        if self.manifest().is_member(&card.key.0) {
            return Ok(());
        }
        let Some(offer) = self.offers.iter().find(|o| o.key.0 == from).cloned() else {
            return Err("a join from someone not invited".into());
        };
        // Joining as themselves: the name we know them by, as before.
        // Under a separate identity: the name they chose for it -- ours
        // for them would say who it is.
        let name = if card.key.0 == from { offer.name } else { card.name.clone() };
        let key = card.key;
        // A founding community's admin owns the shared space with us; if
        // we can't make them one, they join as anyone does.
        let owner = offer.founder.is_some() && self.manifest().is_owner(&self.me);
        self.change(identity, |m| {
            let role = m.default_role();
            m.members.push(cm::Member { key, role, name: name.chars().take(80).collect() });
            if owner {
                m.owners.push(key);
            }
        })?;
        self.offers.retain(|o| o.key.0 != from);
        Ok(())
    }

    /// Our own card, picture included, for `to` only -- for someone who
    /// just joined, since the cards in a Welcome leave pictures out.
    fn avatar_for(&mut self, to: &[[u8; 32]]) -> Vec<Out> {
        let Some(card) = self.card(&self.me).filter(|c| c.has_avatar()).cloned() else { return Vec::new() };
        let Ok(wire) = self.encrypt(&Post::Card(card)) else { return Vec::new() };
        to.iter().filter(|k| **k != self.me).map(|k| Out { to: *k, wire: wire.clone() }).collect()
    }

    /// Posts to a channel, optionally as a reply in a thread.
    pub fn post(&mut self, channel: u32, text: &str, reply_to: Option<u64>) -> Result<(Vec<Out>, Event), String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("Write something first.".into());
        }
        if !self.manifest().can(&self.me, perm::POST, None) {
            return Err("you can't post here".into());
        }
        if !self.manifest().channels.iter().any(|c| c.id == channel) {
            return Err("no such channel".into());
        }
        if !self.manifest().can_read(channel, &self.me) {
            return Err("that channel is private and you're not one of its readers".into());
        }
        let at_secs = now_secs();
        self.pace_mine(at_secs)?;
        let id = random_id();
        let outs = self.send_in(Some(channel), &Post::Message { id, channel, text: text.to_string(), reply_to, at_secs })?;
        Ok((outs, Event::Message { channel, id, sender: self.me, text: text.to_string(), reply_to, at_secs }))
    }

    /// Changes our message `id`, which is in `channel` -- so that an edit
    /// to a private channel's message goes only to its readers. `None`
    /// sends it to everyone; pass the channel whenever it is known.
    pub fn edit(&mut self, id: u64, text: &str, channel: Option<u32>) -> Result<Vec<Out>, String> {
        if text.trim().is_empty() {
            return Err("an edit can't be empty; delete it instead".into());
        }
        self.send_in(channel, &Post::Edit { id, text: text.trim().to_string() })
    }

    /// Withdraws our own message, or removes someone else's if we may.
    /// `channel` as for [`Space::edit`].
    pub fn delete(&mut self, id: u64, author: [u8; 32], channel: Option<u32>) -> Result<Vec<Out>, String> {
        if author != self.me && !self.manifest().can(&self.me, perm::MANAGE_MESSAGES, Some(&author)) {
            return Err("you can't remove their messages".into());
        }
        self.send_in(channel, &Post::Delete { id, author: Key(author) })
    }

    /// Puts our reaction `emoji` on message `id` (written by `author`, in
    /// `channel`), or takes it off. The emoji must be a Unicode emoji or
    /// one of this community's custom ones, as `:name:`.
    pub fn react(&mut self, id: u64, author: [u8; 32], channel: u32, emoji: &str, on: bool) -> Result<(Vec<Out>, Event), String> {
        let m = self.manifest();
        if !crate::reactions::is_allowed_in(emoji, m.emoji.iter().map(|e| e.name.as_str())) {
            return Err("that isn't an emoji this community has".into());
        }
        if !m.can(&self.me, perm::POST, None) {
            return Err("you can't react here".into());
        }
        if !m.can_read(channel, &self.me) {
            return Err("that channel is private and you're not one of its readers".into());
        }
        let post = Post::React { id, author: Key(author), channel, emoji: emoji.to_string(), on };
        let outs = self.send_in(Some(channel), &post)?;
        Ok((outs, Event::Reacted { id, author, sender: self.me, emoji: emoji.to_string(), on }))
    }

    pub fn share_file(&mut self, post: Post) -> Result<Vec<Out>, String> {
        let channel = match &post {
            Post::File { channel, .. } => Some(*channel),
            _ => None,
        };
        if channel.is_some_and(|c| !self.manifest().can_read(c, &self.me)) {
            return Err("that channel is private and you're not one of its readers".into());
        }
        self.pace_mine(now_secs())?;
        self.send_in(channel, &post)
    }

    // ---- voice channels ----

    /// Says we are in voice channel `channel` (or still are): everyone who
    /// can read it learns how to reach us for the call. `invite` is our
    /// invite link. Not under a separate identity, since the call itself
    /// is made with the main key and would tie the two together.
    pub fn voice_join(&mut self, channel: u32, invite: &str) -> Result<Vec<Out>, String> {
        if self.persona {
            return Err("Voice channels aren't available while you're here under a separate identity: the call would show who you are".into());
        }
        let m = self.manifest();
        if !m.channel(channel).is_some_and(|c| c.voice) {
            return Err("that isn't a voice channel".into());
        }
        if !m.can_read(channel, &self.me) {
            return Err("that channel is private and you're not one of its readers".into());
        }
        let at_secs = now_secs();
        let me = Key(self.me);
        self.voice.retain(|p| p.key != me);
        self.voice.push(Presence { channel, key: me, invite: invite.to_string(), at_secs });
        self.send_in(Some(channel), &Post::Voice { channel, here: true, invite: invite.to_string(), at_secs })
    }

    /// Says we left voice channel `channel`.
    pub fn voice_leave(&mut self, channel: u32) -> Result<Vec<Out>, String> {
        let me = Key(self.me);
        self.voice.retain(|p| p.key != me);
        self.send_in(Some(channel), &Post::Voice { channel, here: false, invite: String::new(), at_secs: now_secs() })
    }

    /// Who is in voice channel `channel` now, ourselves included, by when
    /// each last said so.
    pub fn voice_here(&self, channel: u32) -> Vec<Presence> {
        let now = now_secs();
        let m = self.manifest();
        self.voice
            .iter()
            .filter(|p| p.channel == channel && now.saturating_sub(p.at_secs) <= VOICE_TTL_SECS && m.can_read(channel, &p.key.0))
            .cloned()
            .collect()
    }

    // ---- moderation without reading anything ----

    /// The invitations we sent that are still open, oldest first.
    pub fn pending_invites(&self) -> Vec<PendingInvite> {
        let now = now_secs();
        self.offers
            .iter()
            .filter(|o| now.saturating_sub(o.at_secs) <= OFFER_LIFETIME_SECS && !self.manifest().is_member(&o.key.0))
            .map(|o| PendingInvite { key: o.key.0, name: o.name.clone(), at_secs: o.at_secs })
            .collect()
    }

    /// Withdraws our invitation to `who`: their Join is refused from now
    /// on, as from anyone never invited. Only the inviter can, since only
    /// the inviter's app answers that Join. Returns whether there was one.
    pub fn withdraw_invite(&mut self, who: [u8; 32]) -> bool {
        let before = self.offers.len();
        self.offers.retain(|o| o.key.0 != who);
        self.offers.len() != before
    }

    /// Reports messages to the moderators: the excerpts, as we read them,
    /// go sealed only to members who can act on them (remove messages,
    /// remove or ban people) and the owners. Nothing goes anywhere else;
    /// there is no one else to send it to.
    /// A ticket another community's admin brings to join this shared
    /// space as one of the communities sharing it. Only an owner gives
    /// them, and each works once, within a week.
    pub fn new_ticket(&mut self) -> Result<Key, String> {
        if !self.manifest().is_owner(&self.me) {
            return Err("only its owners can bring in another community".into());
        }
        let mut t = [0u8; 32];
        getrandom::fill(&mut t).expect("OS randomness is unavailable");
        let now = now_secs();
        self.tickets.retain(|(_, at)| now.saturating_sub(*at) <= TICKET_LIFETIME_SECS);
        if self.tickets.len() >= MAX_TICKETS {
            self.tickets.remove(0);
        }
        self.tickets.push((Key(t), now));
        Ok(Key(t))
    }

    /// Someone asking into this shared space with a [`Proof`] of the
    /// community they come from. `vouched` is whether the app found the
    /// proved key in its own copy of that community. With a ticket we
    /// gave out, their community joins the ones sharing this space and
    /// they become an owner of it once they join; without one, their
    /// community must be sharing it already. Either way they are invited,
    /// and accept as anyone does.
    pub fn ally_knock(&mut self, identity: &IdentityKeyPair, from: [u8; 32], card: &Card, proof: &Proof, ticket: Option<Key>, vouched: bool) -> Result<Vec<Out>, String> {
        let shared = self.id();
        // A ticket we gave out is its own authority: whoever brings it
        // names the community they bring. Otherwise one of that
        // community's members has to recognise them.
        if proof.check(&shared, &from).is_none() || (!vouched && ticket.is_none()) {
            return Err("they couldn't show which community they come from".into());
        }
        let m = self.manifest();
        if m.is_banned(&from) || card.key.0 != from || !card.valid() {
            return Err("they can't join".into());
        }
        if self.readers().contains(&from) {
            return Ok(Vec::new());
        }
        let community = proof.community;
        let mut outs = Vec::new();
        let founder = match ticket {
            Some(t) => {
                let now = now_secs();
                let Some(at) = self.tickets.iter().position(|(k, at)| *k == t && now.saturating_sub(*at) <= TICKET_LIFETIME_SECS) else {
                    return Err("that ticket isn't one of ours, or it ran out".into());
                };
                if !m.alliance.contains(&community) {
                    if m.alliance.len() >= cm::MAX_ALLIANCE {
                        return Err("as many communities share it as can".into());
                    }
                    outs = self.update(identity, |m| m.alliance.push(community))?;
                }
                self.tickets.remove(at);
                Some(community)
            }
            None if m.alliance.contains(&community) => None,
            None => return Err("their community doesn't share this space".into()),
        };
        let name: String = card.name.chars().take(80).collect();
        outs.append(&mut self.invite(identity, from, &name)?);
        if let Some(o) = self.offers.iter_mut().find(|o| o.key.0 == from) {
            o.knocked = true;
            o.founder = founder;
        }
        Ok(outs)
    }

    pub fn report(&mut self, channel: u32, reason: &str, excerpts: Vec<Excerpt>) -> Result<Vec<Out>, String> {
        if excerpts.is_empty() {
            return Err("pick at least one message to report".into());
        }
        let m = self.manifest();
        let to: Vec<[u8; 32]> = self.others_in_group().into_iter().filter(|k| is_moderator(m, k) && m.can_read(channel, k)).collect();
        if to.is_empty() {
            return Err("nobody else here can act on a report".into());
        }
        let excerpts = excerpts.into_iter().take(MAX_EXCERPTS).map(|e| Excerpt { text: e.text.chars().take(4000).collect(), ..e }).collect();
        let post = Post::Report { id: random_id(), channel, reason: reason.trim().chars().take(500).collect(), excerpts, at_secs: now_secs() };
        let wire = self.encrypt(&post)?;
        Ok(to.into_iter().map(|to| Out { to, wire: wire.clone() }).collect())
    }

    /// The reports waiting for us as a moderator, oldest first.
    pub fn reports(&self) -> &[Report] {
        &self.reports
    }

    /// Takes a report off the list, once it has been dealt with.
    pub fn dismiss_report(&mut self, id: u64) -> bool {
        let before = self.reports.len();
        self.reports.retain(|r| r.id != id);
        self.reports.len() != before
    }

    /// Slow mode and the flood cap, for a post of ours written at `at`:
    /// refused (with when we may post again) rather than sent and then
    /// dropped by everyone.
    fn pace_mine(&mut self, at: u64) -> Result<(), String> {
        let me = self.me;
        match self.pace_check(&me, at) {
            Ok(()) => {
                self.pace_note(me, at);
                Ok(())
            }
            Err(wait) => Err(format!("Slow mode is on here: you can post again in {}", wait_words(wait))),
        }
    }

    /// Whether a post from `who` written at `at` keeps to slow mode and
    /// the flood cap; if not, how many seconds too soon it is. Times are
    /// the ones posts carry, so mail that arrives in a batch hours later
    /// is judged by when it was written. Each check is against posts on
    /// both sides of `at`, so posts arriving out of order are fine.
    fn pace_check(&self, who: &[u8; 32], at: u64) -> Result<(), u64> {
        let m = self.manifest();
        let times = self.pace.iter().find(|p| p.key.0 == *who).map(|p| p.times.as_slice()).unwrap_or(&[]);
        // Someone who may remove messages is how a flood gets cleaned up,
        // so slow mode doesn't hold them; the flood cap still does.
        let slow = if m.can(who, perm::MANAGE_MESSAGES, None) { 0 } else { m.slow_mode_secs as u64 };
        if slow > 0 {
            if let Some(near) = times.iter().find(|&&t| t.abs_diff(at) < slow) {
                return Err(slow - near.abs_diff(at));
            }
        }
        let close = times.iter().filter(|&&t| t.abs_diff(at) < FLOOD_WINDOW_SECS).count();
        if close >= FLOOD_MAX {
            return Err(FLOOD_WINDOW_SECS);
        }
        // Older than everything remembered: too far back to judge, and
        // backdating is how a flood would slip past the checks above.
        if times.len() >= PACE_KEEP && times.first().is_some_and(|&t| at < t) {
            return Err(slow.max(FLOOD_WINDOW_SECS));
        }
        Ok(())
    }

    /// Remembers a post from `who` written at `at`.
    fn pace_note(&mut self, who: [u8; 32], at: u64) {
        let i = match self.pace.iter().position(|p| p.key.0 == who) {
            Some(i) => i,
            None => {
                self.pace.push(Pace { key: Key(who), times: Vec::new() });
                self.pace.len() - 1
            }
        };
        let times = &mut self.pace[i].times;
        let at_index = times.partition_point(|&t| t <= at);
        times.insert(at_index, at);
        if times.len() > PACE_KEEP {
            times.remove(0);
        }
        // Former members' times aren't needed.
        let m = self.chain.last().map(|(_, m)| m.clone());
        if let Some(m) = m {
            self.pace.retain(|p| m.is_member(&p.key.0));
        }
    }

    /// Whether a post from `sender` written at `at` is let through: not
    /// too far in the future, and within slow mode and the flood cap. One
    /// that is let through is remembered.
    fn pace_incoming(&mut self, sender: [u8; 32], at: u64) -> bool {
        if at > now_secs() + FUTURE_SLACK_SECS || self.pace_check(&sender, at).is_err() {
            return false;
        }
        self.pace_note(sender, at);
        true
    }

    /// Sends our card again -- after our route list changed.
    pub fn refresh_card(&mut self, card: Card) -> Result<Vec<Out>, String> {
        self.learn(card.clone());
        self.send(&Post::Card(card))
    }

    pub fn ban(&mut self, identity: &IdentityKeyPair, who: [u8; 32], reason: &str) -> Result<Vec<Out>, String> {
        let by = Key(self.me);
        let signed = self.change(identity, |m| {
            m.members.retain(|x| x.key.0 != who);
            m.bans.push(cm::Ban { key: Key(who), reason: reason.chars().take(200).collect(), by, at_secs: now_secs() });
        })?;
        self.publish(&signed)
    }

    pub fn kick(&mut self, identity: &IdentityKeyPair, who: [u8; 32]) -> Result<Vec<Out>, String> {
        let signed = self.change(identity, |m| m.members.retain(|x| x.key.0 != who))?;
        self.publish(&signed)
    }

    /// Any other manifest change: settings, channels, roles, bans lifted.
    pub fn update(&mut self, identity: &IdentityKeyPair, change: impl FnOnce(&mut Manifest)) -> Result<Vec<Out>, String> {
        let signed = self.change(identity, change)?;
        self.publish(&signed)
    }

    /// Leaves. Someone still in it removes us from the group.
    pub fn leave(&mut self, identity: &IdentityKeyPair) -> Result<Vec<Out>, String> {
        let me = self.me;
        let m = self.manifest();
        if m.is_owner(&me) && m.owners.len() == 1 && m.members.len() > 1 {
            return Err("you're its only owner; make someone else an owner first".into());
        }
        let signed = self.change(identity, |m| {
            m.members.retain(|x| x.key.0 != me);
            m.owners.retain(|o| o.0 != me);
        })?;
        let outs = self.send(&Post::Manifest { manifest: b64(&signed.encode()) })?;
        self.removed = true;
        Ok(outs)
    }

    // ---- what arrives ----

    /// Handles one message from `from` (whose identity the transport
    /// already proved).
    pub fn handle(&mut self, identity: &IdentityKeyPair, from: [u8; 32], wire: Wire) -> Handled {
        let mut h = Handled::default();
        if self.removed {
            return h;
        }
        self.note_epoch();
        let result = self.handle_inner(identity, from, wire, &mut h);
        self.note_epoch();
        match result {
            Ok(()) => {
                if h.events.iter().any(|e| matches!(e, Event::Joined | Event::Changed)) || h.outs.iter().any(|o| matches!(o.wire, Wire::Commit { .. })) {
                    self.replay_held(identity, &mut h);
                }
            }
            Err(e) => h.events.push(Event::Notice(e)),
        }
        h
    }

    fn handle_inner(&mut self, identity: &IdentityKeyPair, from: [u8; 32], wire: Wire, h: &mut Handled) -> Result<(), String> {
        if wire.community().is_some_and(|c| c != self.id()) {
            return Err("for another community".into());
        }
        // Not in the group yet: what the others say is kept for once the
        // Welcome lets us read it.
        if self.joining.is_some() && matches!(wire, Wire::Commit { .. } | Wire::App { .. }) {
            self.hold(from, wire);
            return Ok(());
        }
        match wire {
            // The app fetches it (`Store::receive`).
            Wire::Invite { .. } | Wire::BoardWelcome { .. } => Ok(()),
            // Asking into a shared space: only the app can check which
            // community they come from, against its own copy of it
            // (`Store::receive`, then `ally_knock`).
            Wire::Knock { proof: Some(_), .. } => Ok(()),
            Wire::Knock { card, .. } => {
                // A stranger from the join link: let in only while the
                // community is listed, by someone allowed to invite, and
                // never someone banned. They still accept as anyone does.
                let m = self.manifest();
                if !m.listed || !m.can(&self.me, perm::INVITE, None) || m.is_banned(&from) || card.key.0 != from || !card.valid() {
                    return Ok(());
                }
                if self.readers().contains(&from) {
                    return Ok(());
                }
                let knocked = self.offers.iter().filter(|o| o.knocked).count();
                if knocked >= MAX_KNOCKS && !self.offers.iter().any(|o| o.key.0 == from) {
                    return Ok(());
                }
                let name: String = card.name.chars().take(80).collect();
                let mut outs = self.invite(identity, from, &name)?;
                if let Some(o) = self.offers.iter_mut().find(|o| o.key.0 == from) {
                    o.knocked = true;
                }
                h.outs.append(&mut outs);
                h.events.push(Event::Notice(format!("{name} asked to join from its listing and was invited")));
                Ok(())
            }
            Wire::Join { key_package, card, .. } => {
                // `from` is who we invited, proved by the transport; the
                // card is who they join as -- themselves, or a separate
                // identity, which only an outstanding offer allows.
                let listed = self.manifest().is_member(&from) && card.key.0 == from;
                let now = now_secs();
                self.offers.retain(|o| now.saturating_sub(o.at_secs) <= OFFER_LIFETIME_SECS);
                let offered = self.offers.iter().any(|o| o.key.0 == from);
                if !(listed || offered) || !card.valid() {
                    return Err("a join from someone not invited".into());
                }
                let joiner = card.key.0;
                if self.readers().contains(&joiner) || !self.mls.in_group() {
                    return Ok(());
                }
                let kp = unb64(&key_package)?;
                if self.mls.key_package_identity(&kp).map_err(|e| e.to_string())? != joiner {
                    return Err("a join with someone else's key".into());
                }
                if !listed {
                    self.admit(identity, from, &card)?;
                }
                self.learn(card.clone());
                h.outs.extend(self.commit_add(&kp, card)?);
                h.events.push(Event::Changed);
                Ok(())
            }
            Wire::Welcome { welcome, manifest, cards, .. } => {
                if self.joining.is_none() {
                    return Ok(());
                }
                self.take_first_manifest(&manifest)?;
                self.mls.join(&unb64(&welcome)?).map_err(|e| e.to_string())?;
                let m = self.manifest();
                if self.readers().iter().any(|k| !m.is_member(k)) {
                    self.mls.reset();
                    return Err("a group with people its member list doesn't name".into());
                }
                for c in cards {
                    if self.readers().contains(&c.key.0) {
                        self.learn(c);
                    }
                }
                self.joining = None;
                h.events.push(Event::Joined);
                // Our picture, which the inviter's commit left out.
                let others = self.others_in_group();
                h.outs.extend(self.avatar_for(&others));
                Ok(())
            }
            Wire::Commit { commit, manifest, cards, .. } => {
                if !self.readers().contains(&from) {
                    return Err("a commit from outside the group".into());
                }
                self.take_manifest(identity, from, &manifest, h)?;
                let commit = unb64(&commit)?;
                let Some(epoch) = GroupMember::message_epoch(&commit) else { return Err("malformed commit".into()) };
                let current = self.mls.epoch().map_err(|e| e.to_string())?;
                if epoch > current {
                    self.hold(from, Wire::Commit { community: self.id(), commit: b64(&commit), manifest, cards });
                    return Ok(());
                }
                if epoch < current {
                    return self.competing(identity, from, &commit, epoch, cards, h);
                }
                self.apply_commit(&commit, cards, h)
            }
            Wire::App { data, .. } => {
                let data = unb64(&data)?;
                let Some(epoch) = GroupMember::message_epoch(&data) else { return Err("malformed message".into()) };
                if self.mls.epoch().is_ok_and(|current| epoch > current) {
                    self.hold(from, Wire::App { community: self.id(), data: b64(&data) });
                    return Ok(());
                }
                match self.mls.process(&data) {
                    Ok(Processed::Application { sender, data }) => self.on_post(identity, sender, &data, h),
                    Ok(_) => Ok(()),
                    // Too old, or not for us: nothing to do with it.
                    Err(_) => Ok(()),
                }
            }
            Wire::ManifestRequest { since, .. } => {
                if self.manifest().is_member(&from) {
                    let list = self.chain.iter().filter(|(_, m)| m.version > since).map(|(s, _)| b64(&s.encode())).collect();
                    h.outs.push(Out { to: from, wire: Wire::Manifests { community: self.id(), list } });
                }
                Ok(())
            }
            Wire::Manifests { list, .. } => {
                for s in list {
                    self.take_manifest(identity, from, &s, h)?;
                }
                h.outs.extend(self.converge(false));
                Ok(())
            }
        }
    }

    fn hold(&mut self, from: [u8; 32], wire: Wire) {
        if self.held.len() < MAX_HELD {
            self.held.push(Held { from: Key(from), wire });
        }
    }

    fn replay_held(&mut self, identity: &IdentityKeyPair, h: &mut Handled) {
        // Each pass may move the epoch on, letting more through.
        loop {
            let Ok(current) = self.mls.epoch() else { return };
            let ready: Vec<Held> = {
                let (now, later): (Vec<Held>, Vec<Held>) = std::mem::take(&mut self.held).into_iter().partition(|held| {
                    let bytes = match &held.wire {
                        Wire::Commit { commit, .. } => decode_b64(commit),
                        Wire::App { data, .. } => decode_b64(data),
                        _ => None,
                    };
                    bytes.and_then(|b| GroupMember::message_epoch(&b)).is_some_and(|e| e <= current)
                });
                self.held = later;
                now
            };
            if ready.is_empty() {
                return;
            }
            for held in ready {
                if let Err(e) = self.handle_inner(identity, held.from.0, held.wire, h) {
                    h.events.push(Event::Notice(e));
                }
            }
            if self.mls.epoch().ok() == Some(current) {
                return;
            }
        }
    }

    /// A commit for an epoch we already moved past. If it competes with
    /// the commit we applied there and wins, roll back and take it.
    fn competing(&mut self, identity: &IdentityKeyPair, _from: [u8; 32], commit: &[u8], epoch: u64, cards: Vec<Card>, h: &mut Handled) -> Result<(), String> {
        let Some(prev) = self.previous.clone() else { return Ok(()) };
        if prev.epoch != epoch || sha(commit) == prev.commit || sha(commit) > prev.commit {
            return Ok(()); // a duplicate, or the loser
        }
        let restored = GroupMember::restore(identity, &unb64(&prev.mls)?).map_err(|e| e.to_string())?;
        let mine = std::mem::replace(&mut self.mls, restored);
        if let Err(e) = self.apply_commit(commit, cards, h) {
            self.mls = mine;
            return Err(e);
        }
        h.events.push(Event::Notice("Two changes to the group crossed; the other one was kept.".into()));
        // If the loser was ours, make the change again on top.
        match prev.redo {
            Some(Redo::Add { key_package, card }) => {
                if !self.readers().contains(&card.key.0) && self.manifest().is_member(&card.key.0) {
                    h.outs.extend(self.commit_add(&unb64(&key_package)?, card)?);
                }
            }
            Some(Redo::Remove { .. }) => h.outs.extend(self.converge(true)),
            None => {}
        }
        Ok(())
    }

    fn apply_commit(&mut self, commit: &[u8], cards: Vec<Card>, h: &mut Handled) -> Result<(), String> {
        let before = self.snapshot(commit, None);
        let manifest = self.manifest().clone();
        let result = self.mls.process_checked(commit, |info| {
            // The manifest, not the committer, decides who is in.
            manifest.is_member(&info.committer)
                && info.added.iter().all(|k| manifest.is_member(k))
                && info.removed.iter().all(|k| !manifest.is_member(k))
        });
        match result {
            Ok(Processed::NewEpoch { removed_self, .. }) => {
                self.previous = before;
                let mut newcomers = Vec::new();
                for c in cards {
                    if self.readers().contains(&c.key.0) {
                        newcomers.push(c.key.0);
                        self.learn(c);
                    }
                }
                if !removed_self {
                    h.outs.extend(self.avatar_for(&newcomers));
                }
                if removed_self {
                    self.removed = true;
                    h.events.push(Event::Removed);
                } else {
                    h.events.push(Event::Changed);
                }
                Ok(())
            }
            Ok(_) => Ok(()),
            Err(e) => Err(format!("refused a change to the group: {e}")),
        }
    }

    /// The manifest from a Welcome: the next version of ours, or failing
    /// that one we accept on first use (see `antiphon_crypto::community`).
    fn take_first_manifest(&mut self, manifest: &str) -> Result<(), String> {
        let signed = SignedManifest::decode(&unb64(manifest)?).map_err(|e| e.to_string())?;
        if signed == *self.signed() {
            return Ok(());
        }
        let m = match cm::check_transition(self.manifest(), self.signed(), &signed) {
            Ok(m) => m,
            Err(_) => cm::accept_first(&signed).map_err(|e| e.to_string())?,
        };
        if m.id != self.id() || m.version < self.manifest().version {
            return Err("a manifest for something else".into());
        }
        self.chain = vec![(signed, m)];
        Ok(())
    }

    /// A manifest version someone sent. Newer and allowed: it becomes
    /// current. A gap: ask them for what we missed. Competing with ours:
    /// the lower hash stays.
    fn take_manifest(&mut self, _identity: &IdentityKeyPair, from: [u8; 32], manifest: &str, h: &mut Handled) -> Result<(), String> {
        let head = self.manifest().version;
        let signed = match manifest.strip_prefix("d:") {
            // A change from a version we should hold: rebuilt, then
            // judged like any other. Not holding it, ask for what we
            // missed, whole.
            Some(delta) => {
                let delta = cm::ManifestDelta::decode(&unb64(delta)?).ok_or("a malformed manifest change")?;
                match self.chain.iter().find(|(s, _)| s.hash() == delta.base).and_then(|(s, m)| delta.apply(s, m)) {
                    Some(signed) => signed,
                    None => {
                        if delta.version() > head {
                            h.outs.push(Out { to: from, wire: Wire::ManifestRequest { community: self.id(), since: head } });
                        }
                        return Ok(());
                    }
                }
            }
            None => SignedManifest::decode(&unb64(manifest)?).map_err(|e| e.to_string())?,
        };
        let Ok(incoming) = signed.open() else { return Err("a manifest with a bad signature".into()) };
        if incoming.id != self.id() {
            return Err("a manifest for another community".into());
        }
        if incoming.version <= head {
            if incoming.version == head && signed != *self.signed() && signed.beats(self.signed()) && self.chain.len() >= 2 {
                let (ps, pm) = &self.chain[self.chain.len() - 2];
                if let Ok(m) = cm::check_transition(pm, ps, &signed) {
                    let lost = self.chain.pop().expect("checked above");
                    if lost.0.signer == self.me {
                        h.events.push(Event::Notice("Someone else changed the community at the same moment; your change was dropped. Make it again.".into()));
                    }
                    self.chain.push((signed, m));
                    h.events.push(Event::Changed);
                }
            }
            return Ok(());
        }
        if incoming.version > head + 1 {
            h.outs.push(Out { to: from, wire: Wire::ManifestRequest { community: self.id(), since: head } });
            return Ok(());
        }
        let m = cm::check_transition(self.manifest(), self.signed(), &signed).map_err(|e| format!("refused a change: {e}"))?;
        let was_member = m.is_member(&self.me);
        if m.is_banned(&self.me) {
            h.events.push(Event::Notice(format!("You were banned from {}.", m.name)));
        } else if !was_member {
            h.events.push(Event::Notice(format!("You were removed from {}.", m.name)));
        }
        self.push_manifest(signed, m);
        h.events.push(Event::Changed);
        Ok(())
    }

    fn on_post(&mut self, identity: &IdentityKeyPair, sender: [u8; 32], data: &[u8], h: &mut Handled) -> Result<(), String> {
        let post: Post = serde_json::from_slice(data).map_err(|_| "an unreadable post".to_string())?;
        // A copy: pacing below changes what is remembered about the sender.
        let m = &self.manifest().clone();
        // A manifest that removes its own signer is still theirs to send.
        if !m.is_member(&sender) && !matches!(post, Post::Manifest { .. }) {
            return Ok(());
        }
        match post {
            Post::Message { id, channel, text, reply_to, at_secs } => {
                // In a private channel, only from a reader, and only if we
                // are one -- a message there from anyone else is dropped.
                let private = m.channel(channel).is_some_and(|c| c.readers.is_some());
                if private && !(m.can_read(channel, &sender) && m.can_read(channel, &self.me)) {
                    return Ok(());
                }
                if m.can(&sender, perm::POST, None) {
                    let channel = if m.channels.iter().any(|c| c.id == channel) { channel } else { m.channels[0].id };
                    let text: String = text.chars().take(4000).collect();
                    if !self.pace_incoming(sender, at_secs) {
                        return Ok(());
                    }
                    h.events.push(Event::Message { channel, id, sender, text, reply_to, at_secs });
                }
            }
            Post::Edit { id, text } => h.events.push(Event::Edited { id, sender, text: text.chars().take(4000).collect() }),
            Post::Delete { id, author } => {
                if author.0 == sender || m.can(&sender, perm::MANAGE_MESSAGES, Some(&author.0)) {
                    h.events.push(Event::Deleted { id, author: author.0 });
                }
            }
            Post::React { id, author, channel, emoji, on } => {
                // As for a message: from someone who may post, about a
                // channel both of us can read, with an emoji this
                // community knows -- a reaction is never a place for text.
                let private = m.channel(channel).is_some_and(|c| c.readers.is_some());
                let readable = !private || (m.can_read(channel, &sender) && m.can_read(channel, &self.me));
                if readable
                    && m.can(&sender, perm::POST, None)
                    && crate::reactions::is_allowed_in(&emoji, m.emoji.iter().map(|e| e.name.as_str()))
                {
                    h.events.push(Event::Reacted { id, author: author.0, sender, emoji, on });
                }
            }
            Post::File { id, channel, name, size, file_id, key, holders, at_secs } => {
                let private = m.channel(channel).is_some_and(|c| c.readers.is_some());
                if private && !(m.can_read(channel, &sender) && m.can_read(channel, &self.me)) {
                    return Ok(());
                }
                if m.can(&sender, perm::POST, None) {
                    if !self.pace_incoming(sender, at_secs) {
                        return Ok(());
                    }
                    h.events.push(Event::File { channel, id, sender, name, size, file_id, key, holders, at_secs });
                }
            }
            Post::Card(card) => {
                if card.key.0 == sender {
                    self.learn(card);
                }
            }
            Post::Manifest { manifest } => {
                self.take_manifest(identity, sender, &manifest, h)?;
                h.outs.extend(self.converge(false));
            }
            Post::Voice { channel, here, invite, at_secs } => {
                // Stale ones (mail that waited while we were away) say
                // nothing about now; one from the future is a lie.
                let now = now_secs();
                if !m.channel(channel).is_some_and(|c| c.voice) || !m.can_read(channel, &sender) || at_secs > now + FUTURE_SLACK_SECS {
                    return Ok(());
                }
                let key = Key(sender);
                if self.voice.iter().any(|p| p.key == key && p.at_secs > at_secs) {
                    return Ok(());
                }
                self.voice.retain(|p| p.key != key);
                if here && now.saturating_sub(at_secs) <= VOICE_TTL_SECS && invite.len() <= 2048 {
                    self.voice.push(Presence { channel, key, invite, at_secs });
                }
                self.voice.retain(|p| now.saturating_sub(p.at_secs) <= VOICE_TTL_SECS);
                h.events.push(Event::Changed);
            }
            Post::Report { id, channel, reason, excerpts, at_secs } => {
                // Only a moderator keeps it; and a flood of reports is still
                // a flood.
                if !is_moderator(m, &self.me) || self.reports.iter().any(|r| r.id == id) || !self.pace_incoming(sender, at_secs) {
                    return Ok(());
                }
                let excerpts: Vec<Excerpt> =
                    excerpts.into_iter().take(MAX_EXCERPTS).map(|e| Excerpt { text: e.text.chars().take(4000).collect(), ..e }).collect();
                let report = Report { id, from: Key(sender), channel, reason: reason.chars().take(500).collect(), excerpts, at_secs };
                if self.reports.len() >= MAX_REPORTS {
                    self.reports.remove(0);
                }
                self.reports.push(report.clone());
                h.events.push(Event::Reported(report));
            }
        }
        Ok(())
    }
}

/// Puts `by`'s reaction `emoji` on a stored message's list (`on`), or
/// takes it off. Putting on one already there, or taking off one that
/// isn't, changes nothing, so a repeated delivery is harmless. A member
/// already at [`MAX_REACTIONS_EACH`] on this message adds no more.
pub fn set_reaction(list: &mut Vec<crate::history::StoredReaction>, emoji: &str, by: &str, on: bool) {
    let at = list.iter().position(|r| r.emoji == emoji && r.by == by);
    match (on, at) {
        (true, None) if list.iter().filter(|r| r.by == by).count() < MAX_REACTIONS_EACH => {
            list.push(crate::history::StoredReaction { emoji: emoji.to_string(), by: by.to_string() });
        }
        (false, Some(at)) => {
            list.remove(at);
        }
        _ => {}
    }
}

/// Whether `key` can act on a report: an owner, or a member who may
/// remove messages or remove or ban people.
pub fn is_moderator(m: &Manifest, key: &[u8; 32]) -> bool {
    m.is_member(key) && (m.is_owner(key) || m.perms(key) & (perm::MANAGE_MESSAGES | perm::KICK | perm::BAN) != 0)
}

/// "12 seconds", "3 minutes", "2 hours": how long to wait, plainly.
fn wait_words(secs: u64) -> String {
    let (n, unit) = if secs < 90 {
        (secs.max(1), "second")
    } else if secs < 90 * 60 {
        (secs.div_ceil(60), "minute")
    } else {
        (secs.div_ceil(3600), "hour")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

fn random_id() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("OS randomness is unavailable");
    u64::from_le_bytes(b) >> 1
}

// ---- cutting a Wire to fit a mailbox ---------------------------------------

/// Bytes of a [`Wire`] per fragment: with its header, base64 and the
/// mail body around it, a sealed fragment fits `mailbox::MAX_BLOB`.
pub const FRAGMENT_BYTES: usize = 540;
/// Most fragments in one message: a Welcome into a community of thousands
/// (which goes by its board) carries the whole group's tree and cards.
const MAX_FRAGMENTS: usize = 4000;
const MAX_PARTIAL: usize = 128;

/// A [`Wire`] as base64 fragments: `msg id (8) | part (2) | parts (2) | data`.
pub fn fragments(wire: &Wire) -> Vec<String> {
    let bytes = serde_json::to_vec(wire).expect("a wire message serializes");
    let msg = random_id().to_be_bytes();
    let chunks: Vec<&[u8]> = bytes.chunks(FRAGMENT_BYTES).collect();
    let parts = chunks.len() as u16;
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut f = msg.to_vec();
            f.extend_from_slice(&(i as u16).to_be_bytes());
            f.extend_from_slice(&parts.to_be_bytes());
            f.extend_from_slice(c);
            b64(&f)
        })
        .collect()
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Partial {
    from: Key,
    msg: u64,
    parts: Vec<Option<String>>,
    first_secs: u64,
}

/// Fragments waiting for the rest of their message. Persisted, since a
/// fragment is deleted from its mailbox once collected.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Reassembly {
    partial: Vec<Partial>,
}

impl Reassembly {
    /// Adds one fragment; the whole message once it is complete.
    pub fn feed(&mut self, from: [u8; 32], fragment: &str) -> Option<Wire> {
        let raw = decode_b64(fragment)?;
        if raw.len() < 12 {
            return None;
        }
        let msg = u64::from_be_bytes(raw[..8].try_into().ok()?);
        let part = u16::from_be_bytes(raw[8..10].try_into().ok()?) as usize;
        let parts = u16::from_be_bytes(raw[10..12].try_into().ok()?) as usize;
        if parts == 0 || part >= parts || parts > MAX_FRAGMENTS {
            return None;
        }
        let data = &raw[12..];
        if parts == 1 {
            return serde_json::from_slice(data).ok();
        }
        let at = match self.partial.iter().position(|p| p.from.0 == from && p.msg == msg) {
            Some(at) => at,
            None => {
                if self.partial.len() >= MAX_PARTIAL {
                    self.partial.remove(0);
                }
                self.partial.push(Partial { from: Key(from), msg, parts: vec![None; parts], first_secs: now_secs() });
                self.partial.len() - 1
            }
        };
        let p = &mut self.partial[at];
        if p.parts.len() != parts {
            return None;
        }
        p.parts[part] = Some(b64(data));
        if p.parts.iter().any(Option::is_none) {
            return None;
        }
        let p = self.partial.remove(at);
        let mut whole = Vec::new();
        for piece in p.parts.into_iter().flatten() {
            whole.extend(decode_b64(&piece)?);
        }
        serde_json::from_slice(&whole).ok()
    }

    /// Drops what has waited more than two weeks -- longer than any
    /// mailbox keeps the rest.
    pub fn expire(&mut self, now: u64) {
        self.partial.retain(|p| now.saturating_sub(p.first_secs) < 14 * 86_400);
    }
}

// ---- the app's side: storage, invites, delivery ------------------------------

/// An invite waiting for an answer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Invite {
    pub from: Key,
    pub manifest: String,
    pub card: Card,
    pub name: String,
    pub kind: Kind,
    pub at_secs: u64,
}

/// Which messages in a space or channel notify you. Unread counts go up
/// either way: this is about being interrupted, not about what you see.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Notify {
    /// Every message from someone else.
    #[default]
    All,
    /// Only messages that name you (`@` and the name you go by there).
    Mentions,
    /// None.
    Nothing,
}

/// Whether `text` names `name` the way people mention someone: `@name`,
/// in any case, not followed by more letters (so `@sam` doesn't match
/// `@samantha`).
pub fn mentions(text: &str, name: &str) -> bool {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        return false;
    }
    let text = text.to_lowercase();
    let needle = format!("@{name}");
    text.match_indices(&needle).any(|(at, _)| !text[at + needle.len()..].chars().next().is_some_and(char::is_alphanumeric))
}

/// One fragment waiting to leave.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Queued {
    to: Key,
    id: u64,
    fragment: String,
    #[serde(default)]
    attempts: u32,
    /// Sent as the separate identity we use in this community, rather
    /// than as our main one -- and so never over a live call, which would
    /// say who sent it.
    #[serde(default)]
    persona: Option<Key>,
}

/// One entry waiting to be posted to a community's board.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct BoardQueued {
    space: Key,
    /// The sealed entry, base64.
    entry: String,
    /// On the first entry of a Welcome posted for a newcomer: who to tell
    /// where it starts once it is up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    welcome: Option<WelcomePointer>,
}

/// Where a Welcome on the board starts, for its newcomer: who, and the
/// epoch key it is sealed under.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct WelcomePointer {
    to: Key,
    key: EpochKey,
}

/// How far we are with one community's board.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct BoardState {
    /// Which board this is about: when the community moves to another,
    /// reading starts afresh there.
    #[serde(default)]
    board: Option<Key>,
    /// The last entry read; `None` before the first look.
    seen: Option<u64>,
    /// The newest epoch whose token the node is known to accept.
    allowed: Option<u64>,
    /// Looks spent waiting at an entry from an epoch we haven't reached.
    stuck: u32,
}

/// A newcomer's Welcome on a board (from [`Wire::BoardWelcome`]), to fetch
/// while the space is still joining.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct BoardPointer {
    space: Key,
    from: Key,
    board: cm::BoardRef,
    key: EpochKey,
    since: u64,
    at_secs: u64,
}

/// A Welcome longer than this many fragments goes by the board, when the
/// community has one.
const WELCOME_INLINE: usize = if cfg!(test) { 0 } else { 8 };
/// Most board entries waiting to be posted, across communities.
const MAX_BOARD_OUT: usize = 20_000;
/// Pages read from one board per look, so one busy community can't hold
/// up the courier; the rest comes next time.
const BOARD_PAGES_PER_LOOK: usize = 32;
/// How many looks an entry from an epoch we haven't reached is waited
/// for before it is skipped (its commit never came, or we lost it).
const BOARD_STUCK_LOOKS: u32 = 20;
/// How long a newcomer keeps trying to fetch a Welcome from a board.
const BOARD_WELCOME_LIFETIME_SECS: u64 = 14 * 86_400;

/// How you appear in one group chat or community (spec §7 "per-community
/// identity"). Empty fields mean "as in my profile".
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Appearance {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    /// A small JPEG, base64, or empty for none. Your profile picture is
    /// never sent anywhere, so the default here is no picture either.
    #[serde(default)]
    pub avatar: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Hub {
    #[serde(default)]
    invites: Vec<Invite>,
    #[serde(default)]
    reassembly: Reassembly,
    #[serde(default)]
    outbox: Vec<Queued>,
    /// Unread counts by community id hex.
    #[serde(default)]
    unread: HashMap<String, u32>,
    /// Your ring rule for each, by id hex (spec §9 "per-chat and
    /// per-community ring settings"), as `RingRule::to_i32`. Absent means
    /// your default. Kept here, not in the manifest: it is yours alone.
    #[serde(default)]
    ring: HashMap<String, i32>,
    /// How you appear in each, by id hex. Yours alone, like `ring`.
    #[serde(default)]
    appear: HashMap<String, Appearance>,
    /// Which messages notify you (spec §13 "per-community and per-channel
    /// notification settings"): by id hex for a whole space, and by
    /// `"<id hex>/<channel>"` for one channel, overriding its space's.
    /// Absent means the default ([`Notify::All`], or the space's for a
    /// channel). Yours alone, like `ring`.
    #[serde(default)]
    notify: HashMap<String, Notify>,
    /// Cards of people outside our spaces we need to mail: a stranger who
    /// asked to join, or the host of a join link we used. Newest last.
    #[serde(default)]
    introduced: Vec<Card>,
    /// Communities we asked to join from a join link, and when: their
    /// invitation is taken from a stranger for a while.
    #[serde(default)]
    knocked: Vec<(Key, u64)>,
    /// Shared spaces we asked into for one of our communities with a
    /// ticket: (shared space, our community). Once we accept its
    /// invitation, the shared space goes on our community's list.
    #[serde(default)]
    allying: Vec<(Key, Key)>,
    /// Entries waiting for their community's board, oldest first.
    #[serde(default)]
    board_out: Vec<BoardQueued>,
    /// How far we are with each board, by community id hex.
    #[serde(default)]
    boards: HashMap<String, BoardState>,
    /// Welcomes waiting on a board for us.
    #[serde(default)]
    board_welcomes: Vec<BoardPointer>,
}

/// Keeps a stranger's card, newest last, replacing an older one of theirs.
fn remember_card(hub: &mut Hub, card: Card) {
    hub.introduced.retain(|c| c.key != card.key);
    hub.introduced.push(card);
    if hub.introduced.len() > MAX_INTRODUCED {
        hub.introduced.remove(0);
    }
}

/// Cards of strangers kept, for answering them.
const MAX_INTRODUCED: usize = 64;
/// How long after asking to join an invitation from its host is taken.
const KNOCK_LIFETIME_SECS: u64 = 7 * 24 * 60 * 60;

/// A listed community's join link, as a directory shows it: which
/// community, and the card of the member who made it, so a stranger can
/// mail them.
#[derive(Clone, Debug, PartialEq)]
pub struct JoinLink {
    /// The community.
    pub community: Key,
    /// Who answers: their key, identity and route list.
    pub host: Card,
}

/// How a join link starts.
pub const JOIN_PREFIX: &str = "stoop://join/";
/// How a link for another community's admin to join a shared space
/// starts: then the ticket in hex, a dot, and the join link's body.
pub const ALLY_PREFIX: &str = "stoop://ally/";

/// The ticket and join link in a link for a shared space.
pub fn parse_ally_link(link: &str) -> Option<(Key, JoinLink)> {
    let (ticket, body) = link.trim().strip_prefix(ALLY_PREFIX)?.split_once('.')?;
    let ticket = Key(unhex(ticket)?.try_into().ok()?);
    Some((ticket, JoinLink::parse(&format!("{JOIN_PREFIX}{body}"))?))
}

impl JoinLink {
    /// `stoop://join/` and, in URL-safe base64: the community's id, the
    /// host's encoded identity (length-prefixed), and their route list.
    pub fn to_link(&self) -> String {
        let public = unhex(&self.host.public).unwrap_or_default();
        let routes = unhex(&self.host.routes).unwrap_or_default();
        let mut raw = self.community.0.to_vec();
        raw.extend_from_slice(&(public.len() as u16).to_be_bytes());
        raw.extend_from_slice(&public);
        raw.extend_from_slice(&routes);
        format!("{JOIN_PREFIX}{}", encode_b64(&raw).replace('+', "-").replace('/', "_").trim_end_matches('='))
    }

    /// Reads one back; `None` for anything that isn't a valid join link.
    pub fn parse(link: &str) -> Option<Self> {
        let body = link.trim().strip_prefix(JOIN_PREFIX)?;
        let mut b64 = body.replace('-', "+").replace('_', "/");
        while b64.len() % 4 != 0 {
            b64.push('=');
        }
        let raw = decode_b64(&b64)?;
        if raw.len() < 34 {
            return None;
        }
        let community = Key(raw[..32].try_into().ok()?);
        let n = u16::from_be_bytes(raw[32..34].try_into().ok()?) as usize;
        let public = raw.get(34..34 + n)?;
        let routes = &raw[34 + n..];
        let identity = PublicIdentity::decode(public).ok()?;
        let host = Card {
            key: Key(identity.identity_key),
            public: hex(public),
            routes: hex(routes),
            name: String::new(),
            status: String::new(),
            avatar: None,
        };
        host.valid().then_some(Self { community, host })
    }
}

const MAX_OUTBOX: usize = 20_000;
const MAX_INVITES: usize = 50;

static LOCK: Mutex<()> = Mutex::new(());
static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bumped whenever anything here changes, so the UI knows to redraw.
pub fn revision() -> u64 {
    REVISION.load(std::sync::atomic::Ordering::Relaxed)
}

fn bump() {
    REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn dir(config_dir: &Path) -> PathBuf {
    config_dir.join("communities")
}

/// Where a space's (or one channel's) notification setting is kept in
/// the hub.
fn notify_key(id: &Key, channel: Option<u32>) -> String {
    match channel {
        Some(c) => format!("{}/{c}", hex(&id.0)),
        None => hex(&id.0),
    }
}

/// Which messages in this channel notify you: its own setting, else its
/// space's. A space muted with the older ring rule "Never" (before these
/// settings existed) stays quiet.
fn notify_level(hub: &Hub, id: &Key, channel: u32) -> Notify {
    if let Some(l) = hub.notify.get(&notify_key(id, Some(channel))) {
        return *l;
    }
    if let Some(l) = hub.notify.get(&notify_key(id, None)) {
        return *l;
    }
    if hub.ring.get(&hex(&id.0)).copied() == Some(crate::notify::RingRule::Never.to_i32()) {
        return Notify::Nothing;
    }
    Notify::All
}

fn space_path(config_dir: &Path, id: &Key) -> PathBuf {
    dir(config_dir).join(format!("{}.space", hex(&id.0)))
}

/// Where a channel's messages live: the ordinary history store (spec §9
/// "search over your own history"), under a name derived from the
/// community and channel so it can't collide with a contact's.
pub fn channel_log(id: &Key, channel: u32) -> String {
    let mut h = Sha256::new();
    h.update(b"antiphon/community/channel/v1");
    h.update(id.0);
    h.update(channel.to_be_bytes());
    hex(&h.finalize())
}

fn write_sealed(path: &Path, key: &BackupKey, plain: &[u8]) -> std::io::Result<()> {
    let sealed = key.seal_content(plain).map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::create_dir_all(path.parent().expect("has a parent"))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, sealed)?;
    std::fs::rename(&tmp, path)
}

fn read_sealed(path: &Path, key: &BackupKey) -> Option<Vec<u8>> {
    key.open_content(&std::fs::read(path).ok()?).ok()
}

/// Everything the app side needs: where to keep things and as whom.
pub struct Store {
    pub dir: PathBuf,
    pub key: BackupKey,
    pub identity: Arc<IdentityKeyPair>,
}

impl Store {
    /// The app's own store, once the identity is loaded.
    pub fn app() -> Option<Self> {
        let dir = crate::config::config_dir()?;
        let key = crate::history::load_or_create_backup_key(&dir).ok()?;
        Some(Self { dir, key, identity: crate::delivery::identity()? })
    }

    fn hub(&self) -> Hub {
        read_sealed(&dir(&self.dir).join("hub"), &self.key).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save_hub(&self, hub: &Hub) {
        let _ = write_sealed(&dir(&self.dir).join("hub"), &self.key, &serde_json::to_vec(hub).expect("serializes"));
    }

    pub fn load(&self, id: &Key) -> Option<Space> {
        Space::restore(&self.identity, &read_sealed(&space_path(&self.dir, id), &self.key)?).ok()
    }

    pub fn save(&self, space: &Space) -> std::io::Result<()> {
        write_sealed(&space_path(&self.dir, &space.id()), &self.key, &space.save())
    }

    pub fn ids(&self) -> Vec<Key> {
        let Ok(entries) = std::fs::read_dir(dir(&self.dir)) else { return Vec::new() };
        entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let stem = name.strip_suffix(".space")?;
                crate::connection_mode::decode_hex32(stem).map(Key)
            })
            .collect()
    }

    /// The identity we act as in `space`.
    pub fn identity_in(&self, space: &Space) -> IdentityKeyPair {
        identity_for(&self.identity, &space.id(), space.persona)
    }

    /// Our card for a space: as `identity` (main or separate), with the
    /// name, status and picture chosen for it.
    pub fn card_for(&self, id: &Key, identity: &IdentityKeyPair) -> Card {
        let config = crate::config::load();
        let record = crate::delivery::my_record(identity, &config.routes, config.strict_mode, config.route_sequence);
        card_as(identity, crate::delivery::encode_record(&record), &config.profile, &self.appearance(id))
    }

    pub fn appearance(&self, id: &Key) -> Appearance {
        self.hub().appear.get(&hex(&id.0)).cloned().unwrap_or_default()
    }

    /// Changes how you appear in one space, and tells its members.
    pub fn set_appearance(&self, id: &Key, look: Appearance) -> Result<(), String> {
        if look.avatar.len() > MAX_AVATAR {
            return Err("that picture is too big to send".into());
        }
        {
            let _guard = LOCK.lock().unwrap();
            let mut hub = self.hub();
            if look == Appearance::default() {
                hub.appear.remove(&hex(&id.0));
            } else {
                hub.appear.insert(hex(&id.0), look);
            }
            self.save_hub(&hub);
        }
        self.with_space(id, |space, identity| {
            let card = self.card_for(&space.id(), identity);
            if space.removed || space.joining.is_some() {
                // Not in yet: the card goes with the next thing we send.
                space.learn(card);
                return Ok(((), Vec::new(), vec![Event::Changed]));
            }
            Ok(((), space.refresh_card(card)?, vec![Event::Changed]))
        })
    }

    fn queue(&self, hub: &mut Hub, outs: Vec<Out>, me: [u8; 32], persona: Option<Key>) {
        for out in outs {
            if out.to == self.identity.public().identity_key || out.to == me {
                continue;
            }
            for fragment in fragments(&out.wire) {
                if hub.outbox.len() < MAX_OUTBOX {
                    hub.outbox.push(Queued { to: Key(out.to), id: random_id(), fragment, attempts: 0, persona });
                }
            }
        }
    }

    /// Takes what goes to everyone in a community with a board off `outs`
    /// and queues it for the board instead: sealed once, posted once.
    /// Returns what still goes member by member (a private channel's
    /// readers, someone joining, a reply to one person).
    ///
    /// A commit is sealed under the epoch it was made in, which everyone
    /// it is for (the removed included) can still read; a message under
    /// its own epoch; a big Welcome under the epoch its newcomer joins in,
    /// whose key goes to them by mail ([`Wire::BoardWelcome`]).
    fn to_board(&self, hub: &mut Hub, space: &Space, identity: &IdentityKeyPair, outs: Vec<Out>) -> Vec<Out> {
        let Some(board) = space.settled_board() else { return outs };
        if space.removed || space.joining.is_some() || crate::config::load().strict_mode {
            return outs;
        }
        let everyone = space.others_in_group();
        let mut groups: Vec<(Wire, Vec<[u8; 32]>)> = Vec::new();
        for out in outs {
            match groups.iter_mut().find(|(w, _)| *w == out.wire) {
                Some((_, to)) => to.push(out.to),
                None => groups.push((out.wire, vec![out.to])),
            }
        }
        let mut rest = Vec::new();
        for (wire, to) in groups {
            let epoch_of = |b64: &str| decode_b64(b64).and_then(|b| GroupMember::message_epoch(&b));
            let (epoch, welcome) = match &wire {
                Wire::Commit { commit, .. } => (epoch_of(commit), None),
                Wire::App { data, .. } if !everyone.is_empty() && everyone.iter().all(|k| to.contains(k)) => (epoch_of(data), None),
                Wire::Welcome { .. } if to.len() == 1 && fragments(&wire).len() > WELCOME_INLINE => (space.epoch(), Some(Key(to[0]))),
                _ => (None, None),
            };
            let Some(key) = epoch.and_then(|e| space.board_key(e)).cloned() else {
                rest.extend(to.into_iter().map(|to| Out { to, wire: wire.clone() }));
                continue;
            };
            for (i, fragment) in fragments(&wire).into_iter().enumerate() {
                if hub.board_out.len() >= MAX_BOARD_OUT {
                    break;
                }
                let entry = b64(&community_board::seal(&board.id, &key, identity, &fragment));
                let welcome = welcome.filter(|_| i == 0).map(|to| WelcomePointer { to, key: key.clone() });
                hub.board_out.push(BoardQueued { space: space.id(), entry, welcome });
            }
        }
        rest
    }

    /// Records what a space reported: messages into channel history,
    /// edits and deletes checked against who wrote what.
    fn record(&self, space: &Space, events: &[Event], hub: &mut Hub) {
        let me = space.me();
        let id = space.id();
        for e in events {
            match e {
                Event::Message { channel, id: mid, sender, text, reply_to, at_secs } => {
                    let log = channel_log(&id, *channel);
                    let mut all = crate::history::load(&self.dir, &log, &self.key);
                    if all.iter().any(|m| m.id == *mid && m.sender.as_deref() == Some(hex(sender).as_str())) {
                        continue;
                    }
                    all.push(crate::history::StoredMessage {
                        id: *mid,
                        from_me: *sender == me,
                        text: text.clone(),
                        at_secs: *at_secs,
                        sender: Some(hex(sender)),
                        reply_to: *reply_to,
                        ..Default::default()
                    });
                    let _ = crate::history::save(&self.dir, &log, &self.key, &all);
                    if *sender != me {
                        *hub.unread.entry(hex(&id.0)).or_default() += 1;
                        // Counted either way; whether it interrupts is the
                        // channel's setting, or the space's.
                        let level = notify_level(hub, &id, *channel);
                        let wanted = match level {
                            Notify::All => true,
                            Notify::Mentions => mentions(text, &space.name_of(&me)),
                            Notify::Nothing => false,
                        };
                        if wanted {
                            crate::delivery::notice(format!("New message in {} from {}.", space.manifest().name, space.name_of(sender)));
                        }
                    }
                }
                Event::File { channel, id: mid, sender, name, size, file_id, key, holders, at_secs } => {
                    let log = channel_log(&id, *channel);
                    let mut all = crate::history::load(&self.dir, &log, &self.key);
                    all.push(crate::history::StoredMessage {
                        id: *mid,
                        from_me: *sender == me,
                        text: format!("[file] {name} ({} KB) -- under Shared files", size.div_ceil(1024)),
                        at_secs: *at_secs,
                        sender: Some(hex(sender)),
                        ..Default::default()
                    });
                    let _ = crate::history::save(&self.dir, &log, &self.key, &all);
                    if *sender != me {
                        crate::sharing::receive_link(name.clone(), *size, file_id.clone(), key.clone(), holders.clone(), None, format!("community:{}", hex(&id.0)));
                    }
                }
                Event::Edited { id: mid, sender, text } => self.rewrite(space, *mid, sender, |m| {
                    m.text = text.clone();
                    m.edited = true;
                }),
                Event::Deleted { id: mid, author } => self.rewrite(space, *mid, author, |m| {
                    m.text = String::new();
                    m.deleted = true;
                    // Reactions to a message that is gone are about nothing.
                    m.reactions.clear();
                }),
                Event::Reacted { id: mid, author, sender, emoji, on } => {
                    let by = hex(sender);
                    self.rewrite(space, *mid, author, |m| set_reaction(&mut m.reactions, emoji, &by, *on));
                }
                Event::Removed => crate::delivery::notice(format!("You're no longer in {}.", space.manifest().name)),
                Event::Notice(n) => crate::delivery::notice(format!("{}: {n}", space.manifest().name)),
                Event::Joined => crate::delivery::notice(format!("You joined {}.", space.manifest().name)),
                Event::Reported(_) => crate::delivery::notice(format!("Messages in {} were reported to you. Open its Moderation to see them.", space.manifest().name)),
                Event::Changed => {}
            }
        }
    }

    /// Changes a message in whichever channel holds it, if `author` wrote it.
    fn rewrite(&self, space: &Space, mid: u64, author: &[u8; 32], f: impl Fn(&mut crate::history::StoredMessage)) {
        let who = hex(author);
        for c in &space.manifest().channels {
            let log = channel_log(&space.id(), c.id);
            let mut all = crate::history::load(&self.dir, &log, &self.key);
            if let Some(m) = all.iter_mut().find(|m| m.id == mid && m.sender.as_deref() == Some(who.as_str()) && !m.deleted) {
                f(m);
                let _ = crate::history::save(&self.dir, &log, &self.key, &all);
                return;
            }
        }
    }

    /// Runs `f` on a stored space, saves it, queues what it sends and
    /// records what it reports.
    pub fn with_space<T>(&self, id: &Key, f: impl FnOnce(&mut Space, &IdentityKeyPair) -> Result<(T, Vec<Out>, Vec<Event>), String>) -> Result<T, String> {
        let _guard = LOCK.lock().unwrap();
        let mut space = self.load(id).ok_or("couldn't open it")?;
        let identity = self.identity_in(&space);
        let (value, outs, events) = f(&mut space, &identity)?;
        self.save(&space).map_err(|e| e.to_string())?;
        let mut hub = self.hub();
        self.record(&space, &events, &mut hub);
        let outs = self.to_board(&mut hub, &space, &identity, outs);
        self.queue(&mut hub, outs, space.me(), space.persona.then_some(space.id()));
        self.save_hub(&hub);
        drop(_guard);
        bump();
        crate::delivery::kick();
        Ok(value)
    }

    /// A new group chat or community. With `persona`, we own it under a
    /// separate identity derived for it, and our main key appears nowhere
    /// in it.
    pub fn create(&self, kind: Kind, name: &str, persona: bool) -> Result<Key, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Give it a name first.".into());
        }
        let mut id = [0u8; 32];
        getrandom::fill(&mut id).expect("OS randomness is unavailable");
        let id = Key(id);
        let identity = identity_for(&self.identity, &id, persona);
        let card = self.card_for(&id, &identity);
        let space = Space::create(&identity, id, kind, name, card, persona)?;
        let _guard = LOCK.lock().unwrap();
        self.save(&space).map_err(|e| e.to_string())?;
        bump();
        Ok(space.id())
    }

    pub fn invites(&self) -> Vec<Invite> {
        self.hub().invites
    }

    /// Accepts an invite, as ourselves or -- with `persona` -- under a
    /// separate identity for that community. The answer goes to the
    /// inviter sealed with our main key either way: it is them we were
    /// invited by, and they alone learn which key we chose.
    pub fn accept_invite(&self, index: usize, persona: bool) -> Result<Key, String> {
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        if index >= hub.invites.len() {
            return Err("that invite is gone".into());
        }
        let invite = hub.invites.remove(index);
        let id = SignedManifest::decode(&unb64(&invite.manifest)?).and_then(|s| s.open()).map_err(|e| e.to_string())?.id;
        let identity = identity_for(&self.identity, &id, persona);
        let card = self.card_for(&id, &identity);
        let (space, out) = Space::accept(&identity, &invite, card, persona)?;
        if self.load(&space.id()).is_some_and(|s| !s.removed) {
            self.save_hub(&hub);
            return Err("you're already in it".into());
        }
        self.save(&space).map_err(|e| e.to_string())?;
        self.queue(&mut hub, vec![out], space.me(), None);
        let home = hub.allying.iter().find(|(s, _)| *s == id).map(|(_, h)| *h);
        hub.allying.retain(|(s, _)| *s != id);
        self.save_hub(&hub);
        drop(_guard);
        // We brought our community into a shared space: list it there.
        if let Some(home) = home {
            if let Err(why) = self.list_shared_space(&home, &id) {
                crate::delivery::notice(format!("Couldn't list the shared space on your community: {why}"));
            }
        }
        bump();
        crate::delivery::kick();
        Ok(space.id())
    }

    pub fn decline_invite(&self, index: usize) {
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        if index < hub.invites.len() {
            hub.invites.remove(index);
            self.save_hub(&hub);
        }
        bump();
    }

    /// Forgets a space we've left or been removed from, and its history.
    pub fn forget(&self, id: &Key) {
        let _guard = LOCK.lock().unwrap();
        if let Some(space) = self.load(id) {
            for c in &space.manifest().channels {
                let _ = crate::history::forget(&self.dir, &channel_log(id, c.id));
            }
        }
        let _ = std::fs::remove_file(space_path(&self.dir, id));
        bump();
    }

    /// Whether a reported excerpt is what our own copy of the channel says
    /// (`Some(true)`), says something else or isn't by that author
    /// (`Some(false)`), or we don't have it (`None`: a channel we can't
    /// read, or from before we joined).
    pub fn matches_history(&self, id: &Key, channel: u32, excerpt: &Excerpt) -> Option<bool> {
        let log = crate::history::load(&self.dir, &channel_log(id, channel), &self.key);
        // A message taken down since can't be compared.
        let mine = log.iter().find(|m| m.id == excerpt.id && !m.deleted)?;
        Some(mine.sender.as_deref() == Some(hex(&excerpt.author.0).as_str()) && mine.text == excerpt.text)
    }

    /// Which channel of space `id` holds message `message`, from the saved
    /// history -- so an edit or removal can go only where the message went.
    pub fn channel_of(&self, id: &Key, message: u64) -> Option<u32> {
        let space = self.load(id)?;
        space
            .manifest()
            .channels
            .iter()
            .map(|c| c.id)
            .find(|&c| crate::history::load(&self.dir, &channel_log(id, c), &self.key).iter().any(|m| m.id == message))
    }

    pub fn mark_read(&self, id: &Key) {
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        if hub.unread.remove(&hex(&id.0)).is_some() {
            self.save_hub(&hub);
        }
    }

    /// Your ring rule for this one, or `None` for your default.
    pub fn ring_rule(&self, id: &Key) -> Option<crate::notify::RingRule> {
        self.hub().ring.get(&hex(&id.0)).map(|v| crate::notify::RingRule::from_i32(*v))
    }

    pub fn set_ring_rule(&self, id: &Key, rule: Option<crate::notify::RingRule>) {
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        match rule {
            Some(r) => hub.ring.insert(hex(&id.0), r.to_i32()),
            None => hub.ring.remove(&hex(&id.0)),
        };
        self.save_hub(&hub);
        bump();
    }

    /// The rules you set on the spaces this identity is a member of, for
    /// `notify::shared_rule`.
    pub fn rules_for(&self, key: &[u8; 32]) -> Vec<crate::notify::RingRule> {
        let ring = self.hub().ring;
        self.ids()
            .into_iter()
            .filter_map(|id| Some((ring.get(&hex(&id.0)).copied()?, id)))
            .filter(|(_, id)| self.load(id).is_some_and(|s| !s.removed && s.manifest().is_member(key)))
            .map(|(r, _)| crate::notify::RingRule::from_i32(r))
            .collect()
    }

    /// Your notification setting for a space (`channel` `None`) or one of
    /// its channels, as set; `None` means the default.
    pub fn notify_setting(&self, id: &Key, channel: Option<u32>) -> Option<Notify> {
        self.hub().notify.get(&notify_key(id, channel)).copied()
    }

    /// Changes it; `None` goes back to the default.
    pub fn set_notify(&self, id: &Key, channel: Option<u32>, level: Option<Notify>) {
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        match level {
            Some(l) => hub.notify.insert(notify_key(id, channel), l),
            None => hub.notify.remove(&notify_key(id, channel)),
        };
        self.save_hub(&hub);
        bump();
    }

    pub fn unread(&self, id: &Key) -> u32 {
        self.hub().unread.get(&hex(&id.0)).copied().unwrap_or(0)
    }

    /// One fragment that arrived from `from`. Returns whether it was
    /// taken (so its mailbox copy may be deleted).
    pub fn receive_fragment(&self, from: [u8; 32], fragment: &str) -> bool {
        self.take_fragment(from, fragment, true)
    }

    /// One fragment collected at the inbox of a separate identity. Only
    /// community traffic is taken there: an invite to it is dropped, since
    /// whoever sent one could only know that key from inside a community,
    /// and "invite this key elsewhere" is how two communities would come
    /// to share it.
    pub fn receive_persona_fragment(&self, from: [u8; 32], fragment: &str) -> bool {
        self.take_fragment(from, fragment, false)
    }

    fn take_fragment(&self, from: [u8; 32], fragment: &str, invites: bool) -> bool {
        let wire = {
            let _guard = LOCK.lock().unwrap();
            let mut hub = self.hub();
            hub.reassembly.expire(now_secs());
            let wire = hub.reassembly.feed(from, fragment);
            self.save_hub(&hub);
            wire
        };
        match wire {
            Some(Wire::Invite { .. }) if !invites => {}
            Some(wire) => self.receive(from, wire),
            None => {}
        }
        true
    }

    /// The spaces we are in under a separate identity, and that identity:
    /// the courier collects each one's mail too.
    pub fn personas(&self) -> Vec<IdentityKeyPair> {
        self.ids()
            .iter()
            .filter_map(|id| self.load(id))
            .filter(|s| s.persona && !s.removed)
            .map(|s| self.identity_in(&s))
            .collect()
    }

    /// A whole message from `from`.
    pub fn receive(&self, from: [u8; 32], wire: Wire) {
        match wire {
            Wire::Invite { manifest, card } => self.receive_invite(from, manifest, card),
            Wire::Knock { community, card, proof, ticket } => {
                if self.load(&community).is_none() || card.key.0 != from || !card.valid() {
                    return;
                }
                // Into a shared space: checked against our own copy of the
                // community they say they come from.
                if let Some(proof) = proof {
                    let vouched = proof.check(&community, &from).is_some_and(|key| {
                        self.load(&proof.community).is_some_and(|home| !home.removed && home.joining.is_none() && home.manifest().is_member(&key))
                    });
                    {
                        let _guard = LOCK.lock().unwrap();
                        let mut hub = self.hub();
                        remember_card(&mut hub, card.clone());
                        self.save_hub(&hub);
                    }
                    let _ = self.with_space(&community, |space, identity| {
                        let outs = space.ally_knock(identity, from, &card, &proof, ticket, vouched)?;
                        Ok(((), outs, vec![Event::Notice(format!("{} asked in from an allied community and was invited", card.name.chars().take(80).collect::<String>()))]))
                    });
                    return;
                }
                // Kept so the invitation that answers them can be mailed.
                {
                    let _guard = LOCK.lock().unwrap();
                    let mut hub = self.hub();
                    remember_card(&mut hub, card.clone());
                    self.save_hub(&hub);
                }
                let _ = self.with_space(&community, |space, identity| {
                    let h = space.handle(identity, from, Wire::Knock { community, card, proof: None, ticket: None });
                    Ok(((), h.outs, h.events))
                });
            }
            Wire::BoardWelcome { community, board, epoch, key, token, since } => {
                // Only from whoever we are waiting to be let in by.
                let Some(space) = self.load(&community) else { return };
                let (Some(key), Some(token)) = (crate::connection_mode::decode_hex32(&key), crate::connection_mode::decode_hex32(&token)) else { return };
                if space.joining != Some(from) {
                    return;
                }
                let _guard = LOCK.lock().unwrap();
                let mut hub = self.hub();
                hub.board_welcomes.retain(|p| p.space != community);
                let key = EpochKey { epoch, key: Key(key), token: Key(token) };
                hub.board_welcomes.push(BoardPointer { space: community, from: Key(from), board, key, since, at_secs: now_secs() });
                self.save_hub(&hub);
                drop(_guard);
                crate::delivery::kick();
            }
            other => {
                let Some(id) = other.community() else { return };
                if self.load(&id).is_none() {
                    return; // not something we're in
                }
                let _ = self.with_space(&id, |space, identity| {
                    let h = space.handle(identity, from, other);
                    Ok(((), h.outs, h.events))
                });
            }
        }
    }

    /// Keeps an invite for the user to answer -- only from a saved
    /// contact or someone already in a community with us, so a stranger
    /// can't fill the list.
    fn receive_invite(&self, from: [u8; 32], manifest: String, card: Card) {
        let Some(signed) = unb64(&manifest).ok().and_then(|b| SignedManifest::decode(&b).ok()) else { return };
        let Ok(m) = cm::accept_first(&signed) else { return };
        // Or the answer to our asking to join from its listing.
        let now = now_secs();
        let asked = self.hub().knocked.iter().any(|(c, at)| *c == m.id && now.saturating_sub(*at) <= KNOCK_LIFETIME_SECS);
        let known = asked
            || crate::config::load().contacts.iter().any(|c| c.identity_key.eq_ignore_ascii_case(&hex(&from)))
            || self.ids().iter().filter_map(|id| self.load(id)).any(|s| s.manifest().is_member(&from));
        if !known || card.key.0 != from || !card.valid() {
            return;
        }
        // Listed already (from an older build), or offered by someone
        // allowed to invite: we are written in when we answer.
        if !m.is_member(&self.identity.public().identity_key) && !m.can(&from, perm::INVITE, None) {
            return;
        }
        // Already in it: this is the same invite again, or a re-invite
        // after we lost our place -- either way answer it ourselves.
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        hub.invites.retain(|i| !(unb64(&i.manifest).ok().and_then(|b| SignedManifest::decode(&b).ok()).and_then(|s| s.open().ok()).is_some_and(|x| x.id == m.id)));
        if hub.invites.len() < MAX_INVITES {
            hub.invites.push(Invite { from: Key(from), manifest, card, name: m.name.clone(), kind: m.kind, at_secs: now_secs() });
            crate::delivery::notice(format!("Invitation to {}.", m.name));
        }
        self.save_hub(&hub);
        drop(_guard);
        bump();
    }

    /// The join link for a listed community we may invite people to: a
    /// directory lists it, and a stranger who uses it is invited by us.
    pub fn join_link(&self, id: &Key) -> Result<String, String> {
        let space = self.load(id).ok_or("couldn't open it")?;
        let m = space.manifest();
        if !m.listed {
            return Err("It isn't open to people asking to join; turn that on first".into());
        }
        if !m.can(&space.me(), perm::INVITE, None) {
            return Err("You can't invite people there, so you can't answer them".into());
        }
        let identity = self.identity_in(&space);
        let host = self.card_for(id, &identity);
        Ok(JoinLink { community: *id, host }.to_link())
    }

    /// Asks to join the community a join link names. Its host's app
    /// invites us if it is still open, and the invitation shows up with
    /// the others, to accept or not.
    pub fn knock(&self, link: &str) -> Result<(), String> {
        let join = JoinLink::parse(link).ok_or("That isn't a join link")?;
        self.knock_with(join, None, None)
    }

    /// Sends a Knock to a join link's host, with a proof of where we come
    /// from and a ticket when asking into a shared space.
    fn knock_with(&self, join: JoinLink, from: Option<Key>, ticket: Option<Key>) -> Result<(), String> {
        if self.load(&join.community).is_some_and(|s| !s.removed) {
            return Err("You're already in it".into());
        }
        let me = self.identity.public().identity_key;
        let proof = match from {
            Some(home) => {
                let space = self.load(&home).ok_or("couldn't open your community")?;
                Some(Proof::make(&self.identity_in(&space), home, &join.community, &me))
            }
            None => None,
        };
        let card = self.card_for(&join.community, &self.identity);
        let to = join.host.key.0;
        {
            let _guard = LOCK.lock().unwrap();
            let mut hub = self.hub();
            remember_card(&mut hub, join.host);
            hub.knocked.retain(|(c, _)| *c != join.community);
            hub.knocked.push((join.community, now_secs()));
            if hub.knocked.len() > MAX_INTRODUCED {
                hub.knocked.remove(0);
            }
            if let (Some(home), Some(_)) = (from, ticket) {
                hub.allying.retain(|(s, _)| *s != join.community);
                hub.allying.push((join.community, home));
                if hub.allying.len() > MAX_INTRODUCED {
                    hub.allying.remove(0);
                }
            }
            self.queue(&mut hub, vec![Out { to, wire: Wire::Knock { community: join.community, card, proof, ticket } }], me, None);
            self.save_hub(&hub);
        }
        bump();
        crate::delivery::kick();
        Ok(())
    }

    // ---- shared spaces (spec §12 Q4, federation) ----

    /// Starts a space shared with another community, for `home` (which we
    /// must be allowed to manage): a new space that `home`'s members may
    /// ask into, listed on `home`. Returns the link to give the other
    /// community's admin; it works once, within a week.
    pub fn start_shared_space(&self, home: &Key, name: &str) -> Result<String, String> {
        let space = self.load(home).ok_or("couldn't open it")?;
        if space.manifest().kind != Kind::Community || !space.manifest().alliance.is_empty() {
            return Err("Only a community can share a space".into());
        }
        if !space.manifest().can(&space.me(), perm::MANAGE_COMMUNITY, None) {
            return Err("You can't manage this community".into());
        }
        if space.manifest().allies.len() >= cm::MAX_ALLIES {
            return Err("It shares as many spaces as it can".into());
        }
        let shared = self.create(Kind::Community, name, space.persona)?;
        let home_id = *home;
        let ticket = self.with_space(&shared, |s, identity| {
            let outs = s.update(identity, |m| m.alliance.push(home_id))?;
            Ok((s.new_ticket()?, outs, Vec::new()))
        })?;
        self.list_shared_space(home, &shared)?;
        let link = self.shared_space_link(&shared)?;
        Ok(format!("{ALLY_PREFIX}{}.{}", hex(&ticket.0), link.trim_start_matches(JOIN_PREFIX)))
    }

    /// Another link for the other community's admin, for a shared space
    /// we own.
    pub fn new_ally_link(&self, shared: &Key) -> Result<String, String> {
        let ticket = self.with_space(shared, |s, _| Ok((s.new_ticket()?, Vec::new(), Vec::new())))?;
        let link = self.shared_space_link(shared)?;
        Ok(format!("{ALLY_PREFIX}{}.{}", hex(&ticket.0), link.trim_start_matches(JOIN_PREFIX)))
    }

    /// Brings `home` into the shared space a link from another
    /// community's admin names: asks in with its ticket, and once we
    /// accept the invitation that answers, lists it on `home`.
    pub fn join_shared_space(&self, home: &Key, link: &str) -> Result<(), String> {
        let (ticket, join) = parse_ally_link(link).ok_or("That isn't a link for a shared space")?;
        let space = self.load(home).ok_or("couldn't open it")?;
        if !space.manifest().can(&space.me(), perm::MANAGE_COMMUNITY, None) {
            return Err("You can't manage this community".into());
        }
        if space.manifest().allies.iter().any(|a| a.space == join.community) {
            return Err("It already shares that space".into());
        }
        self.knock_with(join, Some(*home), Some(ticket))
    }

    /// Asks into a shared space `home` lists, as one of its members.
    pub fn ask_into_shared_space(&self, home: &Key, shared: &Key) -> Result<(), String> {
        let space = self.load(home).ok_or("couldn't open it")?;
        let ally = space.manifest().allies.iter().find(|a| a.space == *shared).ok_or("It doesn't share that space any more")?;
        let join = JoinLink::parse(&ally.link).ok_or("Its link is broken; ask an admin to list it again")?;
        self.knock_with(join, Some(*home), None)
    }

    /// Takes a shared space off `home`'s list. Members already in it stay.
    pub fn unlist_shared_space(&self, home: &Key, shared: &Key) -> Result<(), String> {
        let shared = *shared;
        self.with_space(home, |s, identity| Ok(((), s.update(identity, |m| m.allies.retain(|a| a.space != shared))?, Vec::new())))
    }

    /// Lists `shared` (which we are in) on `home`, with a join link we
    /// host, so `home`'s members ask us and we can tell they are members.
    fn list_shared_space(&self, home: &Key, shared: &Key) -> Result<(), String> {
        let space = self.load(shared).ok_or("couldn't open it")?;
        let name = space.manifest().name.clone();
        let link = self.shared_space_link(shared)?;
        let ally = cm::Ally { space: *shared, name, link };
        self.with_space(home, |s, identity| {
            let outs = s.update(identity, |m| {
                m.allies.retain(|a| a.space != ally.space);
                m.allies.push(ally);
            })?;
            Ok(((), outs, Vec::new()))
        })
    }

    /// A shared space's join link, hosted by us.
    fn shared_space_link(&self, shared: &Key) -> Result<String, String> {
        let space = self.load(shared).ok_or("couldn't open it")?;
        let identity = self.identity_in(&space);
        Ok(JoinLink { community: *shared, host: self.card_for(shared, &identity) }.to_link())
    }

    /// How to mail a member: a contact's prekey and routes, or their card.
    fn reach(&self, to: &[u8; 32]) -> Option<([u8; 32], SignedRouteRecord)> {
        let config = crate::config::load();
        let wanted = hex(to);
        if let Some(c) = config.contacts.iter().find(|c| c.identity_key.eq_ignore_ascii_case(&wanted)) {
            if let (Some(pre), Some(rec)) = (crate::connection_mode::decode_hex32(&c.prekey), crate::delivery::accept_record(&c.route_record, to, None)) {
                return Some((pre, rec));
            }
        }
        for id in self.ids() {
            if let Some(card) = self.load(&id).and_then(|s| s.card(to).cloned()) {
                if let (Some(pre), Some(rec)) = (card.prekey(), card.record()) {
                    return Some((pre, rec));
                }
            }
        }
        let card = self.hub().introduced.into_iter().rev().find(|c| c.key.0 == *to)?;
        Some((card.prekey()?, card.record()?))
    }

    /// Sends what is queued: over a live call to that person if there is
    /// one, otherwise to their mailboxes. What can't go yet stays queued.
    pub async fn flush(&self, fan_out: bool, use_mailboxes: bool) -> usize {
        let queued = {
            let _guard = LOCK.lock().unwrap();
            self.hub().outbox
        };
        if queued.is_empty() {
            return 0;
        }
        let mut sent: Vec<u64> = Vec::new();
        let mut failed: Vec<u64> = Vec::new();
        let mut unreachable: Vec<Key> = Vec::new();
        let mut personas: HashMap<Key, IdentityKeyPair> = HashMap::new();
        for q in &queued {
            if unreachable.contains(&q.to) {
                continue;
            }
            // A live call is with our main key: what we send as a separate
            // identity never goes over one, or the call would say who sent it.
            if let Some(live) = live(&q.to.0).filter(|_| q.persona.is_none()) {
                let msg = crate::control::ControlMessage::Community { data: q.fragment.clone() };
                if live.send_control(&msg.encode()).await.is_ok() {
                    sent.push(q.id);
                    continue;
                }
            }
            let reach = if use_mailboxes { self.reach(&q.to.0) } else { None };
            let Some((prekey, record)) = reach else {
                unreachable.push(q.to);
                continue;
            };
            let body = crate::delivery::MailBody { id: q.id, text: String::new(), at_secs: now_secs(), group: Some(q.fragment.clone()), reply_to: None };
            let sender: &IdentityKeyPair = match q.persona {
                Some(id) => personas.entry(id).or_insert_with(|| identity_for(&self.identity, &id, true)),
                None => &self.identity,
            };
            match crate::delivery::send(sender, &q.to.0, &prekey, &record, &body, fan_out).await {
                crate::delivery::Sent::Deposited(_) => sent.push(q.id),
                crate::delivery::Sent::NoRoute => unreachable.push(q.to),
                crate::delivery::Sent::Failed(_) => {
                    failed.push(q.id);
                    // Their nodes are refusing (most likely the per-inbox
                    // rate limit): the rest for them waits for next round.
                    unreachable.push(q.to);
                }
            }
        }
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        hub.outbox.retain(|q| !sent.contains(&q.id));
        for q in hub.outbox.iter_mut().filter(|q| failed.contains(&q.id)) {
            q.attempts += 1;
        }
        self.save_hub(&hub);
        sent.len()
    }

    pub fn queued(&self) -> usize {
        self.hub().outbox.len()
    }

    /// Fragments that arrived together (a page of a board), taken in one
    /// pass: one save of what is waiting for the rest, then each whole
    /// message handled.
    fn take_fragments(&self, fragments: Vec<([u8; 32], String)>) {
        if fragments.is_empty() {
            return;
        }
        let wires: Vec<([u8; 32], Wire)> = {
            let _guard = LOCK.lock().unwrap();
            let mut hub = self.hub();
            hub.reassembly.expire(now_secs());
            let wires = fragments.into_iter().filter_map(|(from, f)| Some((from, hub.reassembly.feed(from, &f)?))).collect();
            self.save_hub(&hub);
            wires
        };
        for (from, wire) in wires {
            // Invitations come by mail, never by a board.
            if !matches!(wire, Wire::Invite { .. }) {
                self.receive(from, wire);
            }
        }
    }

    /// Gives a community a board on the mailbox node `addr`, or (with
    /// `None`) takes it away. A setting like any other: it needs Manage
    /// community, and travels as a new manifest version.
    pub fn set_board(&self, id: &Key, addr: Option<String>) -> Result<(), String> {
        let board = match addr.map(|a| a.trim().to_string()) {
            Some(a) if a.is_empty() => return Err("Give the node's address.".into()),
            Some(addr) => {
                let mut board = [0u8; 32];
                getrandom::fill(&mut board).expect("OS randomness is unavailable");
                Some(cm::BoardRef { addr, id: Key(board) })
            }
            None => None,
        };
        self.with_space(id, |s, identity| {
            if board.is_none() && s.manifest().members.len() > cm::MAX_COMMUNITY_MEMBERS {
                return Err(format!("It has more than {} members, which it can only have with a board.", cm::MAX_COMMUNITY_MEMBERS));
            }
            let outs = s.update(identity, |m| m.nodes.board = board)?;
            Ok(((), outs, vec![Event::Changed]))
        })?;
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        hub.boards.remove(&hex(&id.0));
        self.save_hub(&hub);
        Ok(())
    }

    /// Spec §11 "Communities at full scale": for each community with a
    /// board, makes sure the node takes our current epoch's token, posts
    /// what is waiting, and reads what is new, a page at a time. Also
    /// fetches a Welcome waiting on a board for a community we are joining.
    /// Returns how many entries were read.
    pub async fn sync_boards(&self) -> usize {
        let mut read = 0;
        let pointers = {
            let _guard = LOCK.lock().unwrap();
            let mut hub = self.hub();
            let now = now_secs();
            let before = hub.board_welcomes.len();
            hub.board_welcomes.retain(|p| now.saturating_sub(p.at_secs) < BOARD_WELCOME_LIFETIME_SECS);
            if hub.board_welcomes.len() != before {
                self.save_hub(&hub);
            }
            hub.board_welcomes
        };
        for p in pointers {
            read += self.fetch_welcome(p).await;
        }
        for id in self.ids() {
            let Some(space) = self.load(&id) else { continue };
            if space.removed || space.joining.is_some() || space.board().is_none() {
                continue;
            }
            read += self.sync_board(space).await;
        }
        read
    }

    /// The node's address, and a socket to reach it the way messages go
    /// (through the VPN when they are routed there, spec §6).
    async fn board_node(board: &cm::BoardRef) -> Option<(antiphon_net::AppSocket, std::net::SocketAddr)> {
        use antiphon_net::vpn::TrafficKind;
        let addr = crate::delivery::resolve(TrafficKind::Messages, &board.addr).await?;
        let socket = crate::vpn::bind(TrafficKind::Messages, 0).await.ok()?;
        Some((socket, addr))
    }

    async fn sync_board(&self, space: Space) -> usize {
        use antiphon_net::board as nb;
        use antiphon_net::mailbox::Refusal;
        use antiphon_net::NetError;
        let id = space.id();
        let board = space.board().expect("checked by the caller").clone();
        let Some(current) = space.board_keys().last().cloned() else { return 0 };
        let Some((socket, node)) = Self::board_node(&board).await else { return 0 };
        let timeout = crate::delivery::NODE_TIMEOUT;
        let mut state = self.hub().boards.get(&hex(&id.0)).cloned().filter(|s| s.board == Some(board.id)).unwrap_or_default();
        state.board = Some(board.id);

        // 1. The node takes our current token: we made the board, or it
        //    already knows this epoch, or we tell it, proving an older one.
        if state.allowed != Some(current.epoch) {
            let allowed = match nb::open(&socket, node, board.id.0, &current.token.0, timeout).await {
                Ok(()) => true,
                Err(NetError::MailboxRefused(Refusal::WrongSecret)) => {
                    let mut ok = false;
                    for older in space.board_keys().iter().rev().skip(1) {
                        if nb::rekey(&socket, node, board.id.0, &older.token.0, &current.token.0, timeout).await.is_ok() {
                            ok = true;
                            break;
                        }
                    }
                    ok
                }
                Err(_) => return 0,
            };
            if !allowed {
                return 0;
            }
            state.allowed = Some(current.epoch);
        }

        // 2. What is waiting to go up, in order.
        let waiting: Vec<BoardQueued> = self.hub().board_out.into_iter().filter(|q| q.space == id).collect();
        let mut posted = 0;
        for q in &waiting {
            let Ok(entry) = unb64(&q.entry) else {
                posted += 1;
                continue;
            };
            match nb::post(&socket, node, board.id.0, &current.token.0, &entry, timeout).await {
                Ok(seq) => {
                    posted += 1;
                    if let Some(w) = &q.welcome {
                        let wire = Wire::BoardWelcome {
                            community: id,
                            board: board.clone(),
                            epoch: w.key.epoch,
                            key: hex(&w.key.key.0),
                            token: hex(&w.key.token.0),
                            since: seq - 1,
                        };
                        let _guard = LOCK.lock().unwrap();
                        let mut hub = self.hub();
                        self.queue(&mut hub, vec![Out { to: w.to.0, wire }], space.me(), space.persona.then_some(id));
                        self.save_hub(&hub);
                    }
                }
                Err(_) => break,
            }
        }
        if posted > 0 {
            let _guard = LOCK.lock().unwrap();
            let mut hub = self.hub();
            let mut left = posted;
            hub.board_out.retain(|q| {
                if q.space == id && left > 0 {
                    left -= 1;
                    false
                } else {
                    true
                }
            });
            self.save_hub(&hub);
        }

        // 3. What is new. A first look starts a page back from the newest,
        //    not at the beginning: older entries are from epochs before
        //    we were in, which we couldn't read anyway.
        let mut since = match state.seen {
            Some(s) => s,
            None => match nb::fetch(&socket, node, board.id.0, &current.token.0, 0, 1, timeout).await {
                Ok(page) => page.last.saturating_sub(nb::MAX_PAGE as u64),
                Err(_) => return 0,
            },
        };
        let mut read = 0;
        'pages: for _ in 0..BOARD_PAGES_PER_LOOK {
            let Ok(page) = nb::fetch(&socket, node, board.id.0, &current.token.0, since, nb::MAX_PAGE, timeout).await else { break };
            if page.entries.is_empty() {
                since = since.max(page.next.min(page.last));
                break;
            }
            let mut batch = Vec::new();
            for (seq, entry) in page.entries {
                // Each page is opened with the keys as they stand: a commit
                // earlier on it may have moved us on.
                let keys = self.load(&id).map(|s| s.board_keys().to_vec()).unwrap_or_default();
                match community_board::open(&board.id, &keys, &entry) {
                    community_board::Opened::Fragment { from, fragment } => {
                        if from != space.me() {
                            batch.push((from, fragment));
                        }
                    }
                    community_board::Opened::Ahead(_) if !batch.is_empty() => {
                        // Handle what came before it first: the commit that
                        // gets us there may be in this batch.
                        self.take_fragments(std::mem::take(&mut batch));
                        let keys = self.load(&id).map(|s| s.board_keys().to_vec()).unwrap_or_default();
                        if let community_board::Opened::Fragment { from, fragment } = community_board::open(&board.id, &keys, &entry) {
                            if from != space.me() {
                                batch.push((from, fragment));
                            }
                        } else {
                            state.stuck += 1;
                            if state.stuck < BOARD_STUCK_LOOKS {
                                break 'pages;
                            }
                        }
                    }
                    community_board::Opened::Ahead(_) => {
                        state.stuck += 1;
                        if state.stuck < BOARD_STUCK_LOOKS {
                            break 'pages;
                        }
                    }
                    community_board::Opened::Unreadable => {}
                }
                since = seq;
                state.stuck = 0;
                read += 1;
                if batch.len() >= 16 {
                    self.take_fragments(std::mem::take(&mut batch));
                }
            }
            self.take_fragments(batch);
            if since >= page.last {
                break;
            }
        }
        state.seen = Some(since);
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        hub.boards.insert(hex(&id.0), state);
        self.save_hub(&hub);
        read
    }

    /// Fetches a Welcome left on a board for us, and what follows it in
    /// the same epoch, until we are in. From then on the board is read
    /// as usual, from where this left off.
    async fn fetch_welcome(&self, p: BoardPointer) -> usize {
        use antiphon_net::board as nb;
        let Some((socket, node)) = Self::board_node(&p.board).await else { return 0 };
        let timeout = crate::delivery::NODE_TIMEOUT;
        let mut since = p.since;
        let mut read = 0;
        let mut joined = false;
        for _ in 0..BOARD_PAGES_PER_LOOK {
            let Ok(page) = nb::fetch(&socket, node, p.board.id.0, &p.key.token.0, since, nb::MAX_PAGE, timeout).await else { break };
            let mut batch = Vec::new();
            for (seq, entry) in &page.entries {
                if let community_board::Opened::Fragment { from, fragment } = community_board::open(&p.board.id, std::slice::from_ref(&p.key), entry) {
                    batch.push((from, fragment));
                }
                since = *seq;
                read += 1;
            }
            self.take_fragments(batch);
            joined = self.load(&p.space).is_some_and(|s| s.joining.is_none());
            if joined || page.entries.is_empty() || since >= page.last {
                break;
            }
        }
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        if joined {
            hub.board_welcomes.retain(|q| q.space != p.space);
            hub.boards.insert(hex(&p.space.0), BoardState { board: Some(p.board.id), seen: Some(since), allowed: None, stuck: 0 });
        } else if let Some(q) = hub.board_welcomes.iter_mut().find(|q| q.space == p.space) {
            q.since = since;
        }
        self.save_hub(&hub);
        read
    }
}

/// Our card for one space: `identity`'s key, prekey and route list, and
/// the name, status and picture chosen for that space -- or the profile's
/// name and status where none was. No picture unless one was chosen.
pub fn card_as(identity: &IdentityKeyPair, routes_hex: String, profile: &crate::config::ProfileData, look: &Appearance) -> Card {
    let pick = |own: &str, default: &str| if own.trim().is_empty() { default.to_string() } else { own.trim().to_string() };
    Card::mine(identity, routes_hex, &pick(&look.name, &profile.display_name)).with_profile(&pick(&look.status, &profile.status_message), &look.avatar)
}

/// Our route list or profile changed: tell each space's members where to
/// reach us and what we're called there.
pub fn refresh_cards() {
    let Some(store) = Store::app() else { return };
    for id in store.ids() {
        let _ = store.with_space(&id, |space, identity| {
            if space.removed || space.joining.is_some() {
                return Ok(((), Vec::new(), Vec::new()));
            }
            let card = store.card_for(&space.id(), identity);
            // The record is re-signed with a new timestamp each time, so
            // compare what it says, not its bytes.
            let seq = |c: &Card| c.record().map(|r| (r.record.sequence, r.record.routes));
            let same = space.card(&space.me()).is_some_and(|old| {
                old.name == card.name && old.status == card.status && old.avatar == card.avatar && seq(old) == seq(&card)
            });
            if same {
                return Ok(((), Vec::new(), Vec::new()));
            }
            Ok(((), space.refresh_card(card)?, Vec::new()))
        });
    }
}

// ---- live calls ------------------------------------------------------------

static LIVE: Mutex<Vec<([u8; 32], Arc<antiphon_net::PeerConnection>)>> = Mutex::new(Vec::new());

/// A call is up with this person: community messages for them go over it.
pub fn set_live(peer: Arc<antiphon_net::PeerConnection>) {
    let key = peer.peer_identity().identity_key;
    let mut live = LIVE.lock().unwrap();
    live.retain(|(k, _)| *k != key);
    live.push((key, peer));
    crate::delivery::kick();
}

pub fn clear_live(key: &[u8; 32]) {
    LIVE.lock().unwrap().retain(|(k, _)| k != key);
}

fn live(key: &[u8; 32]) -> Option<Arc<antiphon_net::PeerConnection>> {
    LIVE.lock().unwrap().iter().find(|(k, _)| k == key).map(|(_, p)| p.clone())
}

/// The separate identity to fetch a shared file as, when its link came
/// from a community (`with` is `community:<id hex>`) we are in under one.
pub fn identity_for_link(with: &str) -> Option<Arc<IdentityKeyPair>> {
    let id = crate::connection_mode::decode_hex32(with.strip_prefix("community:")?).map(Key)?;
    let store = Store::app()?;
    let space = store.load(&id).filter(|s| s.persona)?;
    Some(Arc::new(store.identity_in(&space)))
}

/// For the seeding holder (spec §5 "this community"): is this identity a
/// member of that community right now, by our copy of its manifest?
pub fn is_member(community: &[u8; 32], key: &[u8; 32]) -> bool {
    let Some(store) = Store::app() else { return false };
    store.load(&Key(*community)).is_some_and(|s| !s.removed && s.manifest().is_member(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mention_is_the_whole_name_after_an_at() {
        assert!(mentions("hey @Sam, you in?", "sam"));
        assert!(mentions("@sam", "Sam"));
        assert!(!mentions("hey @samantha", "Sam"), "a longer name isn't a mention");
        assert!(!mentions("hey sam", "Sam"), "no at, no mention");
        assert!(!mentions("@", ""));
        assert!(mentions("@Robin Okafor are you coming", "Robin Okafor"));
    }

    #[test]
    fn a_channel_setting_beats_its_space_and_the_old_mute_is_kept() {
        let id = Key([7; 32]);
        let mut hub = Hub::default();
        assert_eq!(notify_level(&hub, &id, 0), Notify::All);
        hub.ring.insert(hex(&id.0), crate::notify::RingRule::Never.to_i32());
        assert_eq!(notify_level(&hub, &id, 0), Notify::Nothing, "a space muted before still is");
        hub.notify.insert(notify_key(&id, None), Notify::Mentions);
        assert_eq!(notify_level(&hub, &id, 0), Notify::Mentions);
        hub.notify.insert(notify_key(&id, Some(2)), Notify::All);
        assert_eq!(notify_level(&hub, &id, 2), Notify::All);
        assert_eq!(notify_level(&hub, &id, 0), Notify::Mentions, "other channels follow the space");
    }

    struct Person {
        id: IdentityKeyPair,
        space: Option<Space>,
        name: &'static str,
        /// Answers invites under a separate identity.
        separate: bool,
        /// How they appear in the next space they join or make.
        look: Appearance,
    }

    fn person(name: &'static str) -> Person {
        Person { id: IdentityKeyPair::generate(), space: None, name, separate: false, look: Appearance::default() }
    }

    impl Person {
        fn key(&self) -> [u8; 32] {
            self.id.public().identity_key
        }
        /// The key they're in their space as.
        fn here(&self) -> [u8; 32] {
            self.space.as_ref().map_or(self.key(), super::Space::me)
        }
        /// Who they act as in their space.
        fn acting(&self) -> IdentityKeyPair {
            match &self.space {
                Some(s) => identity_for(&self.id, &s.id(), s.persona),
                None => IdentityKeyPair::from_master_seed(self.id.master_seed()),
            }
        }
        fn card(&self) -> Card {
            Card::mine(&self.id, String::new(), self.name)
        }
        fn card_as(&self, identity: &IdentityKeyPair) -> Card {
            let profile = crate::config::ProfileData { display_name: self.name.into(), status_message: "Online".into(), ..Default::default() };
            card_as(identity, String::new(), &profile, &self.look)
        }
        fn space(&mut self) -> &mut Space {
            self.space.as_mut().unwrap()
        }
        fn card_with_routes(&self, own: &[Card]) -> Card {
            own.iter().find(|c| c.key.0 == self.key()).cloned().unwrap()
        }
    }

    /// Delivers everything, in memory, until nothing more is sent. What
    /// `divert` picks is not delivered but handed back.
    fn run_holding(people: &mut [&mut Person], mut outs: Vec<(usize, Out)>, divert: &dyn Fn(&Out) -> bool) -> (Vec<Vec<Event>>, Vec<(usize, Out)>) {
        let mut events = vec![Vec::new(); people.len()];
        let mut diverted = Vec::new();
        let mut rounds = 0;
        while !outs.is_empty() {
            rounds += 1;
            assert!(rounds < 50, "messages kept bouncing");
            let mut next = Vec::new();
            for (from, out) in outs {
                if divert(&out) {
                    diverted.push((from, out));
                    continue;
                }
                let Some(to) = people.iter().position(|p| p.key() == out.to || p.here() == out.to) else { continue };
                // A Join is sealed with the joiner's main key, whoever
                // they join as; the rest with the key they're in it as.
                let from_key = if matches!(out.wire, Wire::Join { .. }) { people[from].key() } else { people[from].here() };
                // Through the wire format, fragments and all.
                let mut r = Reassembly::default();
                let mut wire = None;
                for f in fragments(&out.wire) {
                    wire = r.feed(from_key, &f).or(wire);
                }
                let wire = wire.expect("reassembled");
                if let Wire::Invite { manifest, card } = wire {
                    let invite = Invite { from: Key(from_key), manifest, card, name: String::new(), kind: Kind::Group, at_secs: 0 };
                    let p = &mut people[to];
                    let community = SignedManifest::decode(&unb64(&invite.manifest).unwrap()).unwrap().open().unwrap().id;
                    let as_whom = identity_for(&p.id, &community, p.separate);
                    let (space, join) = Space::accept(&as_whom, &invite, p.card_as(&as_whom), p.separate).unwrap();
                    p.space = Some(space);
                    next.push((to, join));
                    continue;
                }
                let p = &mut people[to];
                let id = p.acting();
                let Some(space) = p.space.as_mut() else { continue };
                let h = space.handle(&id, from_key, wire);
                events[to].extend(h.events);
                next.extend(h.outs.into_iter().map(|o| (to, o)));
            }
            outs = next;
        }
        (events, diverted)
    }

    fn run(people: &mut [&mut Person], outs: Vec<(usize, Out)>) -> Vec<Vec<Event>> {
        run_holding(people, outs, &|_| false).0
    }

    fn texts(events: &[Event]) -> Vec<String> {
        events.iter().filter_map(|e| if let Event::Message { text, .. } = e { Some(text.clone()) } else { None }).collect()
    }

    /// Alice makes a group, invites Bob and Carol, everyone joins.
    fn three() -> (Person, Person, Person) {
        let (mut alice, mut bob, mut carol) = (person("Alice"), person("Bob"), person("Carol"));
        alice.space = Some(Space::create(&alice.id, Key([3; 32]), Kind::Community, "Choir", alice.card(), false).unwrap());
        let id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let outs = alice.space().invite(&id, bob.key(), "Bob").unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        let outs = alice.space().invite(&id, carol.key(), "Carol").unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        assert!(ev[2].contains(&Event::Joined));
        for p in [&alice, &bob, &carol] {
            let mut r = p.space.as_ref().unwrap().readers();
            r.sort_unstable();
            let mut want = vec![alice.key(), bob.key(), carol.key()];
            want.sort_unstable();
            assert_eq!(r, want, "{} sees everyone", p.name);
        }
        (alice, bob, carol)
    }

    #[test]
    fn three_people_join_and_talk_with_threads() {
        let (mut alice, mut bob, mut carol) = three();
        // Carol learned how to reach Bob without being his contact.
        assert!(carol.space().card(&bob.key()).is_some());
        let (outs, mine) = bob.space().post(0, "first!", None).unwrap();
        let Event::Message { id: root, .. } = mine else { panic!() };
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (1, o)).collect());
        assert_eq!(texts(&ev[0]), ["first!"]);
        assert_eq!(texts(&ev[2]), ["first!"]);
        let (outs, _) = carol.space().post(0, "a reply", Some(root)).unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (2, o)).collect());
        assert!(ev[1].iter().any(|e| matches!(e, Event::Message { reply_to: Some(r), .. } if *r == root)));
    }

    #[test]
    fn reactions_take_any_emoji_and_this_communitys_own() {
        let (mut alice, mut bob, mut carol) = three();
        let alice_id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let outs = alice
            .space()
            .update(&alice_id, |m| m.emoji.push(antiphon_crypto::community::Emoji { name: "party".into(), image: "x".into() }))
            .unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        let (outs, mine) = bob.space().post(0, "react to me", None).unwrap();
        let Event::Message { id, sender: author, .. } = mine else { panic!() };
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (1, o)).collect());

        // A duck, and the community's own :party:, both arrive.
        for emoji in ["🦆", ":party:"] {
            let (outs, _) = carol.space().react(id, author, 0, emoji, true).unwrap();
            let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (2, o)).collect());
            assert!(ev[1].iter().any(|e| matches!(e, Event::Reacted { emoji: got, on: true, .. } if got == emoji)), "{emoji} didn't reach Bob");
        }
        // Text, or an emoji this community doesn't have, is refused.
        assert!(carol.space().react(id, author, 0, "hello", true).is_err());
        assert!(carol.space().react(id, author, 0, ":nope:", true).is_err());
    }

    #[test]
    fn a_stored_reaction_is_set_once_and_capped_per_member() {
        let mut list = Vec::new();
        set_reaction(&mut list, "🦆", "c", true);
        set_reaction(&mut list, "🦆", "c", true);
        assert_eq!(list.len(), 1, "delivered twice, stored once");
        set_reaction(&mut list, "🦆", "b", true);
        set_reaction(&mut list, "🦆", "c", false);
        assert_eq!(list, [crate::history::StoredReaction { emoji: "🦆".into(), by: "b".into() }]);
        for n in 0..MAX_REACTIONS_EACH + 5 {
            set_reaction(&mut list, &format!(":e{n}:"), "c", true);
        }
        assert_eq!(list.iter().filter(|r| r.by == "c").count(), MAX_REACTIONS_EACH);
    }

    #[test]
    fn a_private_channel_reaches_only_its_readers() {
        let (mut alice, mut bob, mut carol) = three();
        let alice_id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let bob_key = Key(bob.key());
        let outs = alice
            .space()
            .update(&alice_id, |m| {
                m.channels.push(antiphon_crypto::community::Channel { id: 1, name: "staff".into(), readers: Some(vec![bob_key]), ..Default::default() });
            })
            .unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        let carol_key = carol.key();
        assert!(!carol.space().manifest().can_read(1, &carol_key));

        // Bob's post goes to Alice (an owner) and not to Carol.
        let (outs, _) = bob.space().post(1, "staff only", None).unwrap();
        assert!(outs.iter().all(|o| o.to != carol.key()), "not even sent to her");
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (1, o)).collect());
        assert_eq!(texts(&ev[0]), ["staff only"]);
        assert!(texts(&ev[2]).is_empty());
        // Carol can't post there.
        assert!(carol.space().post(1, "let me in", None).is_err());
        // And what Bob says in the open channel next still reaches her.
        let (outs, _) = bob.space().post(0, "hi all", None).unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (1, o)).collect());
        assert_eq!(texts(&ev[2]), ["hi all"]);
    }

    #[test]
    fn slow_mode_holds_posts_back_on_both_sides() {
        let (mut alice, mut bob, mut carol) = three();
        let alice_id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let outs = alice.space().update(&alice_id, |m| m.slow_mode_secs = 60).unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        let (outs, _) = bob.space().post(0, "one", None).unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (1, o)).collect());
        let refused = bob.space().post(0, "two", None).unwrap_err();
        assert!(refused.contains("Slow mode"), "{refused}");
        // A client that ignores it is dropped by everyone else.
        let bob_key = bob.key();
        let now = now_secs();
        let post = bob.space().encrypt(&Post::Message { id: 9, channel: 0, text: "sneaky".into(), reply_to: None, at_secs: now }).unwrap();
        let carol_key = carol.key();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], vec![(1, Out { to: carol_key, wire: post })]);
        assert!(texts(&ev[2]).is_empty(), "too soon after the last");
        // Written a minute later, it gets through, even if it arrives late.
        let post = bob.space().encrypt(&Post::Message { id: 10, channel: 0, text: "later".into(), reply_to: None, at_secs: now + 61 }).unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], vec![(1, Out { to: carol_key, wire: post })]);
        assert_eq!(texts(&ev[2]), ["later"]);
        // The owner may remove messages, so isn't held to it.
        for _ in 0..3 {
            let (outs, _) = alice.space().post(0, "announcement", None).unwrap();
            run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        }
        assert!(carol.space().pace.iter().any(|p| p.key.0 == bob_key));
    }

    #[test]
    fn a_flood_is_capped_without_slow_mode() {
        let (mut alice, mut bob, mut carol) = three();
        let carol_key = carol.key();
        let now = now_secs();
        let mut heard = 0;
        for i in 0..(FLOOD_MAX as u64 + 5) {
            let post = bob.space().encrypt(&Post::Message { id: 100 + i, channel: 0, text: "spam".into(), reply_to: None, at_secs: now }).unwrap();
            let ev = run(&mut [&mut alice, &mut bob, &mut carol], vec![(1, Out { to: carol_key, wire: post })]);
            heard += texts(&ev[2]).len();
        }
        assert_eq!(heard, FLOOD_MAX);
        // From the far future: dropped.
        let post = bob.space().encrypt(&Post::Message { id: 1, channel: 0, text: "hi".into(), reply_to: None, at_secs: now + 3600 }).unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], vec![(1, Out { to: carol_key, wire: post })]);
        assert!(texts(&ev[2]).is_empty());
    }

    #[test]
    fn a_withdrawn_invite_is_refused() {
        let (mut alice, mut bob, mut carol) = three();
        let mut dave = person("Dave");
        let id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let outs = alice.space().invite(&id, dave.key(), "Dave").unwrap();
        assert_eq!(alice.space().pending_invites().len(), 1);
        // Dave's Join is held back until Alice withdraws the invite.
        let (_, joins) = run_holding(&mut [&mut alice, &mut bob, &mut carol, &mut dave], outs.into_iter().map(|o| (0, o)).collect(), &|o| matches!(o.wire, Wire::Join { .. }));
        assert!(alice.space().withdraw_invite(dave.key()));
        assert!(alice.space().pending_invites().is_empty());
        run(&mut [&mut alice, &mut bob, &mut carol, &mut dave], joins);
        assert!(!alice.space().manifest().is_member(&dave.key()), "not let in");
        // So is one answered after it ran out.
        let outs = alice.space().invite(&id, dave.key(), "Dave").unwrap();
        let (_, joins) = run_holding(&mut [&mut alice, &mut bob, &mut carol, &mut dave], outs.into_iter().map(|o| (0, o)).collect(), &|o| matches!(o.wire, Wire::Join { .. }));
        alice.space().offers[0].at_secs = now_secs() - OFFER_LIFETIME_SECS - 1;
        run(&mut [&mut alice, &mut bob, &mut carol, &mut dave], joins);
        assert!(!alice.space().manifest().is_member(&dave.key()));
    }

    #[test]
    fn a_report_reaches_only_the_moderators() {
        let (mut alice, mut bob, mut carol) = three();
        let (outs, mine) = bob.space().post(0, "something nasty", None).unwrap();
        let Event::Message { id, .. } = mine else { panic!() };
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (1, o)).collect());
        let excerpt = Excerpt { id, author: Key(bob.key()), text: "something nasty".into(), at_secs: now_secs() };
        let outs = carol.space().report(0, "rude", vec![excerpt.clone()]).unwrap();
        let alice_key = alice.key();
        assert!(outs.iter().all(|o| o.to == alice_key), "only to the owner");
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (2, o)).collect());
        assert!(ev[0].iter().any(|e| matches!(e, Event::Reported(r) if r.reason == "rude" && r.excerpts == vec![excerpt.clone()])));
        assert_eq!(alice.space().reports().len(), 1);
        let rid = alice.space().reports()[0].id;
        assert!(alice.space().dismiss_report(rid));
        assert!(alice.space().reports().is_empty());
        // A plain member handed one directly doesn't keep it.
        let post = carol.space().encrypt(&Post::Report { id: 5, channel: 0, reason: "x".into(), excerpts: vec![excerpt], at_secs: now_secs() }).unwrap();
        let bob_key = bob.key();
        run(&mut [&mut alice, &mut bob, &mut carol], vec![(2, Out { to: bob_key, wire: post })]);
        assert!(bob.space().reports().is_empty());
    }

    #[test]
    fn a_voice_channel_says_who_is_in_it() {
        let (mut alice, mut bob, mut carol) = three();
        let alice_id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let outs = alice
            .space()
            .update(&alice_id, |m| m.channels.push(antiphon_crypto::community::Channel { id: 1, name: "lounge".into(), voice: true, ..Default::default() }))
            .unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        assert!(bob.space().voice_join(0, "stoop://x").is_err(), "a text channel has no call");
        let outs = bob.space().voice_join(1, "stoop://bob").unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (1, o)).collect());
        let here = alice.space().voice_here(1);
        assert_eq!(here.len(), 1);
        assert_eq!((here[0].key.0, here[0].invite.as_str()), (bob.key(), "stoop://bob"));
        // Word that waited in a mailbox says nothing about now.
        let stale = carol.space().encrypt(&Post::Voice { channel: 1, here: true, invite: "stoop://carol".into(), at_secs: now_secs() - VOICE_TTL_SECS - 5 }).unwrap();
        let alice_key = alice.key();
        run(&mut [&mut alice, &mut bob, &mut carol], vec![(2, Out { to: alice_key, wire: stale })]);
        assert_eq!(alice.space().voice_here(1).len(), 1);
        let outs = bob.space().voice_leave(1).unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (1, o)).collect());
        assert!(alice.space().voice_here(1).is_empty());
    }

    #[test]
    fn a_stranger_joins_a_listed_community_by_asking() {
        let (mut alice, mut bob, mut carol) = three();
        let mut dave = person("Dave");
        let community = alice.space().id();
        let knock = vec![(3, Out { to: alice.key(), wire: Wire::Knock { community, card: dave.card(), proof: None, ticket: None } })];
        // Not listed: asking does nothing.
        run(&mut [&mut alice, &mut bob, &mut carol, &mut dave], knock.clone());
        assert!(dave.space.is_none());
        let alice_id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let outs = alice.space().update(&alice_id, |m| m.listed = true).unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol, &mut dave], outs.into_iter().map(|o| (0, o)).collect());
        let ev = run(&mut [&mut alice, &mut bob, &mut carol, &mut dave], knock);
        assert!(ev[3].contains(&Event::Joined), "{ev:?}");
        assert!(bob.space().manifest().is_member(&dave.key()));
    }

    #[test]
    fn two_communities_meet_in_a_shared_space() {
        // Alice runs the shared space for her community A; Bob is an
        // admin of community B, Carol a member of B.
        let (mut alice, mut bob, mut carol) = (person("Alice"), person("Bob"), person("Carol"));
        let (a, b, shared) = (Key([1; 32]), Key([2; 32]), Key([4; 32]));
        alice.space = Some(Space::create(&alice.id, shared, Kind::Community, "A and B", alice.card(), false).unwrap());
        let alice_id = alice.acting();
        alice.space().update(&alice_id, |m| m.alliance.push(a)).unwrap();
        let ticket = alice.space().new_ticket().unwrap();
        let bob_card = bob.card();

        // Without a ticket or anyone to vouch for them, B can't come in.
        let proof = Proof::make(&bob.id, b, &shared, &bob.key());
        assert!(alice.space().ally_knock(&alice_id, bob.key(), &bob_card, &proof, None, false).is_err());
        // A proof for another space, or by someone else, doesn't hold.
        let elsewhere = Proof::make(&bob.id, b, &Key([8; 32]), &bob.key());
        assert!(alice.space().ally_knock(&alice_id, bob.key(), &bob_card, &elsewhere, Some(ticket), false).is_err());
        // With the ticket, Bob brings B in and becomes an owner.
        let outs = alice.space().ally_knock(&alice_id, bob.key(), &bob_card, &proof, Some(ticket), false).unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        assert!(ev[1].contains(&Event::Joined), "{ev:?}");
        assert_eq!(alice.space().manifest().alliance, vec![a, b]);
        assert!(alice.space().manifest().is_owner(&bob.key()));
        // The ticket worked once.
        let again = Proof::make(&carol.id, b, &shared, &carol.key());
        assert!(alice.space().ally_knock(&alice_id, carol.key(), &carol.card(), &again, Some(ticket), false).is_err());

        // Carol asks Bob, who finds her in B: she's let in, and all three
        // talk there though she is in neither A nor Alice's contacts.
        let bob_id = bob.acting();
        let outs = bob.space().ally_knock(&bob_id, carol.key(), &carol.card(), &again, None, true).unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (1, o)).collect());
        assert!(ev[2].contains(&Event::Joined), "{ev:?}");
        let (outs, _) = carol.space().post(0, "hello from B", None).unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (2, o)).collect());
        assert_eq!(texts(&ev[0]), ["hello from B"]);
        // A community that doesn't share it gets nowhere, vouched or not.
        let dave = person("Dave");
        let outsider = Proof::make(&dave.id, Key([6; 32]), &shared, &dave.key());
        assert!(bob.space().ally_knock(&bob_id, dave.key(), &dave.card(), &outsider, None, true).is_err());
        assert!(dave.space.is_none());
    }

    #[test]
    fn a_shared_space_link_round_trips() {
        let alice = person("Alice");
        let join = JoinLink { community: Key([7; 32]), host: alice.card() };
        let link = format!("{ALLY_PREFIX}{}.{}", hex(&[5; 32]), join.to_link().trim_start_matches(JOIN_PREFIX));
        let (ticket, back) = parse_ally_link(&link).unwrap();
        assert_eq!((ticket, back.community, back.host.key), (Key([5; 32]), join.community, join.host.key));
        assert!(parse_ally_link(&join.to_link()).is_none());
    }

    #[test]
    fn a_join_link_round_trips() {
        let alice = person("Alice");
        let link = JoinLink { community: Key([7; 32]), host: alice.card() };
        let text = link.to_link();
        assert!(text.starts_with(JOIN_PREFIX) && text.len() < 600, "{text}");
        // The name isn't carried: the directory entry gives one.
        let back = JoinLink::parse(&text).unwrap();
        assert_eq!((back.community, back.host.key, &back.host.public), (link.community, link.host.key, &link.host.public));
        assert_eq!(JoinLink::parse("stoop://join/AAAA"), None);
    }

    #[test]
    fn a_group_survives_a_restart() {
        let (mut alice, mut bob, mut carol) = three();
        // Everyone restarts: the space comes back from its saved bytes.
        for p in [&mut alice, &mut bob, &mut carol] {
            let saved = p.space().save();
            p.space = Some(Space::restore(&p.id, &saved).unwrap());
        }
        let (outs, _) = alice.space().post(0, "still here", None).unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        assert_eq!(texts(&ev[1]), ["still here"]);
        assert_eq!(texts(&ev[2]), ["still here"]);
    }

    #[test]
    fn a_ban_removes_them_and_they_cannot_read_what_follows() {
        let (mut alice, mut bob, mut carol) = three();
        let bob_id = IdentityKeyPair::from_master_seed(bob.id.master_seed());
        // Bob is a plain member: he can't ban, locally or by forging.
        assert!(bob.space().ban(&bob_id, carol.key(), "no").is_err());
        let head = alice.space.as_ref().unwrap();
        let mut forged = head.manifest().next(head.signed());
        forged.members.retain(|m| m.key.0 != carol.key());
        let signed = SignedManifest::sign(&forged, &bob_id);
        let post = bob.space().encrypt(&Post::Manifest { manifest: b64(&signed.encode()) }).unwrap();
        let alice_key = alice.key();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], vec![(1, Out { to: alice_key, wire: post })]);
        assert!(ev[0].iter().any(|e| matches!(e, Event::Notice(_))), "refused with a reason");
        assert!(alice.space().manifest().is_member(&carol.key()));

        // Alice (the owner) bans Carol.
        let alice_id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let outs = alice.space().ban(&alice_id, carol.key(), "spam").unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        assert!(ev[2].contains(&Event::Removed));
        assert!(carol.space().removed);
        assert!(bob.space().manifest().is_banned(&carol.key()));
        assert_eq!(bob.space().readers().len(), 2);

        // What Alice says next reaches Bob and not Carol, even handed to
        // her directly.
        let (outs, _) = alice.space().post(0, "after the ban", None).unwrap();
        assert!(outs.iter().all(|o| o.to != carol.key()), "not even sent to her");
        let wire = outs[0].wire.clone();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        assert_eq!(texts(&ev[1]), ["after the ban"]);
        let carol_id = IdentityKeyPair::from_master_seed(carol.id.master_seed());
        carol.space.as_mut().unwrap().removed = false; // as if she ignored the ban
        let h = carol.space().handle(&carol_id, alice.key(), wire);
        assert!(texts(&h.events).is_empty());
        // And she can't rejoin by asking: the manifest lists her as banned.
        assert!(alice.space().invite(&alice_id, carol.key(), "Carol").is_err());
    }

    #[test]
    fn messages_from_a_later_epoch_wait_for_the_commit() {
        let (mut alice, mut bob, mut carol) = three();
        let mut dave = person("Dave");
        let alice_id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let carol_key = carol.key();
        // Alice adds Dave; the commit telling Carol is held up in the mail.
        let outs = alice.space().invite(&alice_id, dave.key(), "Dave").unwrap();
        let (_, held) = run_holding(
            &mut [&mut alice, &mut bob, &mut carol, &mut dave],
            outs.into_iter().map(|o| (0, o)).collect(),
            &|o| o.to == carol_key && matches!(o.wire, Wire::Commit { .. }),
        );
        assert_eq!(held.len(), 1);
        assert_eq!(dave.space().readers().len(), 4);
        // Alice's next post, in the new epoch, reaches Carol first: held,
        // not lost...
        let (outs, _) = alice.space().post(0, "hello Dave", None).unwrap();
        let to_carol: Vec<_> = outs.into_iter().filter(|o| o.to == carol_key).map(|o| (0, o)).collect();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol, &mut dave], to_carol);
        assert!(texts(&ev[2]).is_empty());
        // ...and read once the commit arrives.
        let ev = run(&mut [&mut alice, &mut bob, &mut carol, &mut dave], held);
        assert_eq!(texts(&ev[2]), ["hello Dave"]);
    }

    /// The same seed as `p`, holding a space of its own: one person in a
    /// second community.
    fn twin(p: &Person) -> Person {
        Person { id: IdentityKeyPair::from_master_seed(p.id.master_seed()), space: None, name: p.name, separate: p.separate, look: p.look.clone() }
    }

    fn look(name: &str) -> Appearance {
        Appearance { name: name.into(), ..Default::default() }
    }

    /// `owner` makes a community and invites `joiner`, who answers as
    /// their settings say.
    fn join(owner: &mut Person, joiner: &mut Person, id: u8, name: &str) {
        owner.space = Some(Space::create(&owner.id, Key([id; 32]), Kind::Community, name, owner.card(), false).unwrap());
        let me = owner.acting();
        let outs = owner.space().invite(&me, joiner.key(), "Bob from work").unwrap();
        let ev = run(&mut [owner, joiner], outs.into_iter().map(|o| (0, o)).collect());
        assert!(ev[1].contains(&Event::Joined), "{ev:?}");
    }

    /// Spec §7 "per-community identity": Bob is in two communities under
    /// two separate identities, and Dave in the same two as himself with
    /// two names.
    #[test]
    fn two_communities_see_different_names_and_keys_for_the_same_user() {
        let (mut alice, mut carol) = (person("Alice"), person("Carol"));
        let mut bob_a = person("Bob");
        bob_a.separate = true;
        bob_a.look = look("Robin");
        let mut bob_b = twin(&bob_a);
        bob_b.look = look("Knight");
        join(&mut alice, &mut bob_a, 1, "Choir");
        join(&mut carol, &mut bob_b, 2, "Chess");

        let in_a = bob_a.here();
        let in_b = bob_b.here();
        let main = bob_a.key();
        assert_ne!(in_a, in_b);
        assert_ne!(in_a, main);
        assert_ne!(in_b, main);
        // Each is exactly the key derived for that community.
        assert_eq!(in_a, antiphon_crypto::persona::community_identity(&bob_a.id, &[1; 32]).public().identity_key);

        let a = alice.space.as_ref().unwrap();
        let b = carol.space.as_ref().unwrap();
        // Listed, in the group, and named as they chose -- not "Bob from
        // work", which is what the inviter called his main key.
        assert!(a.manifest().is_member(&in_a) && a.readers().contains(&in_a));
        assert!(b.manifest().is_member(&in_b) && b.readers().contains(&in_b));
        assert_eq!(a.name_of(&in_a), "Robin");
        assert_eq!(b.name_of(&in_b), "Knight");
        assert_eq!(a.manifest().member(&in_a).unwrap().name, "Robin");
        // His main key is in neither community's history: not one version
        // of either manifest, nor any card, ever named it.
        for s in [a, b] {
            assert!(s.chain.iter().all(|(signed, m)| !m.is_member(&main) && !m.owners.contains(&Key(main)) && signed.signer != main));
            assert!(s.cards.iter().all(|c| c.key.0 != main));
            assert!(s.readers().iter().all(|k| *k != main));
        }

        // Dave joins both as himself, with a different name in each: the
        // names differ, the key -- deliberately -- does not.
        let mut dave_a = person("Dave");
        dave_a.look = look("Tenor Dave");
        let mut dave_b = twin(&dave_a);
        dave_b.look = look("D4");
        let me = alice.acting();
        let outs = alice.space().invite(&me, dave_a.key(), "Dave").unwrap();
        run(&mut [&mut alice, &mut bob_a, &mut dave_a], outs.into_iter().map(|o| (0, o)).collect());
        let me = carol.acting();
        let outs = carol.space().invite(&me, dave_b.key(), "Dave").unwrap();
        run(&mut [&mut carol, &mut bob_b, &mut dave_b], outs.into_iter().map(|o| (0, o)).collect());
        assert_eq!(dave_a.here(), dave_b.here());
        // Joined as himself: the manifest keeps the inviter's name for
        // him, as it always did; members see the one he chose.
        assert_eq!(alice.space().manifest().member(&dave_a.key()).unwrap().name, "Dave");
        assert_eq!(alice.space().name_of(&dave_a.key()), "Tenor Dave");
        assert_eq!(carol.space().card(&dave_b.key()).unwrap().name, "D4");
        // Bob, already in A, learned Dave's card for A from the commit.
        assert_eq!(bob_a.space().card(&dave_a.key()).unwrap().name, "Tenor Dave");
    }

    /// The separate identity comes from the seed, so restoring from the
    /// recovery phrase (or shares) gets you back into the community.
    #[test]
    fn a_separate_identity_survives_recovery_from_the_seed() {
        let mut alice = person("Alice");
        let mut bob = person("Bob");
        bob.separate = true;
        join(&mut alice, &mut bob, 5, "Choir");
        let before = bob.here();
        let saved = bob.space().save();

        // A new device: only the phrase came along.
        let phrase = antiphon_crypto::mnemonic::phrase_from_seed(&bob.id.master_seed());
        let restored = IdentityKeyPair::from_master_seed(antiphon_crypto::mnemonic::seed_from_phrase(&phrase).unwrap());
        let mut bob2 = Person { id: restored, space: Some(Space::restore(&IdentityKeyPair::from_master_seed(bob.id.master_seed()), &saved).unwrap()), name: "Bob", separate: true, look: Appearance::default() };
        assert_eq!(bob2.here(), before);
        let (outs, _) = bob2.space().post(0, "back again", None).unwrap();
        let ev = run(&mut [&mut alice, &mut bob2], outs.into_iter().map(|o| (1, o)).collect());
        assert!(ev[0].iter().any(|e| matches!(e, Event::Message { sender, text, .. } if *sender == before && text == "back again")));
    }

    /// What Bob writes in A is his A identity's, and nothing in B -- not
    /// his key there, not his main key -- can be shown to have written it.
    #[test]
    fn a_message_in_one_community_cannot_be_attributed_in_another() {
        let (mut alice, mut carol) = (person("Alice"), person("Carol"));
        let mut bob_a = person("Bob");
        bob_a.separate = true;
        let mut bob_b = twin(&bob_a);
        join(&mut alice, &mut bob_a, 1, "Choir");
        join(&mut carol, &mut bob_b, 2, "Chess");
        let (in_a, in_b) = (bob_a.here(), bob_b.here());

        let (outs, _) = bob_a.space().post(0, "only for the choir", None).unwrap();
        let wire = outs[0].wire.clone();
        let ev = run(&mut [&mut alice, &mut bob_a], outs.into_iter().map(|o| (1, o)).collect());
        assert!(ev[0].iter().any(|e| matches!(e, Event::Message { sender, .. } if *sender == in_a)));

        // Handed to Carol, it is for a community she isn't in.
        let carol_id = carol.acting();
        let h = carol.space().handle(&carol_id, in_b, wire);
        assert!(texts(&h.events).is_empty());

        // His A card is self-signed by the A key; it can't be passed off
        // as -- or checked against -- the key B knows, or his main key.
        let card = alice.space().card(&in_a).unwrap().clone();
        assert!(card.prekey().is_some());
        for other in [in_b, bob_a.key()] {
            let relabelled = Card { key: Key(other), ..card.clone() };
            assert!(relabelled.prekey().is_none());
        }
        // And the A key's signatures don't verify as either.
        let a_id = bob_a.acting();
        let sig = a_id.sign(b"hello");
        assert!(antiphon_crypto::verify_signature(&in_b, b"hello", &sig).is_err());
        assert!(antiphon_crypto::verify_signature(&bob_a.key(), b"hello", &sig).is_err());
    }

    /// A picture chosen for a community reaches members who were there
    /// first and those who come after, though bulk cards leave it out.
    #[test]
    fn a_community_picture_reaches_everyone() {
        let (mut alice, mut bob, mut carol) = (person("Alice"), person("Bob"), person("Carol"));
        bob.look = Appearance { name: "Robin".into(), status: "humming".into(), avatar: "QUJD".into() };
        carol.look = Appearance { avatar: "WFla".into(), ..Default::default() };
        // Carol made it, with her picture; Alice joins first, then Bob.
        carol.space = Some(Space::create(&carol.id, Key([8; 32]), Kind::Group, "Friday", carol.card_as(&carol.id), false).unwrap());
        let me = carol.acting();
        let outs = carol.space().invite(&me, alice.key(), "Alice").unwrap();
        run(&mut [&mut carol, &mut alice, &mut bob], outs.into_iter().map(|o| (0, o)).collect());
        let outs = carol.space().invite(&me, bob.key(), "Bob").unwrap();
        run(&mut [&mut carol, &mut alice, &mut bob], outs.into_iter().map(|o| (0, o)).collect());

        let bob_card = alice.space().card(&bob.key()).unwrap().clone();
        assert_eq!(bob_card.avatar.as_deref(), Some("QUJD"), "Alice, there first, has Bob's picture");
        assert_eq!(bob_card.status, "humming");
        assert_eq!(bob.space().card(&carol.key()).unwrap().avatar.as_deref(), Some("WFla"), "Bob, the newcomer, has Carol's");
        // Too big a picture is not taken.
        let huge = Card { avatar: Some("A".repeat(MAX_AVATAR + 1)), ..bob_card };
        let saved = alice.space().save();
        let mut s = Space::restore(&alice.id, &saved).unwrap();
        s.learn(huge);
        assert_eq!(s.card(&bob.key()).unwrap().avatar.as_deref(), Some("QUJD"));
    }

    #[test]
    fn fragments_fit_a_mailbox_and_reassemble_in_any_order() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let wire = Wire::App { community: Key([1; 32]), data: b64(&vec![7u8; 3000]) };
        let mut frags = fragments(&wire);
        assert!(frags.len() > 1);
        // Each, sealed in a mail body, fits a mailbox blob.
        for f in &frags {
            let body = crate::delivery::MailBody { id: u64::MAX, text: String::new(), at_secs: u64::MAX, group: Some(f.clone()), reply_to: None };
            let sealed = antiphon_crypto::mail::seal(&alice, &bob.public().identity_key, &bob.public().prekey, &serde_json::to_vec(&body).unwrap()).unwrap();
            assert!(sealed.len() <= antiphon_net::mailbox::MAX_BLOB, "{}", sealed.len());
        }
        frags.reverse();
        let mut r = Reassembly::default();
        let mut got = None;
        for f in &frags {
            got = r.feed(alice.public().identity_key, f).or(got);
        }
        assert_eq!(got, Some(wire));
        assert!(r.feed([0; 32], "junk").is_none());
    }

    /// Spec §9 over spec §8: three members, messages carried by a real
    /// mailbox node on 127.0.0.1, sealed per recipient and fragmented.
    #[tokio::test]
    async fn three_members_talk_through_a_real_mailbox_node() {
        use antiphon_relay::mailbox::{Mailbox, MailboxLimits};
        use tokio::net::UdpSocket;
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let node = sock.local_addr().unwrap().to_string();
        let mb = Arc::new(Mutex::new(Mailbox::new(MailboxLimits { enabled: true, deposits_per_minute: 1000, ..Default::default() })));
        tokio::spawn(antiphon_relay::run_node(sock, Arc::new(Mutex::new(antiphon_relay::Relay::new(Default::default()))), Some(mb), None));

        let mut routes = crate::delivery::default_routes();
        routes[2].nodes = vec![node.clone()];
        let mk = |name: &'static str| {
            let p = person(name);
            let record = crate::delivery::my_record(&p.id, &routes, false, 1);
            let card = Card::mine(&p.id, crate::delivery::encode_record(&record), name);
            (p, card)
        };
        let (mut alice, a_card) = mk("Alice");
        let (mut bob, b_card) = mk("Bob");
        let (mut carol, c_card) = mk("Carol");
        // Each registers at the node by collecting once.
        for p in [&alice, &bob, &carol] {
            crate::delivery::collect(&p.id, &routes, false, false, |_| true).await;
        }
        alice.space = Some(Space::create(&alice.id, Key([4; 32]), Kind::Group, "Friday", a_card, false).unwrap());
        let cards = [b_card.clone(), c_card.clone()];

        /// Mails every Out to its recipient's node, then has everyone
        /// collect and handle what arrived, until it all goes quiet.
        async fn deliver(people: &mut [&mut Person], cards: &[Card], routes: &[crate::delivery::RouteConfig], mut outs: Vec<(usize, Out)>, own: &[Card]) -> Vec<Vec<Event>> {
            let mut events = vec![Vec::new(); people.len()];
            let mut assembly: Vec<Reassembly> = vec![Reassembly::default(); people.len()];
            for _ in 0..20 {
                if outs.is_empty() {
                    break;
                }
                for (from, out) in std::mem::take(&mut outs) {
                    let card = own.iter().chain(cards).find(|c| c.key.0 == out.to).expect("a card for everyone");
                    for f in fragments(&out.wire) {
                        let body = crate::delivery::MailBody { id: random_id(), text: String::new(), at_secs: 1, group: Some(f), reply_to: None };
                        let sent = crate::delivery::send(&people[from].id, &out.to, &card.prekey().unwrap(), &card.record().unwrap(), &body, false).await;
                        assert_eq!(sent, crate::delivery::Sent::Deposited(1));
                    }
                }
                for i in 0..people.len() {
                    let mut wires = Vec::new();
                    let id = IdentityKeyPair::from_master_seed(people[i].id.master_seed());
                    let r = &mut assembly[i];
                    crate::delivery::collect(&id, routes, false, false, |m| {
                        if let Some(w) = m.body.group.as_ref().and_then(|f| r.feed(m.from, f)) {
                            wires.push((m.from, w));
                        }
                        true
                    })
                    .await;
                    for (from, wire) in wires {
                        let p = &mut *people[i];
                        if let Wire::Invite { manifest, card } = wire {
                            let invite = Invite { from: Key(from), manifest, card, name: String::new(), kind: Kind::Group, at_secs: 0 };
                            let (space, join) = Space::accept(&p.id, &invite, p.card_with_routes(own), false).unwrap();
                            p.space = Some(space);
                            outs.push((i, join));
                            continue;
                        }
                        let Some(space) = p.space.as_mut() else { continue };
                        let h = space.handle(&id, from, wire);
                        events[i].extend(h.events);
                        outs.extend(h.outs.into_iter().map(|o| (i, o)));
                    }
                }
            }
            events
        }

        let a_key = alice.key();
        let own = [alice.space().card(&a_key).cloned().unwrap(), b_card.clone(), c_card.clone()];
        let alice_id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let mut outs: Vec<(usize, Out)> = alice.space().invite(&alice_id, bob.key(), "Bob").unwrap().into_iter().map(|o| (0, o)).collect();
        outs.extend(alice.space().invite(&alice_id, carol.key(), "Carol").unwrap().into_iter().map(|o| (0, o)));
        deliver(&mut [&mut alice, &mut bob, &mut carol], &cards, &routes, outs, &own).await;
        assert_eq!(alice.space().readers().len(), 3, "both joined through the node");
        assert!(bob.space.as_ref().unwrap().joining.is_none() && carol.space.as_ref().unwrap().joining.is_none());

        let (outs, _) = bob.space().post(0, "see you at eight", None).unwrap();
        let ev = deliver(&mut [&mut alice, &mut bob, &mut carol], &cards, &routes, outs.into_iter().map(|o| (1, o)).collect(), &own).await;
        assert_eq!(texts(&ev[0]), ["see you at eight"]);
        assert_eq!(texts(&ev[2]), ["see you at eight"]);
        let (outs, _) = carol.space().post(0, "on my way", None).unwrap();
        let ev = deliver(&mut [&mut alice, &mut bob, &mut carol], &cards, &routes, outs.into_iter().map(|o| (2, o)).collect(), &own).await;
        assert_eq!(texts(&ev[0]), ["on my way"]);
        assert_eq!(texts(&ev[1]), ["on my way"]);
    }

    /// Spec §7 over spec §8: Bob joins under a separate identity through a
    /// real mailbox node on 127.0.0.1. The Welcome reaches the inbox that
    /// identity registered -- under labels unrelated to his own -- and what
    /// he posts arrives sealed as that identity, never his main key.
    #[tokio::test]
    async fn a_separate_identity_joins_and_talks_through_a_real_mailbox_node() {
        use antiphon_relay::mailbox::{Mailbox, MailboxLimits};
        use tokio::net::UdpSocket;
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let node = sock.local_addr().unwrap().to_string();
        let mb = Arc::new(Mutex::new(Mailbox::new(MailboxLimits { enabled: true, deposits_per_minute: 1000, ..Default::default() })));
        tokio::spawn(antiphon_relay::run_node(sock, Arc::new(Mutex::new(antiphon_relay::Relay::new(Default::default()))), Some(mb), None));
        let mut routes = crate::delivery::default_routes();
        routes[2].nodes = vec![node.clone()];
        let profile = |n: &str| crate::config::ProfileData { display_name: n.into(), status_message: String::new(), ..Default::default() };
        let card_of = |id: &IdentityKeyPair, n: &str| {
            let record = crate::delivery::my_record(id, &routes, false, 1);
            card_as(id, crate::delivery::encode_record(&record), &profile(n), &Appearance::default())
        };

        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let community = Key([6; 32]);
        let bob_here = identity_for(&bob, &community, true);
        let cards = [card_of(&alice, "Alice"), card_of(&bob, "Bob"), card_of(&bob_here, "Robin")];
        assert_ne!(
            crate::delivery::mailboxes_in(&cards[1].record().unwrap()),
            crate::delivery::mailboxes_in(&cards[2].record().unwrap()),
            "his two inboxes at the node have unrelated labels"
        );
        for id in [&alice, &bob, &bob_here] {
            crate::delivery::collect(id, &routes, false, false, |_| true).await;
        }
        let mut a_space = Space::create(&alice, community, Kind::Group, "Friday", cards[0].clone(), false).unwrap();
        let mut b_space: Option<Space> = None;

        // (sender, Out): sealed with `sender`, to whoever `out.to` is.
        let mut outs: Vec<(&IdentityKeyPair, Out)> = a_space.invite(&alice, bob.public().identity_key, "Bob").unwrap().into_iter().map(|o| (&alice, o)).collect();
        let mut alice_heard = Vec::new();
        let mut alice_froms = Vec::new();
        let mut r = [Reassembly::default(), Reassembly::default(), Reassembly::default()];
        for round in 0..20 {
            if round == 6 {
                let (o, _) = b_space.as_mut().expect("bob joined").post(0, "hi, it's Robin", None).unwrap();
                outs.extend(o.into_iter().map(|o| (&bob_here, o)));
            }
            for (from, out) in std::mem::take(&mut outs) {
                let card = cards.iter().find(|c| c.key.0 == out.to).expect("a card for everyone");
                for f in fragments(&out.wire) {
                    let body = crate::delivery::MailBody { id: random_id(), text: String::new(), at_secs: 1, group: Some(f), reply_to: None };
                    let sent = crate::delivery::send(from, &out.to, &card.prekey().unwrap(), &card.record().unwrap(), &body, false).await;
                    assert_eq!(sent, crate::delivery::Sent::Deposited(1));
                }
            }
            for (i, id) in [&alice, &bob, &bob_here].into_iter().enumerate() {
                let mut wires = Vec::new();
                let ri = &mut r[i];
                crate::delivery::collect(id, &routes, false, false, |m| {
                    if let Some(w) = m.body.group.as_ref().and_then(|f| ri.feed(m.from, f)) {
                        wires.push((m.from, w));
                    }
                    true
                })
                .await;
                for (from, wire) in wires {
                    match i {
                        0 => {
                            alice_froms.push(from);
                            let h = a_space.handle(&alice, from, wire);
                            alice_heard.extend(h.events);
                            outs.extend(h.outs.into_iter().map(|o| (&alice, o)));
                        }
                        1 => {
                            // His main inbox: the invite, answered as Robin.
                            let Wire::Invite { manifest, card } = wire else { panic!("only the invite comes here") };
                            let invite = Invite { from: Key(from), manifest, card, name: String::new(), kind: Kind::Group, at_secs: 0 };
                            let (space, join) = Space::accept(&bob_here, &invite, cards[2].clone(), true).unwrap();
                            b_space = Some(space);
                            outs.push((&bob, join));
                        }
                        _ => {
                            let h = b_space.as_mut().unwrap().handle(&bob_here, from, wire);
                            outs.extend(h.outs.into_iter().map(|o| (&bob_here, o)));
                        }
                    }
                }
            }
            if round > 6 && outs.is_empty() {
                break;
            }
        }
        let robin = bob_here.public().identity_key;
        assert!(b_space.as_ref().unwrap().joining.is_none(), "the Welcome reached his separate inbox");
        assert!(a_space.readers().contains(&robin) && !a_space.readers().contains(&bob.public().identity_key));
        assert!(alice_heard.iter().any(|e| matches!(e, Event::Message { sender, text, .. } if *sender == robin && text == "hi, it's Robin")));
        // Only the Join came sealed with his main key, and only to Alice
        // who invited him; everything else arrived as Robin.
        let main_count = alice_froms.iter().filter(|k| **k == bob.public().identity_key).count();
        assert!(main_count >= 1);
        assert!(alice_froms.iter().all(|k| *k == robin || *k == bob.public().identity_key));
    }

    #[test]
    fn with_a_board_manifest_changes_travel_as_deltas() {
        let (mut alice, mut bob, mut carol) = three();
        let alice_id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let board = cm::BoardRef { addr: "node.example".into(), id: Key([8; 32]) };
        let outs = alice.space().update(&alice_id, |m| m.nodes.board = Some(board.clone())).unwrap();
        // Naming the board is itself a whole version, as nobody knew of it.
        assert!(outs.iter().all(|o| !matches!(&o.wire, Wire::App { .. }) || alice.space.as_ref().unwrap().settled_board().is_none()));
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        assert_eq!(bob.space().board(), Some(&board));
        assert_eq!(bob.space().manifest().max_members(), cm::MAX_BOARD_COMMUNITY_MEMBERS);

        // Now a ban: the commit carries only the change, and everyone ends
        // on the same signed version.
        let outs = alice.space().ban(&alice_id, carol.key(), "spam").unwrap();
        let commit = outs.iter().find_map(|o| if let Wire::Commit { manifest, .. } = &o.wire { Some(manifest.clone()) } else { None }).unwrap();
        assert!(commit.starts_with("d:"), "a delta");
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        assert!(carol.space().removed);
        assert_eq!(bob.space().signed(), alice.space().signed());
        assert!(bob.space().manifest().is_banned(&carol.key()));
        // Both hold the same board key for the epoch they're in, and
        // Carol, out of the group, never learns it.
        let (a, b) = (alice.space().board_keys().last().cloned().unwrap(), bob.space().board_keys().last().cloned().unwrap());
        assert_eq!(a, b);
        assert_eq!(Some(a.epoch), alice.space().epoch());
        assert!(carol.space().board_keys().iter().all(|k| k.epoch < a.epoch));
    }

    #[test]
    fn a_delta_on_a_version_we_lack_asks_for_the_whole() {
        let (mut alice, mut bob, mut carol) = three();
        let alice_id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let outs = alice.space().update(&alice_id, |m| m.nodes.board = Some(cm::BoardRef { addr: "n".into(), id: Key([8; 32]) })).unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        // Two changes; Bob misses the first.
        let bob_key = bob.key();
        let outs = alice.space().update(&alice_id, |m| m.name = "Choir II".into()).unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().filter(|o| o.to != bob_key).map(|o| (0, o)).collect());
        let outs = alice.space().update(&alice_id, |m| m.description = "We sing".into()).unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        assert_eq!(bob.space().manifest().description, "We sing");
        assert_eq!(bob.space().signed(), alice.space().signed());
    }

    /// A store of one's own in a fresh folder, as the app has.
    fn store(dir: &Path, name: &str) -> Store {
        let dir = dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let key = crate::history::load_or_create_backup_key(&dir).unwrap();
        Store { dir, key, identity: Arc::new(IdentityKeyPair::generate()) }
    }

    /// Hands what `from` has queued for `to` over, as mail would.
    fn deliver(from: &Store, to: &Store) -> usize {
        let them = Key(to.identity.public().identity_key);
        let mut hub = from.hub();
        let (theirs, rest): (Vec<Queued>, Vec<Queued>) = hub.outbox.into_iter().partition(|q| q.to == them);
        hub.outbox = rest;
        from.save_hub(&hub);
        for q in &theirs {
            to.receive_fragment(from.identity.public().identity_key, &q.fragment);
        }
        theirs.len()
    }

    fn said(store: &Store, id: &Key) -> Vec<String> {
        crate::history::load(&store.dir, &channel_log(id, 0), &store.key).into_iter().map(|m| m.text).collect()
    }

    /// Invites `who` by mail and has them accept; the inviter's answer
    /// (the Welcome, or with a board a pointer to it) is left queued.
    fn invite_in(owner: &Store, id: &Key, who: &Store, name: &str) {
        let key = who.identity.public().identity_key;
        owner.with_space(id, |s, identity| Ok(((), s.invite(identity, key, name)?, Vec::new()))).unwrap();
        // As if they had asked from its join link: an invitation from a
        // stranger is otherwise dropped.
        let mut hub = who.hub();
        hub.knocked.push((*id, now_secs()));
        who.save_hub(&hub);
        deliver(owner, who);
        who.accept_invite(0, false).unwrap();
        deliver(who, owner);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_community_with_a_board_posts_once_and_everyone_reads_it() {
        let dir = std::env::temp_dir().join(format!("antiphon-board-{}", random_id()));
        // A mailbox node with boards, on this machine.
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let node = socket.local_addr().unwrap().to_string();
        let mailbox = antiphon_relay::mailbox::Mailbox::new(antiphon_relay::mailbox::MailboxLimits { enabled: true, ..Default::default() });
        let mailbox = Arc::new(Mutex::new(mailbox));
        let relay = Arc::new(Mutex::new(antiphon_relay::Relay::new(Default::default())));
        tokio::spawn(antiphon_relay::run_node(socket, relay, Some(mailbox.clone()), None));

        let (alice, bob, carol) = (store(&dir, "alice"), store(&dir, "bob"), store(&dir, "carol"));
        let id = alice.create(Kind::Community, "Big", false).unwrap();
        invite_in(&alice, &id, &bob, "Bob");
        deliver(&alice, &bob);
        assert!(bob.load(&id).unwrap().joining.is_none(), "Bob is in");

        // The board is named by mail; from then on, it carries what is for
        // everyone.
        alice.set_board(&id, Some(node.clone())).unwrap();
        deliver(&alice, &bob);
        assert!(bob.load(&id).unwrap().settled_board().is_none());
        alice.with_space(&id, |s, identity| Ok(((), s.update(identity, |m| m.description = "big".into())?, Vec::new()))).unwrap();
        assert_eq!(deliver(&alice, &bob), 0, "the next change goes by the board");
        alice.sync_boards().await;
        bob.sync_boards().await;
        assert!(bob.load(&id).unwrap().settled_board().is_some());

        alice.with_space(&id, |s, _| {
            let (outs, e) = s.post(0, "one upload", None)?;
            Ok(((), outs, vec![e]))
        })
        .unwrap();
        assert_eq!(alice.hub().outbox.len(), 0, "nothing by mail");
        assert_eq!(alice.hub().board_out.len(), 1);
        alice.sync_boards().await;
        assert!(alice.hub().board_out.is_empty());
        assert!(bob.sync_boards().await >= 1);
        assert_eq!(said(&bob, &id), ["one upload"]);
        assert_eq!(mailbox.lock().unwrap().stats().boards, 1);

        // Carol joins: the commit and her Welcome go on the board, and she
        // is told by mail where it starts.
        invite_in(&alice, &id, &carol, "Carol");
        assert!(alice.hub().outbox.is_empty(), "the Welcome waits for the board");
        alice.sync_boards().await;
        assert_eq!(deliver(&alice, &carol), 1, "the pointer");
        carol.sync_boards().await;
        assert!(carol.load(&id).unwrap().joining.is_none(), "Carol is in");
        bob.sync_boards().await;
        let (a, b, c) = (alice.load(&id).unwrap(), bob.load(&id).unwrap(), carol.load(&id).unwrap());
        assert_eq!(b.readers().len(), 3, "Bob took the commit from the board");
        assert_eq!(b.signed(), a.signed(), "and its manifest change");
        assert_eq!((b.epoch(), c.epoch()), (a.epoch(), a.epoch()));

        // A new epoch: the node lets its token in once someone proves an
        // older one, and everyone reads on.
        carol.with_space(&id, |s, _| {
            let (outs, e) = s.post(0, "hi from Carol", None)?;
            Ok(((), outs, vec![e]))
        })
        .unwrap();
        carol.sync_boards().await;
        alice.sync_boards().await;
        bob.sync_boards().await;
        assert_eq!(said(&alice, &id), ["one upload", "hi from Carol"]);
        assert_eq!(said(&bob, &id), ["one upload", "hi from Carol"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
