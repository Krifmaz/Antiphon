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
//!   tell which channel is busy. The cost is that a private channel -- a
//!   subset of members -- would need a group of its own; that is not built.
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
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
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
}

impl Card {
    pub fn mine(identity: &IdentityKeyPair, routes_hex: String, name: &str) -> Self {
        Self {
            key: Key(identity.public().identity_key),
            public: hex(&identity.public().encode()),
            routes: routes_hex,
            name: name.chars().filter(|c| !c.is_control()).take(80).collect(),
        }
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
    /// "I have version `since`; send me what came after."
    ManifestRequest { community: Key, since: u64 },
    Manifests { community: Key, list: Vec<String> },
}

impl Wire {
    pub fn community(&self) -> Option<Key> {
        match self {
            Wire::Invite { .. } => None,
            Wire::Join { community, .. }
            | Wire::Welcome { community, .. }
            | Wire::Commit { community, .. }
            | Wire::App { community, .. }
            | Wire::ManifestRequest { community, .. }
            | Wire::Manifests { community, .. } => Some(*community),
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
    /// A file shared in a channel (spec §5), fetchable from its holders.
    File { id: u64, channel: u32, name: String, size: u64, file_id: String, key: String, holders: Vec<String>, at_secs: u64 },
    Card(Card),
    /// A new manifest version that changes nobody's membership.
    Manifest { manifest: String },
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
    File { channel: u32, id: u64, sender: [u8; 32], name: String, size: u64, file_id: String, key: String, holders: Vec<String>, at_secs: u64 },
    Joined,
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
}

impl Space {
    /// A new group chat or community, with us as its owner.
    pub fn create(identity: &IdentityKeyPair, kind: Kind, name: &str, card: Card) -> Result<Self, String> {
        let m = Manifest::genesis(identity, kind, name, &card.name);
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
        })
    }

    /// Accepting an invite: the space as far as we know it, and the Join
    /// for the inviter.
    pub fn accept(identity: &IdentityKeyPair, invite: &Invite, card: Card) -> Result<(Self, Out), String> {
        let signed = SignedManifest::decode(&unb64(&invite.manifest)?).map_err(|e| e.to_string())?;
        let m = cm::accept_first(&signed).map_err(|e| e.to_string())?;
        let me = identity.public().identity_key;
        if !m.is_member(&me) {
            return Err("that invite doesn't list you".into());
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
        };
        serde_json::to_vec(&saved).expect("a space always serializes")
    }

