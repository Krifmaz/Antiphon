//! Group chats and communities (spec §9 "Group chats up to ~100, no
//! server required", "Communities with channels, roles, permissions",
//! "per-community ban lists"): who is in one, and who may change that.
//!
//! There is no server to ask, so a community *is* its **manifest**: a
//! versioned document -- name, channels, roles, members, ban list, owner
//! keys, the nodes that serve it -- signed by whoever made each version.
//! Every member keeps the current one and accepts the next only if
//! [`check_transition`] passes: the signer had, *under the version being
//! replaced*, the permission for every change in it. A ban, a kick, a new
//! channel, a role given out are all such versions, so every moderation
//! action is a signature every client checks, and a member who isn't a
//! moderator can produce a "ban" but nobody will accept it.
//!
//! The manifest says who *should* be in the group. The MLS group (see
//! [`crate::group`]) is who *can read it*; the app keeps the two in step
//! and refuses MLS commits that add someone the manifest doesn't list.
//!
//! # Ranks
//!
//! Owners outrank every role; roles are listed most powerful first. You
//! can only kick, ban or re-role someone you strictly outrank, and only
//! hand out roles below your own -- so a moderator cannot ban an admin,
//! nor make themselves one. Owners cannot be removed by anyone but
//! themselves.
//!
//! # Trust on first use, stated
//!
//! A newcomer is handed the *current* manifest by whoever invited them,
//! not the whole history, and accepts it if its signer had authority
//! under it. Everyone already in the community holds the chain and checks
//! each step. So a newcomer trusts their inviter about the community's
//! present state the way they trust a contact's first key -- which is no
//! more than accepting the invite already meant.
//!
//! # Concurrent edits
//!
//! Two admins can each sign a version N+1. The one whose signed bytes
//! hash lower wins everywhere ([`SignedManifest::beats`]); the other's
//! change is dropped and the app tells its author to make it again.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::identity::{verify_signature, IdentityKeyPair};

const SIGN_CONTEXT: &[u8] = b"antiphon/community/manifest/v1";

/// Spec §9: "Group chats up to ~100". Every message is sealed once per
/// recipient's mailbox, so this is also the sender's fan-out.
pub const MAX_GROUP_MEMBERS: usize = 100;
/// Communities are bigger, short of spec §11's deferred "full scale".
pub const MAX_COMMUNITY_MEMBERS: usize = 250;
pub const MAX_CHANNELS: usize = 50;
pub const MAX_ROLES: usize = 16;
pub const MAX_BANS: usize = 1000;
const MAX_NAME: usize = 80;
/// Custom emoji a community may have. Few, because the whole set travels
/// with every new version of the manifest.
pub const MAX_EMOJI: usize = 16;
/// The longest a custom emoji's picture may be, as base64: a small image
/// (the apps make them 32 pixels square, about a kilobyte).
pub const MAX_EMOJI_IMAGE: usize = 1800;
/// The longest a custom emoji's name may be.
pub const MAX_EMOJI_NAME: usize = 24;
const MAX_TEXT: usize = 500;

/// Permission bits a role carries.
pub mod perm {
    /// Rename, describe, set the nodes and the server-hosted toggle.
    pub const MANAGE_COMMUNITY: u32 = 1;
    pub const MANAGE_CHANNELS: u32 = 1 << 1;
    /// Give members roles below your own.
    pub const MANAGE_ROLES: u32 = 1 << 2;
    pub const INVITE: u32 = 1 << 3;
    pub const KICK: u32 = 1 << 4;
    pub const BAN: u32 = 1 << 5;
    /// Remove other people's messages.
    pub const MANAGE_MESSAGES: u32 = 1 << 6;
    pub const POST: u32 = 1 << 7;
    pub const ALL: u32 = (1 << 8) - 1;
}

/// An identity key, written as hex in the manifest's JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Key(pub [u8; 32]);

impl Serialize for Key {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        let mut out = [0u8; 32];
        if text.len() != 64 {
            return Err(serde::de::Error::custom("a key is 64 hex digits"));
        }
        for (i, o) in out.iter_mut().enumerate() {
            *o = text
                .get(i * 2..i * 2 + 2)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| serde::de::Error::custom("not hex"))?;
        }
        Ok(Key(out))
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A group chat: one conversation, everyone may invite.
    Group,
    Community,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    pub perms: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub key: Key,
    /// Index into [`Manifest::roles`]. Ignored for owners.
    pub role: u16,
    /// What the person who added them called them; members' own cards
    /// carry the name they chose.
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Channel {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub topic: String,
    /// A private channel: only these members (and owners) may read or
    /// post in it. `None` is open to every member. Members' apps send a
    /// private channel's messages only to its readers, each sealed to them,
    /// so nobody else ever receives them. Who the readers are is in the
    /// manifest, which every member can read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readers: Option<Vec<Key>>,
    /// A voice channel: a standing group call members drop in and out of,
    /// rather than text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub voice: bool,
}

