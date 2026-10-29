//! Group chats and communities for the desktop's engine: what the screens
//! show of them, and carrying out what the person does in them.
//!
//! Everything about a space -- its signed manifest, its MLS group, the
//! queue of what is waiting to go out, the history of each channel -- is
//! `antiphon_client::community`'s, sealed on disk. Reading all of that is
//! too slow to do every frame, so the screens' copy is rebuilt only when
//! the community module says something changed (its revision moves) or a
//! different channel comes on screen.

use antiphon_client::community::{self, Event, Store};
use antiphon_client::{config, connection_mode, delivery, history};
use antiphon_crypto::community::{perm, Key, Kind};
use antiphon_ui as ui;

use crate::engine::{clock_time, day_label, short_time};

/// The screens' copy of every space, kept between frames.
#[derive(Default)]
pub struct Spaces {
    /// The community module's revision the copy was built at; `None`
    /// before the first build.
    built_at: Option<u64>,
    /// The channel the copy of messages is for.
    built_for: Option<(String, u32)>,
    /// Every space, as the list shows them.
    communities: Vec<ui::Community>,
    /// Invitations waiting for an answer.
    invites: Vec<ui::SpaceInvite>,
    /// The open channel's messages.
    channel: Option<ui::ChannelView>,
    /// The voice channel we are in, as (space id, channel).
    voice_room: Option<(String, u32)>,
    /// The last link made for another community's admin, by the space
    /// (hex) it was made from.
    ally_links: std::collections::HashMap<String, String>,
}

impl Spaces {
    /// What the screens show: every space, the invitations, and the
    /// messages of `viewing` (the channel on screen, which counts as read).
    pub fn model(&mut self, viewing: Option<(&str, u32)>) -> (Vec<ui::Community>, Vec<ui::SpaceInvite>, Option<ui::ChannelView>) {
        let revision = community::revision();
        let viewing_owned = viewing.map(|(id, ch)| (id.to_string(), ch));
        if self.built_at != Some(revision) || self.built_for != viewing_owned {
            self.rebuild(viewing);
            self.built_at = Some(revision);
            self.built_for = viewing_owned;
        }
        (self.communities.clone(), self.invites.clone(), self.channel.clone())
    }

    /// A change to the spaces `community` shares with other communities
    /// (spec §12 Q4). Returns what to tell the person.
    pub fn shared(&mut self, community: &str, change: ui::SpaceChange) -> Result<String, String> {
        use ui::SpaceChange as C;
        let store = Store::app().ok_or("Your identity isn't loaded yet")?;
        let id = key(community)?;
        let said = match change {
            C::StartShared(name) => {
                let link = store.start_shared_space(&id, &name)?;
                self.ally_links.insert(community.to_string(), link);
                "Shared space started. Copy the link for the other community's admin.".to_string()
            }
            C::NewAllyLink => {
                let link = store.new_ally_link(&id)?;
                self.ally_links.insert(community.to_string(), link);
                "Link made. Copy it for the other community's admin.".to_string()
            }
            C::JoinShared(link) => {
                store.join_shared_space(&id, &link)?;
                "Asked to join. Their invitation shows up with your others; once you accept, it's on this community's list.".to_string()
            }
            C::AskIntoShared(space) => {
                store.ask_into_shared_space(&id, &key(&space)?)?;
                "Asked to join. The invitation shows up with your others.".to_string()
            }
            C::UnlistShared(space) => {
                store.unlist_shared_space(&id, &key(&space)?)?;
                "Taken off the list. Members already in it stay.".to_string()
            }
            _ => return Err("not a change to shared spaces".into()),
        };
        self.built_at = None;
        Ok(said)
    }

    /// Notes which voice channel we are in, for the screens.
    pub fn set_voice_room(&mut self, room: Option<(String, u32)>) {
        if self.voice_room != room {
            self.voice_room = room;
            self.built_at = None;
        }
    }