    pub fn restore(identity: &IdentityKeyPair, bytes: &[u8]) -> Result<Self, String> {
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
        Ok(Self {
            me: identity.public().identity_key,
            chain,
            mls: GroupMember::restore(identity, &unb64(&saved.mls)?).map_err(|e| e.to_string())?,
            cards: saved.cards,
            held: saved.held,
            previous: saved.previous,
            joining: saved.joining.map(|k| k.0),
            removed: saved.removed,
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

    fn learn(&mut self, card: Card) {
        if !card.valid() {
            return;
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

    /// A manifest change: tell everyone, then bring the group in line.
    fn publish(&mut self, signed: &SignedManifest) -> Result<Vec<Out>, String> {
        let mut outs = self.send(&Post::Manifest { manifest: b64(&signed.encode()) })?;
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
        let state = b64(&self.mls.save());
        let epoch = self.mls.epoch().map_err(|e| e.to_string())?;
        let c = self.mls.remove(keys).map_err(|e| e.to_string())?;
        self.previous = Some(Previous {
            epoch,
            mls: state,
            commit: sha(&c.commit),
            redo: Some(Redo::Remove { keys: keys.iter().map(|k| Key(*k)).collect() }),
        });
        let wire = Wire::Commit { community: self.id(), commit: b64(&c.commit), manifest: b64(&self.signed().encode()), cards: Vec::new() };
        Ok(before.into_iter().map(|to| Out { to, wire: wire.clone() }).collect())
    }

    fn commit_add(&mut self, key_package: &[u8], card: Card) -> Result<Vec<Out>, String> {
        let before = self.others_in_group();
        let state = b64(&self.mls.save());
        let epoch = self.mls.epoch().map_err(|e| e.to_string())?;
        let c = self.mls.add(&[key_package]).map_err(|e| e.to_string())?;
        self.previous = Some(Previous {
            epoch,
            mls: state,
            commit: sha(&c.commit),
            redo: Some(Redo::Add { key_package: b64(key_package), card: card.clone() }),
        });
        let manifest = b64(&self.signed().encode());
        let id = self.id();
        let mut outs: Vec<Out> = before
            .into_iter()
            .filter(|k| *k != card.key.0)
            .map(|to| Out { to, wire: Wire::Commit { community: id, commit: b64(&c.commit), manifest: manifest.clone(), cards: vec![card.clone()] } })
            .collect();
        if let Some(welcome) = c.welcome {
            outs.push(Out {
                to: card.key.0,
                wire: Wire::Welcome { community: id, welcome: b64(&welcome), manifest, cards: self.cards.clone() },
            });
        }
        Ok(outs)
    }

    // ---- what a member does ----

    /// Lists someone as a member (if they aren't already) and sends them
    /// an invite. They are added to the group when they accept.
    pub fn invite(&mut self, identity: &IdentityKeyPair, who: [u8; 32], name: &str) -> Result<Vec<Out>, String> {
        let mut outs = Vec::new();
        if !self.manifest().is_member(&who) {
            let signed = self.change(identity, |m| {
                let role = m.default_role();
                m.members.push(cm::Member { key: Key(who), role, name: name.chars().take(80).collect() });
            })?;
            outs.extend(self.send(&Post::Manifest { manifest: b64(&signed.encode()) })?);
        } else if self.readers().contains(&who) {
            return Err("they're already in it".into());
        }
        let card = self.card(&self.me).cloned().ok_or("no card of your own")?;
        outs.push(Out { to: who, wire: Wire::Invite { manifest: b64(&self.signed().encode()), card } });
        Ok(outs)
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
        let id = random_id();
        let at_secs = now_secs();
        let outs = self.send(&Post::Message { id, channel, text: text.to_string(), reply_to, at_secs })?;
        Ok((outs, Event::Message { channel, id, sender: self.me, text: text.to_string(), reply_to, at_secs }))
    }

    pub fn edit(&mut self, id: u64, text: &str) -> Result<Vec<Out>, String> {
        if text.trim().is_empty() {
            return Err("an edit can't be empty; delete it instead".into());
        }
        self.send(&Post::Edit { id, text: text.trim().to_string() })
    }

    /// Withdraws our own message, or removes someone else's if we may.
    pub fn delete(&mut self, id: u64, author: [u8; 32]) -> Result<Vec<Out>, String> {
        if author != self.me && !self.manifest().can(&self.me, perm::MANAGE_MESSAGES, Some(&author)) {
            return Err("you can't remove their messages".into());
        }
        self.send(&Post::Delete { id, author: Key(author) })
    }

    pub fn share_file(&mut self, post: Post) -> Result<Vec<Out>, String> {
        self.send(&post)
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
        match self.handle_inner(identity, from, wire, &mut h) {
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
            Wire::Invite { .. } => Ok(()),
            Wire::Join { key_package, card, .. } => {
                let m = self.manifest();
                if !m.is_member(&from) || card.key.0 != from || !card.valid() {
                    return Err("a join from someone not invited".into());
                }
                if self.readers().contains(&from) || !self.mls.in_group() {
                    return Ok(());
                }
                let kp = unb64(&key_package)?;
                if self.mls.key_package_identity(&kp).map_err(|e| e.to_string())? != from {
                    return Err("a join with someone else's key".into());
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
                for c in cards {
                    if self.readers().contains(&c.key.0) {
                        self.learn(c);
                    }
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
        let signed = SignedManifest::decode(&unb64(manifest)?).map_err(|e| e.to_string())?;
        let head = self.manifest().version;
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
        let m = self.manifest();
        // A manifest that removes its own signer is still theirs to send.
        if !m.is_member(&sender) && !matches!(post, Post::Manifest { .. }) {
            return Ok(());
        }
        match post {
            Post::Message { id, channel, text, reply_to, at_secs } => {
                if m.can(&sender, perm::POST, None) {
                    let channel = if m.channels.iter().any(|c| c.id == channel) { channel } else { m.channels[0].id };
                    let text: String = text.chars().take(4000).collect();
                    h.events.push(Event::Message { channel, id, sender, text, reply_to, at_secs });
                }
            }
            Post::Edit { id, text } => h.events.push(Event::Edited { id, sender, text: text.chars().take(4000).collect() }),
            Post::Delete { id, author } => {
                if author.0 == sender || m.can(&sender, perm::MANAGE_MESSAGES, Some(&author.0)) {
                    h.events.push(Event::Deleted { id, author: author.0 });
                }
            }
            Post::File { id, channel, name, size, file_id, key, holders, at_secs } => {
                if m.can(&sender, perm::POST, None) {
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
        }
        Ok(())
    }
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
const MAX_FRAGMENTS: usize = 400;
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

/// One fragment waiting to leave.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Queued {
    to: Key,
    id: u64,
    fragment: String,
    #[serde(default)]
    attempts: u32,
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

    fn queue(&self, hub: &mut Hub, outs: Vec<Out>) {
        for out in outs {
            if out.to == self.identity.public().identity_key {
                continue;
            }
            for fragment in fragments(&out.wire) {
                if hub.outbox.len() < MAX_OUTBOX {
                    hub.outbox.push(Queued { to: Key(out.to), id: random_id(), fragment, attempts: 0 });
                }
            }
        }
    }

    /// Records what a space reported: messages into channel history,
    /// edits and deletes checked against who wrote what.
    fn record(&self, space: &Space, events: &[Event], hub: &mut Hub) {
        let me = self.identity.public().identity_key;
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
                        // Never is a mute: counted, but no notice.
                        if hub.ring.get(&hex(&id.0)).copied() != Some(crate::notify::RingRule::Never.to_i32()) {
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
                }),
                Event::Removed => crate::delivery::notice(format!("You're no longer in {}.", space.manifest().name)),
                Event::Notice(n) => crate::delivery::notice(format!("{}: {n}", space.manifest().name)),
                Event::Joined => crate::delivery::notice(format!("You joined {}.", space.manifest().name)),
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
        let (value, outs, events) = f(&mut space, &self.identity)?;
        self.save(&space).map_err(|e| e.to_string())?;
        let mut hub = self.hub();
        self.record(&space, &events, &mut hub);
        self.queue(&mut hub, outs);
        self.save_hub(&hub);
        drop(_guard);
        bump();
        crate::delivery::kick();
        Ok(value)
    }

    pub fn create(&self, kind: Kind, name: &str, card: Card) -> Result<Key, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Give it a name first.".into());
        }
        let space = Space::create(&self.identity, kind, name, card)?;
        let _guard = LOCK.lock().unwrap();
        self.save(&space).map_err(|e| e.to_string())?;
        bump();
        Ok(space.id())
    }

    pub fn invites(&self) -> Vec<Invite> {
        self.hub().invites
    }

    pub fn accept_invite(&self, index: usize, card: Card) -> Result<Key, String> {
        let _guard = LOCK.lock().unwrap();
        let mut hub = self.hub();
        if index >= hub.invites.len() {
            return Err("that invite is gone".into());
        }
        let invite = hub.invites.remove(index);
        let (space, out) = Space::accept(&self.identity, &invite, card)?;
        if self.load(&space.id()).is_some_and(|s| !s.removed) {
            self.save_hub(&hub);
            return Err("you're already in it".into());
        }
        self.save(&space).map_err(|e| e.to_string())?;
        self.queue(&mut hub, vec![out]);
        self.save_hub(&hub);
        drop(_guard);
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

    pub fn unread(&self, id: &Key) -> u32 {
        self.hub().unread.get(&hex(&id.0)).copied().unwrap_or(0)
    }

    /// One fragment that arrived from `from`. Returns whether it was
    /// taken (so its mailbox copy may be deleted).
    pub fn receive_fragment(&self, from: [u8; 32], fragment: &str) -> bool {
        let wire = {
            let _guard = LOCK.lock().unwrap();
            let mut hub = self.hub();
            hub.reassembly.expire(now_secs());
            let wire = hub.reassembly.feed(from, fragment);
            self.save_hub(&hub);
            wire
        };
        if let Some(wire) = wire {
            self.receive(from, wire);
        }
        true
    }

    /// A whole message from `from`.
    pub fn receive(&self, from: [u8; 32], wire: Wire) {
        match wire {
            Wire::Invite { manifest, card } => self.receive_invite(from, manifest, card),
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
        let known = crate::config::load().contacts.iter().any(|c| c.identity_key.eq_ignore_ascii_case(&hex(&from)))
            || self.ids().iter().filter_map(|id| self.load(id)).any(|s| s.manifest().is_member(&from));
        if !known || card.key.0 != from || !card.valid() {
            return;
        }
        let Some(signed) = unb64(&manifest).ok().and_then(|b| SignedManifest::decode(&b).ok()) else { return };
        let Ok(m) = cm::accept_first(&signed) else { return };
        if !m.is_member(&self.identity.public().identity_key) {
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
        None
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
        for q in &queued {
            if unreachable.contains(&q.to) {
                continue;
            }
            if let Some(live) = live(&q.to.0) {
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
            let body = crate::delivery::MailBody { id: q.id, text: String::new(), at_secs: now_secs(), group: Some(q.fragment.clone()) };
            match crate::delivery::send(&self.identity, &q.to.0, &prekey, &record, &body, fan_out).await {
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
}

/// Our route list changed: tell each space's members where to reach us.
pub fn refresh_cards(card: Card) {
    let Some(store) = Store::app() else { return };
    for id in store.ids() {
        let _ = store.with_space(&id, |space, _| {
            if space.removed || space.joining.is_some() {
                return Ok(((), Vec::new(), Vec::new()));
            }
            Ok(((), space.refresh_card(card.clone())?, Vec::new()))
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

/// For the seeding holder (spec §5 "this community"): is this identity a
/// member of that community right now, by our copy of its manifest?
pub fn is_member(community: &[u8; 32], key: &[u8; 32]) -> bool {
    let Some(store) = Store::app() else { return false };
    store.load(&Key(*community)).is_some_and(|s| !s.removed && s.manifest().is_member(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Person {
        id: IdentityKeyPair,
        space: Option<Space>,
        name: &'static str,
    }

    fn person(name: &'static str) -> Person {
        Person { id: IdentityKeyPair::generate(), space: None, name }
    }

    impl Person {
        fn key(&self) -> [u8; 32] {
            self.id.public().identity_key
        }
        fn card(&self) -> Card {
            Card::mine(&self.id, String::new(), self.name)
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
    #[allow(clippy::type_complexity)]
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
                let Some(to) = people.iter().position(|p| p.key() == out.to) else { continue };
                let from_key = people[from].key();
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
                    let (space, join) = Space::accept(&p.id, &invite, p.card()).unwrap();
                    p.space = Some(space);
                    next.push((to, join));
                    continue;
                }
                let p = &mut people[to];
                let id = IdentityKeyPair::from_master_seed(p.id.master_seed());
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
        alice.space = Some(Space::create(&alice.id, Kind::Community, "Choir", alice.card()).unwrap());
        let id = IdentityKeyPair::from_master_seed(alice.id.master_seed());
        let outs = alice.space().invite(&id, bob.key(), "Bob").unwrap();
        run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        let outs = alice.space().invite(&id, carol.key(), "Carol").unwrap();
        let ev = run(&mut [&mut alice, &mut bob, &mut carol], outs.into_iter().map(|o| (0, o)).collect());
        assert!(ev[2].contains(&Event::Joined));
        for p in [&alice, &bob, &carol] {
            let mut r = p.space.as_ref().unwrap().readers();
            r.sort();
            let mut want = vec![alice.key(), bob.key(), carol.key()];
            want.sort();
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

    #[test]
    fn fragments_fit_a_mailbox_and_reassemble_in_any_order() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let wire = Wire::App { community: Key([1; 32]), data: b64(&vec![7u8; 3000]) };
        let mut frags = fragments(&wire);
        assert!(frags.len() > 1);
        // Each, sealed in a mail body, fits a mailbox blob.
        for f in &frags {
            let body = crate::delivery::MailBody { id: u64::MAX, text: String::new(), at_secs: u64::MAX, group: Some(f.clone()) };
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
        alice.space = Some(Space::create(&alice.id, Kind::Group, "Friday", a_card).unwrap());
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
                        let body = crate::delivery::MailBody { id: random_id(), text: String::new(), at_secs: 1, group: Some(f) };
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
                            let (space, join) = Space::accept(&p.id, &invite, p.card_with_routes(own)).unwrap();
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
}