/// A community's own emoji (spec §13 "custom emoji"): a short name,
/// written `:name:` in a message, and a small picture. Only someone who
/// may manage the community adds or removes them, so what members are
/// shown is chosen by the people they agreed to be governed by when they
/// joined, never by whoever posts.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Emoji {
    /// Lowercase letters, digits and underscores, 2 to [`MAX_EMOJI_NAME`].
    pub name: String,
    /// The picture (PNG or JPEG), base64.
    pub image: String,
}

impl Emoji {
    /// Whether `name` is usable as an emoji name.
    pub fn valid_name(name: &str) -> bool {
        (2..=MAX_EMOJI_NAME).contains(&name.len()) && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Ban {
    pub key: Key,
    pub reason: String,
    pub by: Key,
    pub at_secs: u64,
}

/// A node the community runs, and the members code it asks for.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct NodeRef {
    pub addr: String,
    #[serde(default)]
    pub code: String,
}

/// The community's own infrastructure (spec §4, §5, §8): mailbox, file
/// and DHT nodes, joined with the members code they were started with
/// (`antiphon-relay --mailbox-code` / `--files-code`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Nodes {
    #[serde(default)]
    pub mailbox: Vec<NodeRef>,
    #[serde(default)]
    pub files: Vec<NodeRef>,
    #[serde(default)]
    pub dht: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Random, fixed at creation.
    pub id: Key,
    pub version: u64,
    /// Hash of the signed version this one replaces; zero for the first.
    pub prev: Key,
    pub kind: Kind,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub owners: Vec<Key>,
    /// Most powerful first. New members get the last one.
    pub roles: Vec<Role>,
    pub members: Vec<Member>,
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub bans: Vec<Ban>,
    #[serde(default)]
    pub nodes: Nodes,
    /// Spec §5 "Server-hosted mode -- per-community toggle, off by
    /// default": members pin files shared here to `nodes.files`.
    #[serde(default)]
    pub server_hosted_files: bool,
    /// Its custom emoji. Left out of the JSON while empty, so a manifest
    /// without any reads exactly as one from before they existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emoji: Vec<Emoji>,
    /// Slow mode: the fewest seconds between two posts from one member;
    /// 0 is off. Every client holds its own posts back and drops others'
    /// that come too soon (`antiphon_client::community`). Members who may
    /// remove messages aren't held to it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub slow_mode_secs: u32,
    /// Open to strangers: anyone with its join link (as a directory lists
    /// it) can ask to join, and a member who may invite lets them in
    /// without being asked. Off, only invitations bring people in.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub listed: bool,
}