    /// Says we are in voice channel `channel` of `community`, with our
    /// invite link to be reached by.
    pub fn voice_join(&mut self, community: &str, channel: u32, invite: &str) -> Result<(), String> {
        let store = Store::app().ok_or("Your identity isn't loaded yet")?;
        store.with_space(&key(community)?, |space, _| Ok(((), space.voice_join(channel, invite)?, Vec::new())))
    }

    /// Says we left voice channel `channel` of `community`.
    pub fn voice_leave(&mut self, community: &str, channel: u32) -> Result<(), String> {
        let store = Store::app().ok_or("Your identity isn't loaded yet")?;
        store.with_space(&key(community)?, |space, _| Ok(((), space.voice_leave(channel)?, Vec::new())))
    }

    /// Who is in voice channel `channel` of `community` now, us included.
    pub fn voice_here(&self, community: &str, channel: u32) -> Vec<community::Presence> {
        let (Some(store), Ok(id)) = (Store::app(), key(community)) else { return Vec::new() };
        store.load(&id).map(|s| s.voice_here(channel)).unwrap_or_default()
    }

    /// A directory entry for `community` saying `description` (`None` for
    /// one that takes it off), and the key to sign it with: the one we are
    /// in it as.
    pub fn listing(&self, community: &str, description: Option<&str>) -> Result<(antiphon_net::dht::ListingEntry, antiphon_crypto::IdentityKeyPair), String> {
        let store = Store::app().ok_or("Your identity isn't loaded yet")?;
        let id = key(community)?;
        let space = store.load(&id).ok_or("couldn't open it")?;
        let name: String = space.manifest().name.chars().take(60).collect();
        let invite = match description {
            Some(_) => store.join_link(&id)?,
            None => String::new(),
        };
        let entry = antiphon_net::dht::ListingEntry { name, description: description.unwrap_or("").chars().take(240).collect(), invite };
        Ok((entry, store.identity_in(&space)))
    }

    /// Reads every space again from disk.
    fn rebuild(&mut self, viewing: Option<(&str, u32)>) {
        let Some(store) = Store::app() else {
            self.communities.clear();
            self.invites.clear();
            self.channel = None;
            return;
        };
        let cfg = config::load();
        let mut spaces: Vec<community::Space> = store.ids().iter().filter_map(|id| store.load(id)).collect();
        spaces.sort_by_key(|s| s.manifest().name.to_lowercase());
        self.communities = spaces
            .iter()
            .map(|s| {
                let id = delivery::hex(&s.id().0);
                // The one on screen is read as it arrives.
                let unread = if viewing.is_some_and(|(v, _)| v == id) {
                    store.mark_read(&s.id());
                    0
                } else {
                    store.unread(&s.id())
                };
                let mut c = community_model(&store, s, unread, self.voice_room.as_ref());
                c.ally_link = self.ally_links.get(&id).cloned();
                c
            })
            .collect();
        self.invites = store
            .invites()
            .into_iter()
            .map(|i| {
                let from = cfg
                    .contacts
                    .iter()
                    .find(|c| c.identity_key.eq_ignore_ascii_case(&delivery::hex(&i.from.0)))
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| i.card.name.clone());
                let kind = if i.kind == Kind::Group { "Group chat" } else { "Community" };
                ui::SpaceInvite { name: i.name.clone(), detail: format!("{kind} · from {from} · {}", short_time(i.at_secs)) }
            })
            .collect();
        self.channel = viewing.and_then(|(id, channel)| {
            let space = spaces.iter().find(|s| delivery::hex(&s.id().0) == id)?;
            Some(channel_model(&store, space, channel))
        });
    }

    /// Carries out one of the space actions. Returns the id of a space just
    /// made (for the screen to open), or why the action failed.
    pub fn act(&mut self, action: ui::Action) -> Result<Option<String>, String> {
        use ui::Action as A;
        let store = Store::app().ok_or("Your identity isn't loaded yet")?;
        match action {
            A::CreateSpace { name, group } => {
                let kind = if group { Kind::Group } else { Kind::Community };
                let id = store.create(kind, &name, false)?;
                return Ok(Some(delivery::hex(&id.0)));
            }
            A::AcceptSpaceInvite(index) => {
                let id = store.accept_invite(index, false)?;
                return Ok(Some(delivery::hex(&id.0)));
            }
            A::DeclineSpaceInvite(index) => store.decline_invite(index),
            A::Post { community, channel, text } => {
                let id = key(&community)?;
                store.with_space(&id, |space, _| {
                    let (outs, event) = space.post(channel, &text, None)?;
                    Ok(((), outs, vec![event]))
                })?;
            }
            A::EditPost { community, id: message, text } => {
                let id = key(&community)?;
                let channel = store.channel_of(&id, message);
                store.with_space(&id, |space, identity| {
                    let me = identity.public().identity_key;
                    let outs = space.edit(message, &text, channel)?;
                    Ok(((), outs, vec![Event::Edited { id: message, sender: me, text: text.trim().to_string() }]))
                })?;
            }
            A::DeletePost { community, id: message, author } => {
                let id = key(&community)?;
                let author = connection_mode::decode_hex32(&author).ok_or("whose message that is isn't known")?;
                let channel = store.channel_of(&id, message);
                store.with_space(&id, |space, _| Ok(((), space.delete(message, author, channel)?, vec![Event::Deleted { id: message, author }])))?;
            }
            A::InviteToSpace { community, person } => {
                let id = key(&community)?;
                let cfg = config::load();
                let contact = cfg.contacts.iter().find(|c| c.identity_key.eq_ignore_ascii_case(&person)).ok_or("They aren't one of your people")?;
                let who = connection_mode::decode_hex32(&contact.identity_key).ok_or("They have no key saved yet; call them once first")?;
                let name = contact.name.clone();
                store.with_space(&id, |space, identity| Ok(((), space.invite(identity, who, &name)?, vec![Event::Changed])))?;
            }
            A::AddChannel { community, name, voice } => {
                let id = key(&community)?;
                store.with_space(&id, |space, identity| {
                    let outs = space.update(identity, |m| {
                        // One past the highest, so a removed channel's
                        // number is never reused for a different one.
                        let next = m.channels.iter().map(|c| c.id).max().map_or(0, |n| n + 1);
                        m.channels.push(antiphon_crypto::community::Channel { id: next, name: name.clone(), voice, ..Default::default() });
                    })?;
                    Ok(((), outs, vec![Event::Changed]))
                })?;
            }
            A::SpaceNotify { community, channel, level } => {
                let id = key(&community)?;
                // The whole space has no "follow" setting: it is the default.
                let level = level.or(channel.is_none().then_some(ui::Notify::All)).map(from_ui);
                store.set_notify(&id, channel, level);
            }
            A::RemoveEmoji { community, name } => {
                let id = key(&community)?;
                store.with_space(&id, |space, identity| {
                    let outs = space.update(identity, |m| m.emoji.retain(|e| e.name != name))?;
                    Ok(((), outs, vec![Event::Changed]))
                })?;
            }
            A::ManageSpace { community, change: ui::SpaceChange::Report { channel, messages, reason } } => {
                let id = key(&community)?;
                // The excerpts are what this device's copy of the channel says.
                let log = history::load(&store.dir, &community::channel_log(&id, channel), &store.key);
                let excerpts: Vec<community::Excerpt> = messages
                    .iter()
                    .filter_map(|mid| log.iter().find(|m| m.id == *mid && !m.deleted))
                    .filter_map(|m| {
                        let author = connection_mode::decode_hex32(m.sender.as_deref()?)?;
                        Some(community::Excerpt { id: m.id, author: Key(author), text: m.text.clone(), at_secs: m.at_secs })
                    })
                    .collect();
                store.with_space(&id, |space, _| Ok(((), space.report(channel, &reason, excerpts)?, Vec::new())))?;
            }
            A::ManageSpace { community, change } => {
                let id = key(&community)?;
                store.with_space(&id, |space, identity| {
                    let outs = manage(space, identity, change)?;
                    Ok(((), outs, vec![Event::Changed]))
                })?;
            }
            A::LeaveSpace(community) => {
                let id = key(&community)?;
                // A space you were never let into has nobody to tell.
                let left = store.with_space(&id, |space, identity| {
                    if space.removed || space.joining.is_some() {
                        return Ok(((), Vec::new(), Vec::new()));
                    }
                    Ok(((), space.leave(identity)?, vec![Event::Changed]))
                });
                left?;
                store.forget(&id);
            }
            _ => {}
        }
        Ok(None)
    }
}