/// The longest slow mode a community can set: six hours.
pub const MAX_SLOW_MODE_SECS: u32 = 6 * 60 * 60;

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Why a manifest, or a change to one, was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Denied {
    #[error("the signature does not check out")]
    BadSignature,
    #[error("the manifest is malformed: {0}")]
    Malformed(&'static str),
    #[error("this is a different community")]
    WrongCommunity,
    #[error("not the next version")]
    NotNext,
    #[error("the signer is not a member")]
    NotAMember,
    #[error("the signer may not {0}")]
    NotAllowed(&'static str),
}

/// A manifest and the signature of whoever made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedManifest {
    /// The manifest's JSON, exactly as signed.
    pub body: Vec<u8>,
    pub signer: [u8; 32],
    pub signature: [u8; 64],
}

fn signed_bytes(body: &[u8]) -> Vec<u8> {
    let mut m = SIGN_CONTEXT.to_vec();
    m.extend_from_slice(body);
    m
}

impl SignedManifest {
    pub fn sign(manifest: &Manifest, signer: &IdentityKeyPair) -> Self {
        let body = serde_json::to_vec(manifest).expect("a manifest always serializes");
        let signature = signer.sign(&signed_bytes(&body));
        Self { body, signer: signer.public().identity_key, signature }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.signer.to_vec();
        out.extend_from_slice(&self.signature);
        out.extend_from_slice(&self.body);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Denied> {
        if bytes.len() < 96 {
            return Err(Denied::Malformed("too short"));
        }
        Ok(Self {
            signer: bytes[..32].try_into().expect("32"),
            signature: bytes[32..96].try_into().expect("64"),
            body: bytes[96..].to_vec(),
        })
    }

    /// What the next version's `prev` names.
    pub fn hash(&self) -> Key {
        Key(Sha256::digest(self.encode()).into())
    }

    /// Of two different signed versions with the same number, whether
    /// `self` is the one everyone keeps.
    pub fn beats(&self, other: &SignedManifest) -> bool {
        self.hash() < other.hash()
    }

    /// The manifest, if the signature holds and it is well formed. Says
    /// nothing about whether the signer was *allowed* to make it; see
    /// [`check_transition`] and [`accept_first`].
    pub fn open(&self) -> Result<Manifest, Denied> {
        verify_signature(&self.signer, &signed_bytes(&self.body), &self.signature).map_err(|_| Denied::BadSignature)?;
        let m: Manifest = serde_json::from_slice(&self.body).map_err(|_| Denied::Malformed("not a manifest"))?;
        m.validate()?;
        Ok(m)
    }
}

impl Manifest {
    /// A new group chat or community with `creator` as its owner.
    pub fn genesis(creator: &IdentityKeyPair, kind: Kind, name: &str, creator_name: &str) -> Self {
        let mut id = [0u8; 32];
        getrandom::fill(&mut id).expect("OS randomness is unavailable");
        let roles = match kind {
            // A group chat is a conversation among equals: anyone may
            // bring someone in; removing people is for its owners.
            Kind::Group => vec![Role { name: "Member".into(), perms: perm::INVITE | perm::POST }],
            Kind::Community => vec![
                Role { name: "Admin".into(), perms: perm::ALL },
                Role {
                    name: "Moderator".into(),
                    perms: perm::INVITE | perm::KICK | perm::BAN | perm::MANAGE_MESSAGES | perm::POST,
                },
                Role { name: "Member".into(), perms: perm::POST },
            ],
        };
        let me = Key(creator.public().identity_key);
        let default_role = roles.len() as u16 - 1;
        Self {
            id: Key(id),
            version: 0,
            prev: Key::default(),
            kind,
            name: clip(name, MAX_NAME),
            description: String::new(),
            owners: vec![me],
            roles,
            members: vec![Member { key: me, role: default_role, name: clip(creator_name, MAX_NAME) }],
            channels: vec![Channel { id: 0, name: if kind == Kind::Group { "chat".into() } else { "general".into() }, ..Channel::default() }],
            bans: Vec::new(),
            nodes: Nodes::default(),
            server_hosted_files: false,
            emoji: Vec::new(),
            slow_mode_secs: 0,
            listed: false,
        }
    }

    /// The starting point for the version after `signed` (this manifest).
    pub fn next(&self, signed: &SignedManifest) -> Self {
        let mut n = self.clone();
        n.version += 1;
        n.prev = signed.hash();
        n
    }

    pub fn max_members(&self) -> usize {
        match self.kind {
            Kind::Group => MAX_GROUP_MEMBERS,
            Kind::Community => MAX_COMMUNITY_MEMBERS,
        }
    }

    pub fn default_role(&self) -> u16 {
        self.roles.len().saturating_sub(1) as u16
    }

    pub fn member(&self, key: &[u8; 32]) -> Option<&Member> {
        self.members.iter().find(|m| m.key.0 == *key)
    }

    pub fn is_member(&self, key: &[u8; 32]) -> bool {
        self.member(key).is_some()
    }

    pub fn is_owner(&self, key: &[u8; 32]) -> bool {
        self.owners.iter().any(|o| o.0 == *key)
    }

    pub fn is_banned(&self, key: &[u8; 32]) -> bool {
        self.bans.iter().any(|b| b.key.0 == *key)
    }

    /// 0 for owners, then 1 + role index; lower outranks higher. Someone
    /// not in the community ranks below everyone.
    pub fn rank(&self, key: &[u8; 32]) -> u32 {
        if self.is_owner(key) {
            return 0;
        }
        self.member(key).map(|m| m.role as u32 + 1).unwrap_or(u32::MAX)
    }

    /// What this person may do here.
    pub fn perms(&self, key: &[u8; 32]) -> u32 {
        if self.is_owner(key) {
            return perm::ALL;
        }
        self.member(key).and_then(|m| self.roles.get(m.role as usize)).map(|r| r.perms).unwrap_or(0)
    }

    /// The channel with this number.
    pub fn channel(&self, id: u32) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    /// May `key` read (and, with Post, write in) channel `id`: every member
    /// for an open channel; its readers and the owners for a private one.
    pub fn can_read(&self, id: u32, key: &[u8; 32]) -> bool {
        let Some(c) = self.channel(id) else { return false };
        self.is_member(key) && c.readers.as_ref().is_none_or(|r| self.is_owner(key) || r.iter().any(|k| k.0 == *key))
    }

    /// Adds a role at rank `at` (0 is the most powerful), keeping everyone's
    /// role where it was. The newcomers' role stays last, so `at` is capped
    /// to sit above it.
    pub fn insert_role(&mut self, at: usize, role: Role) {
        let at = at.min(self.roles.len().saturating_sub(1));
        self.roles.insert(at, role);
        for m in &mut self.members {
            if m.role as usize >= at {
                m.role += 1;
            }
        }
    }

    /// Removes the role at `at`; whoever held it gets the newcomers' role.
    /// The newcomers' role itself (the last) can't be removed.
    pub fn remove_role(&mut self, at: usize) -> bool {
        if at + 1 >= self.roles.len() {
            return false;
        }
        self.roles.remove(at);
        let default = self.default_role();
        for m in &mut self.members {
            match (m.role as usize).cmp(&at) {
                std::cmp::Ordering::Equal => m.role = default,
                std::cmp::Ordering::Greater => m.role -= 1,
                std::cmp::Ordering::Less => {}
            }
        }
        true
    }

    /// May `actor` use `permission` on `target` -- holding it and, when
    /// there is a target, strictly outranking them.
    pub fn can(&self, actor: &[u8; 32], permission: u32, target: Option<&[u8; 32]>) -> bool {
        self.perms(actor) & permission == permission && target.is_none_or(|t| self.rank(actor) < self.rank(t))
    }

    fn validate(&self) -> Result<(), Denied> {
        let bad = |why| Err(Denied::Malformed(why));
        if self.name.chars().count() > MAX_NAME || self.description.chars().count() > MAX_TEXT {
            return bad("a name is too long");
        }
        if self.roles.is_empty() || self.roles.len() > MAX_ROLES || self.roles.iter().any(|r| r.name.chars().count() > MAX_NAME) {
            return bad("roles");
        }
        if self.members.len() > self.max_members() {
            return bad("too many members");
        }
        if self.channels.is_empty() || self.channels.len() > MAX_CHANNELS || self.bans.len() > MAX_BANS {
            return bad("too many channels or bans");
        }
        let mut keys: Vec<_> = self.members.iter().map(|m| m.key).collect();
        keys.sort();
        keys.dedup();
        if keys.len() != self.members.len() {
            return bad("a member is listed twice");
        }
        if self.members.iter().any(|m| m.role as usize >= self.roles.len() || m.name.chars().count() > MAX_NAME) {
            return bad("a member's role or name");
        }
        if self.owners.is_empty() || self.owners.iter().any(|o| !self.is_member(&o.0)) {
            return bad("every owner must be a member");
        }
        if self.members.iter().any(|m| self.is_banned(&m.key.0)) {
            return bad("a banned person is still a member");
        }
        if self.emoji.len() > MAX_EMOJI || self.emoji.iter().any(|e| !Emoji::valid_name(&e.name) || e.image.is_empty() || e.image.len() > MAX_EMOJI_IMAGE) {
            return bad("emoji");
        }
        let mut names: Vec<_> = self.emoji.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        names.dedup();
        if names.len() != self.emoji.len() {
            return bad("an emoji name is used twice");
        }
        let mut ids: Vec<_> = self.channels.iter().map(|c| c.id).collect();
        ids.sort();
        ids.dedup();
        if ids.len() != self.channels.len() || self.channels.iter().any(|c| c.name.chars().count() > MAX_NAME) {
            return bad("channels");
        }
        if self.channels.iter().any(|c| c.readers.as_ref().is_some_and(|r| r.len() > self.max_members())) {
            return bad("a private channel lists too many readers");
        }
        if self.slow_mode_secs > MAX_SLOW_MODE_SECS {
            return bad("slow mode is too slow");
        }
        Ok(())
    }
}

fn clip(s: &str, n: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(n).collect()
}

/// A first manifest for a newcomer (see the module's note on trust): its
/// signature holds, and its signer could have made it -- the owner of a
/// version 0; later, a member with some management permission, or
/// someone signing themselves out.
pub fn accept_first(signed: &SignedManifest) -> Result<Manifest, Denied> {
    let m = signed.open()?;
    let signer = &signed.signer;
    if m.version == 0 {
        if m.prev != Key::default() || !m.is_owner(signer) {
            return Err(Denied::NotAllowed("create a community they do not own"));
        }
        return Ok(m);
    }
    let manages = perm::MANAGE_COMMUNITY | perm::MANAGE_ROLES | perm::INVITE | perm::KICK | perm::BAN | perm::MANAGE_CHANNELS;
    let left = !m.is_member(signer) && !m.is_banned(signer);
    if m.perms(signer) & manages == 0 && !left {
        return Err(Denied::NotAllowed("change the community"));
    }
    Ok(m)
}

/// Accepts `new` as the version after `old` (already accepted) if every
/// change in it was allowed to its signer under `old`.
pub fn check_transition(old: &Manifest, old_signed: &SignedManifest, new: &SignedManifest) -> Result<Manifest, Denied> {
    let n = new.open()?;
    if n.id != old.id {
        return Err(Denied::WrongCommunity);
    }
    if n.version != old.version + 1 || n.prev != old_signed.hash() {
        return Err(Denied::NotNext);
    }
    if n.kind != old.kind {
        return Err(Denied::NotAllowed("change what kind of group this is"));
    }
    let s = &new.signer;
    if !old.is_member(s) {
        return Err(Denied::NotAMember);
    }
    let need = |p: u32, what: &'static str| if old.perms(s) & p == p { Ok(()) } else { Err(Denied::NotAllowed(what)) };

    if n.name != old.name
        || n.description != old.description
        || n.nodes != old.nodes
        || n.server_hosted_files != old.server_hosted_files
        || n.slow_mode_secs != old.slow_mode_secs
        || n.listed != old.listed
    {
        need(perm::MANAGE_COMMUNITY, "change the community's settings")?;
    }
    if n.emoji != old.emoji {
        need(perm::MANAGE_COMMUNITY, "change the community's emoji")?;
    }
    if n.channels != old.channels {
        need(perm::MANAGE_CHANNELS, "change channels")?;
    }
    if n.roles != old.roles {
        // What a role may do is the whole permission system; only owners
        // redefine it.
        if !old.is_owner(s) {
            return Err(Denied::NotAllowed("change what roles can do"));
        }
    }
    if n.owners != old.owners {
        if !old.is_owner(s) {
            return Err(Denied::NotAllowed("change the owners"));
        }
        // An owner may step down, or name new owners, but not depose
        // another one.
        if old.owners.iter().any(|o| o.0 != *s && !n.owners.contains(o)) {
            return Err(Denied::NotAllowed("remove another owner"));
        }
    }

    // Who left, who was removed, who arrived.
    for m in &old.members {
        if n.is_member(&m.key.0) {
            continue;
        }
        if m.key.0 == *s {
            continue; // leaving is always allowed
        }
        let banned = n.is_banned(&m.key.0) && !old.is_banned(&m.key.0);
        let p = if banned { perm::BAN } else { perm::KICK };
        if !old.can(s, p, Some(&m.key.0)) {
            return Err(Denied::NotAllowed(if banned { "ban this person" } else { "remove this person" }));
        }
    }
    for m in &n.members {
        match old.member(&m.key.0) {
            None => {
                need(perm::INVITE, "invite people")?;
                if n.is_owner(&m.key.0) && !old.is_owner(s) {
                    return Err(Denied::NotAllowed("make someone an owner"));
                }
                if m.role != n.default_role() && !(old.can(s, perm::MANAGE_ROLES, None) && (old.rank(s) < m.role as u32 + 1)) {
                    return Err(Denied::NotAllowed("give a newcomer that role"));
                }
            }
            // Compared by the role itself, not its number: an owner adding or
            // removing a role renumbers the ones below it without anyone's
            // role changing.
            // An owner's role means nothing while they are one (they may do
            // everything), so it is not checked: otherwise an owner couldn't
            // rename a role another owner, or they themselves, hold.
            Some(before) if old.roles.get(before.role as usize) != n.roles.get(m.role as usize) && !old.is_owner(&m.key.0) => {
                // Re-roling: only someone you outrank, and only to a role
                // below your own.
                if !old.can(s, perm::MANAGE_ROLES, Some(&m.key.0)) || old.rank(s) > m.role as u32 {
                    return Err(Denied::NotAllowed("give that role"));
                }
            }
            Some(_) => {}
        }
    }
    for b in &n.bans {
        if !old.is_banned(&b.key.0) {
            need(perm::BAN, "ban people")?;
            if b.by.0 != *s {
                return Err(Denied::Malformed("a ban names someone else as its author"));
            }
            if !old.can(s, perm::BAN, Some(&b.key.0)) {
                return Err(Denied::NotAllowed("ban this person"));
            }
        }
    }
    for b in &old.bans {
        if !n.is_banned(&b.key.0) {
            need(perm::BAN, "lift a ban")?;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct World {
        owner: IdentityKeyPair,
        signed: SignedManifest,
        m: Manifest,
    }

    fn community() -> World {
        let owner = IdentityKeyPair::generate();
        let m = Manifest::genesis(&owner, Kind::Community, "Choir", "Olive");
        let signed = SignedManifest::sign(&m, &owner);
        World { owner, signed, m }
    }

    impl World {
        /// `by` signs `change` applied to the current version.
        fn propose(&self, by: &IdentityKeyPair, change: impl FnOnce(&mut Manifest)) -> SignedManifest {
            let mut n = self.m.next(&self.signed);
            change(&mut n);
            SignedManifest::sign(&n, by)
        }

        fn apply(&mut self, by: &IdentityKeyPair, change: impl FnOnce(&mut Manifest)) -> Result<(), Denied> {
            let s = self.propose(by, change);
            self.m = check_transition(&self.m, &self.signed, &s)?;
            self.signed = s;
            Ok(())
        }
    }

    #[test]
    fn a_private_channel_is_readable_only_by_its_readers_and_owners() {
        let mut w = community();
        let (a, b) = (IdentityKeyPair::generate(), IdentityKeyPair::generate());
        let owner = std::mem::replace(&mut w.owner, IdentityKeyPair::generate());
        let ak = Key(a.public().identity_key);
        w.apply(&owner, |m| {
            add(m, &a, 2);
            add(m, &b, 2);
            m.channels.push(Channel { id: 1, name: "staff".into(), readers: Some(vec![ak]), ..Channel::default() });
        })
        .unwrap();
        assert!(w.m.can_read(1, &a.public().identity_key));
        assert!(!w.m.can_read(1, &b.public().identity_key));
        assert!(w.m.can_read(1, &owner.public().identity_key));
        assert!(w.m.can_read(0, &b.public().identity_key));
        // Making a channel private is a channel change: a member can't.
        assert!(w.apply(&b, |m| m.channels[1].readers = None).is_err());
    }

    #[test]
    fn custom_roles_keep_everyone_where_they_were() {
        let mut w = community();
        let a = IdentityKeyPair::generate();
        let owner = std::mem::replace(&mut w.owner, IdentityKeyPair::generate());
        w.apply(&owner, |m| add(m, &a, 1)).unwrap(); // a Moderator
        w.apply(&owner, |m| m.insert_role(1, Role { name: "Host".into(), perms: perm::POST | perm::INVITE })).unwrap();
        assert_eq!(w.m.roles[w.m.member(&a.public().identity_key).unwrap().role as usize].name, "Moderator");
        assert_eq!(w.m.roles.last().unwrap().name, "Member", "the newcomers' role stays last");
        let mut gone = w.m.clone();
        assert!(gone.remove_role(2));
        assert_eq!(gone.roles[gone.member(&a.public().identity_key).unwrap().role as usize].name, "Member");
        assert!(!gone.remove_role(gone.roles.len() - 1), "the newcomers' role stays");
        // Only an owner redefines roles.
        assert!(w.apply(&a, |m| m.roles[0].perms = 0).is_err());
        // An owner may rename or rewrite any role, the ones owners and the
        // people below them hold included.
        let mine = w.m.member(&owner.public().identity_key).unwrap().role as usize;
        w.apply(&owner, |m| m.roles[mine].name = "Founders".into()).unwrap();
        w.apply(&owner, |m| m.roles[0].perms &= !perm::BAN).unwrap();
    }

    #[test]
    fn slow_mode_is_a_community_setting() {
        let mut w = community();
        let moder = IdentityKeyPair::generate();
        let owner = std::mem::replace(&mut w.owner, IdentityKeyPair::generate());
        w.apply(&owner, |m| add(m, &moder, 1)).unwrap();
        assert!(w.apply(&moder, |m| m.slow_mode_secs = 30).is_err(), "a moderator can't change the community's settings");
        w.apply(&owner, |m| m.slow_mode_secs = 30).unwrap();
        assert_eq!(w.m.slow_mode_secs, 30);
        assert!(w.apply(&owner, |m| m.slow_mode_secs = MAX_SLOW_MODE_SECS + 1).is_err());
    }

    fn add(m: &mut Manifest, who: &IdentityKeyPair, role: u16) {
        m.members.push(Member { key: Key(who.public().identity_key), role, name: "x".into() });
    }

    fn ban(m: &mut Manifest, who: &IdentityKeyPair, by: &IdentityKeyPair) {
        let k = Key(who.public().identity_key);
        m.members.retain(|x| x.key != k);
        m.bans.push(Ban { key: k, reason: "spam".into(), by: Key(by.public().identity_key), at_secs: 1 });
    }

    #[test]
    fn a_manifest_round_trips_and_a_tampered_one_is_refused() {
        let w = community();
        let decoded = SignedManifest::decode(&w.signed.encode()).unwrap();
        assert_eq!(decoded.open().unwrap(), w.m);
        assert_eq!(accept_first(&decoded).unwrap(), w.m);
        let mut bad = decoded.clone();
        let at = bad.body.iter().position(|b| *b == b'C').unwrap();
        bad.body[at] = b'K'; // "Choir" -> "Khoir"
        assert_eq!(bad.open(), Err(Denied::BadSignature));
        // A version 0 must be signed by its owner.
        let mallory = IdentityKeyPair::generate();
        assert!(accept_first(&SignedManifest::sign(&w.m, &mallory)).is_err());
    }

    #[test]
    fn a_member_cannot_ban_but_a_moderator_can_and_only_below_them() {
        let mut w = community();
        let owner = IdentityKeyPair::from_master_seed(w.owner.master_seed());
        let (admin, moder, alice, bob) =
            (IdentityKeyPair::generate(), IdentityKeyPair::generate(), IdentityKeyPair::generate(), IdentityKeyPair::generate());
        w.apply(&owner, |m| {
            add(m, &admin, 0);
            add(m, &moder, 1);
            add(m, &alice, 2);
            add(m, &bob, 2);
        })
        .unwrap();

        // A plain member's ban is refused by every client.
        let attempt = w.propose(&alice, |m| ban(m, &bob, &alice));
        assert!(matches!(check_transition(&w.m, &w.signed, &attempt), Err(Denied::NotAllowed(_))));
        // So is kicking.
        let attempt = w.propose(&alice, |m| m.members.retain(|x| x.key.0 != bob.public().identity_key));
        assert!(check_transition(&w.m, &w.signed, &attempt).is_err());
        // A moderator can't ban the admin above them.
        let attempt = w.propose(&moder, |m| ban(m, &admin, &moder));
        assert!(check_transition(&w.m, &w.signed, &attempt).is_err());
        // Nor promote themselves.
        let attempt = w.propose(&moder, |m| m.members.iter_mut().find(|x| x.key.0 == moder.public().identity_key).unwrap().role = 0);
        assert!(check_transition(&w.m, &w.signed, &attempt).is_err());
        // Nor forge a ban "by" the owner.
        let attempt = w.propose(&moder, |m| ban(m, &bob, &owner));
        assert!(check_transition(&w.m, &w.signed, &attempt).is_err());
        // But can ban a member.
        w.apply(&moder, |m| ban(m, &bob, &moder)).unwrap();
        assert!(w.m.is_banned(&bob.public().identity_key));
        assert!(!w.m.is_member(&bob.public().identity_key));
        // A banned person can't be invited back, even by the owner, while
        // the ban stands (the manifest itself is refused).
        let attempt = w.propose(&owner, |m| add(m, &bob, 2));
        assert!(check_transition(&w.m, &w.signed, &attempt).is_err());
        // The admin can't depose the owner; a member can leave.
        let attempt = w.propose(&admin, |m| ban(m, &owner, &admin));
        assert!(check_transition(&w.m, &w.signed, &attempt).is_err());
        w.apply(&alice, |m| m.members.retain(|x| x.key.0 != alice.public().identity_key)).unwrap();
        // Someone no longer a member signs nothing.
        let attempt = w.propose(&alice, |m| m.name = "Mine now".into());
        assert_eq!(check_transition(&w.m, &w.signed, &attempt), Err(Denied::NotAMember));
    }

    #[test]
    fn only_managers_change_the_emoji_and_bad_ones_are_refused() {
        let mut w = community();
        let owner = IdentityKeyPair::from_master_seed(w.owner.master_seed());
        let member = IdentityKeyPair::generate();
        w.apply(&owner, |m| add(m, &member, 2)).unwrap();
        let party = || Emoji { name: "party_cat".into(), image: "iVBORw0KGgo=".into() };
        assert!(check_transition(&w.m, &w.signed, &w.propose(&member, |m| m.emoji.push(party()))).is_err(), "a member can't add one");
        w.apply(&owner, |m| m.emoji.push(party())).unwrap();
        assert_eq!(w.m.emoji.len(), 1);
        assert!(check_transition(&w.m, &w.signed, &w.propose(&member, |m| m.emoji.clear())).is_err(), "nor remove one");
        // Names that could be confused with text, oversized pictures and
        // a name used twice are malformed for everyone.
        for bad in [
            Emoji { name: "Party Cat".into(), image: "x".into() },
            Emoji { name: "p".into(), image: "x".into() },
            Emoji { name: "big".into(), image: "x".repeat(MAX_EMOJI_IMAGE + 1) },
            party(),
        ] {
            assert!(w.apply(&owner, |m| m.emoji.push(bad.clone())).is_err(), "{:?} was let in", bad.name);
        }
    }

    #[test]
    fn a_manifest_without_emoji_reads_as_before() {
        let w = community();
        let json = String::from_utf8(w.signed.body.clone()).unwrap();
        assert!(!json.contains("emoji"), "an empty set isn't written, so older apps see the same manifest");
    }

    #[test]
    fn versions_must_chain_and_settings_need_their_permission() {
        let mut w = community();
        let owner = IdentityKeyPair::from_master_seed(w.owner.master_seed());
        let member = IdentityKeyPair::generate();
        w.apply(&owner, |m| add(m, &member, 2)).unwrap();
        // A member can't rename, add channels or redefine roles.
        assert!(check_transition(&w.m, &w.signed, &w.propose(&member, |m| m.name = "x".into())).is_err());
        assert!(check_transition(&w.m, &w.signed, &w.propose(&member, |m| m.channels.push(Channel { id: 9, name: "x".into(), ..Default::default() }))).is_err());
        assert!(check_transition(&w.m, &w.signed, &w.propose(&member, |m| m.roles[2].perms = perm::ALL)).is_err());
        assert!(check_transition(&w.m, &w.signed, &w.propose(&member, |m| m.server_hosted_files = true)).is_err());
        // The owner can.
        w.apply(&owner, |m| {
            m.channels.push(Channel { id: 1, name: "voice".into(), ..Default::default() });
            m.server_hosted_files = true;
            m.nodes.files.push(NodeRef { addr: "files.example:3479".into(), code: "choir".into() });
        })
        .unwrap();
        // Skipping or replaying a version is refused.
        let stale = SignedManifest::sign(&w.m, &owner);
        assert_eq!(check_transition(&w.m, &w.signed, &stale), Err(Denied::NotNext));
        let mut skip = w.m.next(&w.signed);
        skip.version += 1;
        assert_eq!(check_transition(&w.m, &w.signed, &SignedManifest::sign(&skip, &owner)), Err(Denied::NotNext));
        // Two competing versions: exactly one beats the other.
        let a = w.propose(&owner, |m| m.name = "A".into());
        let b = w.propose(&owner, |m| m.name = "B".into());
        assert_ne!(a.beats(&b), b.beats(&a));
    }

    #[test]
    fn a_group_chat_lets_anyone_invite_but_only_owners_remove() {
        let owner = IdentityKeyPair::generate();
        let m = Manifest::genesis(&owner, Kind::Group, "Friday", "Olive");
        let mut w = World { signed: SignedManifest::sign(&m, &owner), m, owner };
        let owner = IdentityKeyPair::from_master_seed(w.owner.master_seed());
        let (a, b, c) = (IdentityKeyPair::generate(), IdentityKeyPair::generate(), IdentityKeyPair::generate());
        w.apply(&owner, |m| add(m, &a, 0)).unwrap();
        w.apply(&a, |m| add(m, &b, 0)).unwrap();
        assert!(check_transition(&w.m, &w.signed, &w.propose(&a, |m| m.members.retain(|x| x.key.0 != b.public().identity_key))).is_err());
        w.apply(&owner, |m| m.members.retain(|x| x.key.0 != b.public().identity_key)).unwrap();
        // The cap holds.
        let mut big = w.m.next(&w.signed);
        for _ in 0..MAX_GROUP_MEMBERS {
            big.members.push(Member { key: Key(IdentityKeyPair::generate().public().identity_key), role: 0, name: String::new() });
        }
        assert!(check_transition(&w.m, &w.signed, &SignedManifest::sign(&big, &owner)).is_err());
        let _ = c;
    }
}