/// Adds a custom emoji to a community: `file_name` is the picked file's,
/// made into the emoji's name; `image` is its picture as
/// `profile::emoji_from_image` makes it. Returns the name it got.
pub fn add_emoji(community: &str, file_name: &str, image: String) -> Result<String, String> {
    let store = Store::app().ok_or("Your identity isn't loaded yet")?;
    let id = key(community)?;
    let mut name = emoji_name(file_name);
    store.with_space(&id, |space, identity| {
        let taken = |n: &str| space.manifest().emoji.iter().any(|e| e.name == n);
        // A name already used gets a number rather than replacing it.
        let base = name.clone();
        let mut n = 2;
        while taken(&name) {
            name = format!("{}{n}", &base[..base.len().min(antiphon_crypto::community::MAX_EMOJI_NAME - 2)]);
            n += 1;
        }
        if space.manifest().emoji.len() >= antiphon_crypto::community::MAX_EMOJI {
            return Err("It has as many emoji as it can; remove one first".into());
        }
        let emoji = antiphon_crypto::community::Emoji { name: name.clone(), image };
        let outs = space.update(identity, |m| m.emoji.push(emoji))?;
        Ok(((), outs, vec![Event::Changed]))
    })?;
    Ok(name)
}

/// An emoji name from a file name: its stem in lowercase, anything but
/// letters and digits made an underscore, cut to length.
fn emoji_name(file_name: &str) -> String {
    let stem = file_name.rsplit_once('.').map_or(file_name, |(s, _)| s);
    let mut name: String = stem.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    name = name.trim_matches('_').to_string();
    name.truncate(antiphon_crypto::community::MAX_EMOJI_NAME);
    if name.len() < 2 {
        name = "emoji".into();
    }
    name
}

/// What a role can be allowed, in the order the roles dialog lists them:
/// the permission bit and how the dialog says it.
const PERMS: [(u32, &str); 8] = [
    (perm::POST, "Post messages"),
    (perm::INVITE, "Invite people"),
    (perm::MANAGE_MESSAGES, "Remove other people's messages"),
    (perm::KICK, "Remove members below them"),
    (perm::BAN, "Ban members below them"),
    (perm::MANAGE_ROLES, "Give members roles below their own"),
    (perm::MANAGE_CHANNELS, "Add channels and choose who reads them"),
    (perm::MANAGE_COMMUNITY, "Change the community's name and settings"),
];

/// Signs one change to how a community is set up into its manifest and
/// sends it to everyone. The manifest's own rules refuse what the signer
/// isn't allowed (`check_transition`), so this only says why up front
/// where it can.
fn manage(space: &mut community::Space, identity: &antiphon_crypto::IdentityKeyPair, change: ui::SpaceChange) -> Result<Vec<community::Out>, String> {
    use ui::SpaceChange as C;
    let member_key = |hex: &str| connection_mode::decode_hex32(hex).map(Key).ok_or_else(|| "That member isn't known".to_string());
    match change {
        C::ChannelPrivate { channel, private } => {
            let me = space.me();
            space.update(identity, |m| {
                let owner = m.is_owner(&me);
                if let Some(c) = m.channels.iter_mut().find(|c| c.id == channel) {
                    // Whoever makes it private keeps reading it, unless an
                    // owner (who always can).
                    c.readers = private.then(|| if owner { Vec::new() } else { vec![Key(me)] });
                }
            })
        }
        C::ChannelReader { channel, member, on } => {
            let who = member_key(&member)?;
            space.update(identity, |m| {
                if let Some(readers) = m.channels.iter_mut().find(|c| c.id == channel).and_then(|c| c.readers.as_mut()) {
                    readers.retain(|k| *k != who);
                    if on {
                        readers.push(who);
                    }
                }
            })
        }
        C::AddRole(name) => {
            if space.manifest().roles.len() >= antiphon_crypto::community::MAX_ROLES {
                return Err(format!("A community can have at most {} roles", antiphon_crypto::community::MAX_ROLES));
            }
            // New roles start out able to do what newcomers can.
            let perms = space.manifest().roles.last().map_or(perm::POST, |r| r.perms);
            space.update(identity, |m| {
                let at = m.roles.len().saturating_sub(1);
                m.insert_role(at, antiphon_crypto::community::Role { name: name.trim().to_string(), perms });
            })
        }
        C::RemoveRole(at) => {
            if at + 1 >= space.manifest().roles.len() {
                return Err("The role newcomers get can't be removed".into());
            }
            space.update(identity, |m| {
                m.remove_role(at);
            })
        }
        C::RolePerm { role, perm: which, on } => {
            let (bit, _) = *PERMS.get(which).ok_or("That isn't something a role can do")?;
            space.update(identity, |m| {
                if let Some(r) = m.roles.get_mut(role) {
                    if on {
                        r.perms |= bit;
                    } else {
                        r.perms &= !bit;
                    }
                }
            })
        }
        C::Listed(on) => space.update(identity, |m| m.listed = on),
        // The engine's: they go to a directory, not to the members.
        C::SubmitListing { .. } | C::WithdrawListing(_) => Ok(Vec::new()),
        // Handled by `Spaces::shared`: they span more than one space.
        C::StartShared(_) | C::JoinShared(_) | C::AskIntoShared(_) | C::UnlistShared(_) | C::NewAllyLink => Ok(Vec::new()),
        C::SlowMode(secs) => space.update(identity, |m| m.slow_mode_secs = secs.min(antiphon_crypto::community::MAX_SLOW_MODE_SECS)),
        C::WithdrawInvite(who) => {
            let who = member_key(&who)?;
            space.withdraw_invite(who.0);
            Ok(Vec::new())
        }
        C::DismissReport(id) => {
            space.dismiss_report(id);
            Ok(Vec::new())
        }
        C::RemoveMember { member, ban } => {
            let who = member_key(&member)?;
            if ban {
                space.ban(identity, who.0, "")
            } else {
                space.kick(identity, who.0)
            }
        }
        // Built from history before the space is opened (see `act`).
        C::Report { .. } => Ok(Vec::new()),
        // The engine's, which also puts the call through.
        C::JoinVoice(_) | C::LeaveVoice(_) => Ok(Vec::new()),
        C::MemberRole { member, role } => {
            let who = member_key(&member)?;
            space.update(identity, |m| {
                if let Some(x) = m.members.iter_mut().find(|x| x.key == who) {
                    x.role = role as u16;
                }
            })
        }
    }
}

/// A space's id from its hex.
fn key(hex: &str) -> Result<Key, String> {
    connection_mode::decode_hex32(hex).map(Key).ok_or_else(|| "That isn't one of yours".to_string())
}

/// The store's notification setting as the screens name it.
fn to_ui(n: community::Notify) -> ui::Notify {
    match n {
        community::Notify::All => ui::Notify::All,
        community::Notify::Mentions => ui::Notify::Mentions,
        community::Notify::Nothing => ui::Notify::Nothing,
    }
}

/// The screens' notification setting as the store keeps it.
fn from_ui(n: ui::Notify) -> community::Notify {
    match n {
        ui::Notify::All => community::Notify::All,
        ui::Notify::Mentions => community::Notify::Mentions,
        ui::Notify::Nothing => community::Notify::Nothing,
    }
}

/// One space as the screens show it.
fn community_model(store: &Store, s: &community::Space, unread: u32, voice_room: Option<&(String, u32)>) -> ui::Community {
    let m = s.manifest();
    let me = s.me();
    let joined = s.joining.is_none() && !s.removed;
    let group = m.kind == Kind::Group;
    let assigns = joined && !group && m.perms(&me) & perm::MANAGE_ROLES != 0;
    let last = m.roles.len().saturating_sub(1);
    let members = m
        .members
        .iter()
        .map(|x| {
            let k = x.key.0;
            let owner = m.is_owner(&k);
            let role = if owner { "Owner".to_string() } else { m.roles.get(x.role as usize).map(|r| r.name.clone()).unwrap_or_default() };
            // Only someone you outrank, and only to a role at or below
            // your own rank (the manifest's re-roling rule).
            let assignable = if assigns && !owner && m.can(&me, perm::MANAGE_ROLES, Some(&k)) {
                (0..m.roles.len()).filter(|&i| i as u32 >= m.rank(&me)).collect()
            } else {
                Vec::new()
            };
            ui::Member {
                id: delivery::hex(&k),
                name: s.name_of(&k),
                status: s.card(&k).map(|c| c.status.clone()).unwrap_or_default(),
                role,
                me: k == me,
                role_index: (!owner).then_some(x.role as usize),
                plain: group || (!owner && x.role as usize == last),
                assignable,
            }
        })
        .collect::<Vec<_>>();
    let roles = if group {
        Vec::new()
    } else {
        m.roles
            .iter()
            .enumerate()
            .map(|(i, r)| ui::Role {
                name: r.name.clone(),
                perms: PERMS.iter().map(|(bit, what)| (what.to_string(), r.perms & bit == *bit)).collect(),
                holders: m.members.iter().filter(|x| x.role as usize == i && !m.is_owner(&x.key.0)).count(),
            })
            .collect()
    };
    let count = members.len();
    ui::Community {
        id: delivery::hex(&s.id().0),
        name: m.name.clone(),
        group,
        detail: format!("{} · {count} {}", if group { "Group chat" } else { "Community" }, if count == 1 { "member" } else { "members" }),
        // A private channel you can't read isn't shown at all.
        channels: m
            .channels
            .iter()
            .filter(|c| m.can_read(c.id, &me) || s.joining.is_some())
            .map(|c| ui::Channel {
                id: c.id,
                name: c.name.clone(),
                topic: c.topic.clone(),
                private: c.readers.is_some(),
                readers: c.readers.iter().flatten().map(|k| delivery::hex(&k.0)).collect(),
                voice: c.voice,
                in_voice: if c.voice { s.voice_here(c.id).iter().map(|p| s.name_of(&p.key.0)).collect() } else { Vec::new() },
                notify: store.notify_setting(&s.id(), Some(c.id)).map(to_ui),
            })
            .collect(),
        // A space muted with the older ring rule shows as muted here too.
        notify: match store.notify_setting(&s.id(), None) {
            Some(n) => to_ui(n),
            None if store.ring_rule(&s.id()) == Some(antiphon_client::notify::RingRule::Never) => ui::Notify::Nothing,
            None => ui::Notify::All,
        },
        members,
        unread,
        waiting: if s.joining.is_some() {
            Some("Waiting to be let in: whoever invited you adds you when their app next hears from yours".into())
        } else if s.removed {
            Some("You're no longer in this".into())
        } else {
            None
        },
        can_post: joined && m.can(&me, perm::POST, None),
        can_invite: joined && m.can(&me, perm::INVITE, None),
        can_manage_channels: joined && !group && m.can(&me, perm::MANAGE_CHANNELS, None),
        can_manage: joined && !group && m.can(&me, perm::MANAGE_COMMUNITY, None),
        emoji: m
            .emoji
            .iter()
            .map(|e| {
                // Decoded once, when its picture changes.
                ui::avatars::set(&ui::avatars::emoji_id(&delivery::hex(&s.id().0), &e.name), &e.image, "", antiphon_client::profile::decode_emoji);
                e.name.clone()
            })
            .collect(),
        roles,
        can_edit_roles: joined && !group && m.is_owner(&me),
        can_assign_roles: assigns,
        slow_mode_secs: m.slow_mode_secs,
        can_set_slow_mode: joined && !group && m.can(&me, perm::MANAGE_COMMUNITY, None),
        moderator: joined && community::is_moderator(m, &me),
        reports: s
            .reports()
            .iter()
            .map(|r| ui::Report {
                id: r.id,
                from: s.name_of(&r.from.0),
                channel: m.channel(r.channel).map(|c| c.name.clone()).unwrap_or_default(),
                reason: r.reason.clone(),
                when: short_time(r.at_secs),
                excerpts: r
                    .excerpts
                    .iter()
                    .map(|e| {
                        let a = e.author.0;
                        ui::Excerpt {
                            id: e.id,
                            author: s.name_of(&a),
                            author_id: delivery::hex(&a),
                            text: e.text.clone(),
                            verified: store.matches_history(&s.id(), r.channel, e),
                            can_remove: m.can(&me, perm::MANAGE_MESSAGES, Some(&a)),
                            can_kick: m.is_member(&a) && m.can(&me, perm::KICK, Some(&a)),
                            can_ban: m.is_member(&a) && m.can(&me, perm::BAN, Some(&a)),
                        }
                    })
                    .collect(),
            })
            .collect(),
        listed: m.listed,
        shared_spaces: m
            .allies
            .iter()
            .map(|a| ui::SharedSpace { id: delivery::hex(&a.space.0), name: a.name.clone(), joined: store.load(&a.space).is_some_and(|x| !x.removed) })
            .collect(),
        sharing: m.alliance.len(),
        ally_link: None,
        join_link: (m.listed && joined && m.can(&me, perm::INVITE, None)).then(|| store.join_link(&s.id()).ok()).flatten(),
        voice_room: voice_room.filter(|(id, _)| *id == delivery::hex(&s.id().0)).map(|(_, ch)| *ch),
        pending_invites: s
            .pending_invites()
            .into_iter()
            .map(|p| ui::PendingInvite { id: delivery::hex(&p.key), name: p.name, when: short_time(p.at_secs) })
            .collect(),
    }
}

/// A channel's messages from its history, oldest first, with who wrote
/// each and a day rule where the day changes. Replies show in the channel
/// with the rest; threads are not shown apart yet.
fn channel_model(store: &Store, space: &community::Space, channel: u32) -> ui::ChannelView {
    let me = space.me();
    let log = history::load(&store.dir, &community::channel_log(&space.id(), channel), &store.key);
    let mut last_day = None;
    let lines = log
        .iter()
        .map(|msg| {
            let sender = msg.sender.as_deref().and_then(connection_mode::decode_hex32).unwrap_or(me);
            let day = day_label(msg.at_secs);
            let line = ui::Line {
                id: msg.id,
                mine: sender == me,
                author: space.name_of(&sender),
                author_id: delivery::hex(&sender),
                text: if msg.deleted { String::new() } else { msg.text.clone() },
                deleted: msg.deleted,
                time: clock_time(msg.at_secs),
                day: (last_day.as_ref() != Some(&day)).then(|| day.clone()),
                edited: msg.edited,
                ..ui::Line::default()
            };
            last_day = Some(day);
            line
        })
        .collect();
    ui::ChannelView { community: delivery::hex(&space.id().0), channel, lines }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_names_come_from_file_names() {
        assert_eq!(emoji_name("Party Cat.png"), "party_cat");
        assert_eq!(emoji_name("x.gif"), "emoji");
        assert_eq!(emoji_name("__ok__.jpeg"), "ok");
        assert!(antiphon_crypto::community::Emoji::valid_name(&emoji_name("Ünïcode héllo wörld that is far too long.png")));
    }
}
