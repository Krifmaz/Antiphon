//! The messaging screens: chat, files, contacts, recent calls, and the
//! battery and data settings.
//!
//! A child of `ui` so it can read the app's state directly; nothing here
//! changes it -- every button is a [`Message`] handled in `ui`. Every
//! control carries one plain sentence on what it does, which is the
//! project's rule for anything a user can switch.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Element, Length, Theme};
use iced_widget::{button, column, container, mouse_area, pick_list, progress_bar, row, scrollable, text, text_input, toggler};

use super::{App, Expiry, Message, Renderer};
use crate::call::now_secs;
use crate::contacts::hex;
use crate::conversation::reaction_label;
use crate::data_rules::{self, Link};
use crate::reactions;

/// Every screen and widget here produces the app's [`Message`] on the
/// shared renderer.
type View<'a> = Element<'a, Message, Theme, Renderer>;

/// A padded, rounded box filling the width, which groups one thing's
/// text and buttons together.
fn card<'a>(content: impl Into<View<'a>>) -> View<'a> {
    container(content).padding(12).width(Length::Fill).style(container::rounded_box).into()
}

/// The small explanatory line under a control (13 logical pixels).
fn help<'a>(line: impl text::IntoFragment<'a>) -> iced_widget::Text<'a, Theme, Renderer> {
    text(line).size(13)
}

/// Progress as a fraction from 0 to 1 for a progress bar. Nothing to do
/// reads as finished rather than dividing by zero.
fn fraction(done: u32, total: u32) -> f32 {
    if total == 0 {
        1.0
    } else {
        done as f32 / total as f32
    }
}

impl App {
    /// Who is on the call, against your contacts: save them, or mark them
    /// verified once the code has been compared.
    pub(super) fn caller_card(&self, key_hex: &str) -> View<'_> {
        match self.contacts.by_key(key_hex) {
            Some(contact) if contact.verified => card(
                column![
                    text(format!("{} is verified.", contact.name)),
                    help("You compared the code on an earlier call; it is the same person as then."),
                ]
                .spacing(4),
            ),
            Some(contact) => card(
                column![
                    text(format!("{} is not verified yet.", contact.name)),
                    help("Once you have read the code above to each other and it matched, mark them verified."),
                    button("The code matched: mark verified").on_press(Message::MarkVerified(key_hex.to_string(), true)),
                ]
                .spacing(6),
            ),
            None => card(
                column![
                    text("Not in your contacts."),
                    help("Save them to call them again from Contacts and keep your messages with them."),
                    row![
                        text_input("Their name", &self.save_name).on_input(Message::SaveCallerName).on_submit(Message::SaveCaller).padding(8),
                        button("Save").on_press(Message::SaveCaller),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                ]
                .spacing(6),
            ),
        }
    }

    /// The Chat screen: the open conversation with its messages, actions on
    /// the selected one, and the composer; or the list of conversations
    /// when none is open.
    pub(super) fn chat_view(&self) -> View<'_> {
        let Some(chat) = &self.chat else { return self.chat_list_view() };
        let live = self.engine.live();
        let on_call = live.as_ref().is_some_and(|l| hex(&l.peer_key) == chat.peer_hex);
        let now = now_secs();

        let mut header = column![
            row![
                text(self.display_name_for(&chat.peer_hex)).size(20).width(Length::Fill),
                button("All chats").style(button::secondary).on_press(Message::CloseChat),
            ]
            .align_y(Alignment::Center),
            help(if on_call {
                "On a call now: messages go straight to them."
            } else {
                "Not on a call: new messages wait on this phone and are sent when you next talk."
            }),
            row![
                text("Disappearing").size(14),
                pick_list(Expiry::ALL, Some(Expiry::from_secs(chat.expire_after_secs())), Message::Expiry).text_size(14),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            help("New messages in this conversation delete themselves from this phone after this long."),
        ]
        .spacing(6);
        if !chat.outbox.is_empty() {
            header = header.push(help(format!("{} waiting to be sent.", chat.outbox.len())));
        }

        let mut list = column![].spacing(8);
        if chat.messages.is_empty() {
            list = list.push(help("No messages yet."));
        }
        for message in &chat.messages {
            let mut meta = data_rules::ago(now, message.at_secs);
            if message.from_me && !message.deleted {
                meta = format!("{meta} · {}", chat.status(message.id, on_call));
            }
            if message.expires_at_secs.is_some() {
                meta = format!("{meta} · disappears");
            }
            let mut bubble = column![text(message.display_text()).wrapping(Wrapping::WordOrGlyph), help(meta)].spacing(2);
            let summary = reaction_summary(&chat.reactions, message.id);
            if !summary.is_empty() {
                bubble = bubble.push(text(summary).size(13));
            }
            let bubble = container(bubble)
                .padding(10)
                .max_width(300)
                .style(if message.from_me { container::primary } else { container::secondary });
            list = list.push(
                container(mouse_area(bubble).on_press(Message::SelectChat(Some(message.id))))
                    .width(Length::Fill)
                    .align_x(if message.from_me { Alignment::End } else { Alignment::Start }),
            );

            if self.selected == Some(message.id) && !message.deleted {
                let mut actions = column![].spacing(6);
                if on_call {
                    let mut buttons: Vec<View<'_>> = reactions::AVAILABLE
                        .iter()
                        .map(|emoji| {
                            button(text(reaction_label(emoji)).size(13))
                                .style(button::secondary)
                                .padding([6, 6])
                                .on_press(Message::React(message.id, emoji))
                                .into()
                        })
                        .collect();
                    // Two rows of four fit a phone held upright.
                    let second: Vec<View<'_>> = buttons.split_off(4);
                    actions = actions.push(row(buttons).spacing(6)).push(row(second).spacing(6));
                    actions = actions.push(help("Tap a reaction again to take it back."));
                } else {
                    actions = actions.push(help("Reactions are sent during a call."));
                }
                let mut own = row![].spacing(8);
                if message.from_me && chat.can_change(message.id, on_call) {
                    own = own
                        .push(button("Edit").style(button::secondary).on_press(Message::EditChat(message.id)))
                        .push(button("Delete").style(button::danger).on_press(Message::DeleteChat(message.id)));
                }
                own = own.push(button("Close").style(button::text).on_press(Message::SelectChat(None)));
                actions = actions.push(own);
                list = list.push(card(actions));
            }
        }

        let mut composer = column![].spacing(6);
        if self.editing.is_some() {
            composer = composer.push(
                row![
                    help("Editing your message. They will see it was edited.").width(Length::Fill),
                    button("Cancel").style(button::text).on_press(Message::CancelEdit),
                ]
                .align_y(Alignment::Center),
            );
        }
        composer = composer.push(
            row![
                text_input("Message", &self.draft).on_input(Message::Draft).on_submit(Message::SendChat).padding(10),
                button(if self.editing.is_some() { "Save" } else { "Send" }).on_press(Message::SendChat),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );

        column![header, scrollable(list).anchor_bottom().height(Length::Fill).spacing(6), composer].spacing(10).into()
    }

    /// Every contact with a known key, each with a button to open their
    /// conversation. Contacts saved from a bare address are left out until
    /// a call pins their key, since there is no history to file under.
    fn chat_list_view(&self) -> View<'_> {
        let mut content = column![
            text("Messages").size(22),
            help("During a call, Chat opens the person you are talking to. Here you can read back conversations with your contacts, and write to them for later."),
        ]
        .spacing(10);
        if self.contacts.list.is_empty() {
            content = content.push(help("No contacts yet. Save someone during a call, or add them from an invite in Contacts."));
        }
        for contact in self.contacts.list.iter().filter(|c| !c.identity_key.is_empty()) {
            content = content.push(
                row![
                    text(contact.name.as_str()).width(Length::Fill),
                    button("Open").style(button::secondary).on_press(Message::OpenChat(contact.identity_key.clone())),
                ]
                .align_y(Alignment::Center),
            );
        }
        content.into()
    }

    /// The Files screen: an offer to answer, the file arriving, the file
    /// being sent, the queue in sending order, and what has been received.
    pub(super) fn files_view(&self) -> View<'_> {
        let live = self.engine.live();
        let live_peer = live.as_ref().map(|l| hex(&l.peer_key));
        let mut content = column![
            text("Files").size(22),
            help("Files go to the person you are on a call with, one at a time; the rest wait here in order."),
        ]
        .spacing(10);
        match &live {
            Some(_) => content = content.push(button("Send a file").on_press(Message::PickFile)),
            None => content = content.push(help("Start a call to send or receive files. Files queued for someone go when you next talk to them.")),
        }
        let hold = self.transfer_hold();
        if let Some(hold) = hold {
            if self.files.active.is_some() || !self.files.queue.is_empty() {
                content = content.push(card(text(hold.explain())));
            }
        }

        // Incoming: an offer to answer, or a file arriving.
        if let Some(live) = &live {
            let files = live.files.lock().unwrap();
            let who = self.display_name_for(&hex(&live.peer_key));
            if let Some(offer) = &files.pending_offer {
                let mut offer_card = column![
                    text(format!("{who} wants to send you {} ({}).", offer.file_name, data_rules::size(offer.file_size))),
                ]
                .spacing(8);
                if !offer.thumbnail_png.is_empty() {
                    offer_card = offer_card.push(help("They attached a preview picture; it is not shown here yet."));
                }
                if hold.is_some() && self.link() == Link::Metered {
                    offer_card = offer_card.push(help("You are on mobile data: accepting downloads it now, whatever the Wi-Fi setting says."));
                }
                offer_card = offer_card.push(
                    row![
                        button("Accept").style(button::success).on_press(Message::AcceptOffer),
                        button("Decline").style(button::secondary).on_press(Message::DeclineOffer),
                    ]
                    .spacing(10),
                );
                content = content.push(card(offer_card));
            }
            if let Some(incoming) = &files.incoming {
                let state = if incoming.verifying {
                    "checking it arrived intact".to_string()
                } else if incoming.resumed_chunks > 0 {
                    let pct = incoming.resumed_chunks as u64 * 100 / incoming.total_chunks.max(1) as u64;
                    format!("resumed; {pct}% was already here")
                } else {
                    format!("{:.0}%", fraction(incoming.chunks_received, incoming.total_chunks) * 100.0)
                };
                let mut incoming_card = column![
                    text(format!("Receiving {} ({})", incoming.file_name, data_rules::size(incoming.file_size))),
                    progress_bar(0.0..=1.0, fraction(incoming.chunks_received, incoming.total_chunks)).girth(8),
                    help(state),
                ]
                .spacing(6);
                if files.outgoing.is_none() && !incoming.verifying {
                    incoming_card = incoming_card.push(button("Stop receiving").style(button::danger).on_press(Message::CancelIncoming));
                }
                content = content.push(card(incoming_card));
            }
        }

        // The file being sent.
        if let Some(active) = &self.files.active {
            let item = &active.entry.item;
            let (progress, state) = if active.preparing() {
                (0.0, "Getting the file ready...".to_string())
            } else {
                let files = live.as_ref().map(|l| l.files.lock().unwrap());
                match files.as_ref().and_then(|f| f.outgoing.as_ref()) {
                    Some(o) if !o.accepted => (0.0, "Waiting for them to accept.".to_string()),
                    Some(o) => {
                        let progress = fraction(o.chunks_sent, o.total_chunks);
                        let state = match (active.user_paused, active.held) {
                            (true, _) => "Paused.".to_string(),
                            (false, Some(hold)) => hold.explain().to_string(),
                            _ if o.chunks_sent >= o.total_chunks => "Waiting for them to confirm it arrived.".to_string(),
                            _ => format!("{:.0}%", progress * 100.0),
                        };
                        (progress, state)
                    }
                    None => (0.0, "Starting...".to_string()),
                }
            };
            let pause = if active.user_paused {
                button("Resume").on_press(Message::PauseSend(false))
            } else {
                button("Pause").style(button::secondary).on_press(Message::PauseSend(true))
            };
            content = content.push(card(
                column![
                    text(format!("Sending {}", item.name)),
                    progress_bar(0.0..=1.0, progress).girth(8),
                    help(state),
                    row![pause, button("Cancel").style(button::danger).on_press(Message::CancelSend)].spacing(10),
                    help("Pause keeps your place, so resuming carries on rather than starting over."),
                ]
                .spacing(6),
            ));
        }

        // The queue.
        if !self.files.queue.is_empty() {
            content = content.push(text("Waiting to send").size(18));
            content = content.push(help("Up and Down change the order; Hold keeps a file back without losing its place."));
            for entry in self.files.queue.iter() {
                let item = &entry.item;
                let mut line = item.name.clone();
                if let Some(size) = item.size {
                    line = format!("{line} ({})", data_rules::size(size));
                }
                let mut detail = Vec::new();
                if live_peer.as_deref() != Some(item.peer_hex.as_str()) {
                    detail.push(format!("for {}, next call", self.display_name_for(&item.peer_hex)));
                }
                if entry.held {
                    detail.push("on hold".to_string());
                }
                let mut entry_card = column![text(line).wrapping(Wrapping::WordOrGlyph)].spacing(6);
                if !detail.is_empty() {
                    entry_card = entry_card.push(help(detail.join(", ")));
                }
                entry_card = entry_card.push(
                    row![
                        button(text("Up").size(13)).style(button::secondary).on_press(Message::QueueUp(entry.id)),
                        button(text("Down").size(13)).style(button::secondary).on_press(Message::QueueDown(entry.id)),
                        button(text(if entry.held { "Release" } else { "Hold" }).size(13))
                            .style(button::secondary)
                            .on_press(Message::QueueHold(entry.id, !entry.held)),
                        button(text("Remove").size(13)).style(button::danger).on_press(Message::QueueRemove(entry.id)),
                    ]
                    .spacing(6),
                );
                content = content.push(card(entry_card));
            }
        }

        if let Some(line) = &self.files.last_line {
            content = content.push(help(line.as_str()));
        }
        if !self.files.received.is_empty() {
            content = content.push(text("Received").size(18));
            for (name, location) in &self.files.received {
                content = content.push(
                    column![text(name.as_str()), help(location.as_str()).wrapping(Wrapping::Glyph)].spacing(2),
                );
            }
        }
        content.into()
    }

    /// The Contacts screen: adding someone from an invite link, then every
    /// contact with their verification state and buttons to call, chat or
    /// remove them.
    pub(super) fn contacts_view(&self) -> View<'_> {
        let mut content = column![
            text("Contacts").size(22),
            help("A contact is their key, so a call to them is refused if anyone else answers. Names are yours; they never see them."),
            text("Add from an invite").size(18),
            text_input("Their name", &self.contact_name).on_input(Message::ContactName).padding(10),
            text_input("antiphon://... invite link", &self.contact_invite)
                .on_input(Message::ContactInvite)
                .on_submit(Message::AddContact)
                .padding(10),
            row![
                button("Paste").style(button::secondary).on_press(Message::PasteContactInvite),
                button("Add contact").on_press(Message::AddContact),
            ]
            .spacing(10),
        ]
        .spacing(10);

        if self.contacts.list.is_empty() {
            content = content.push(help("Nobody saved yet."));
        }
        for contact in &self.contacts.list {
            let key = contact.identity_key.clone();
            let mut details = vec![if contact.verified { "Verified".to_string() } else { "Not verified".to_string() }];
            let fingerprint = contact.fingerprint();
            if !fingerprint.is_empty() {
                details.push(fingerprint);
            }
            let address = if contact.address.is_empty() { "No address saved".to_string() } else { contact.address.clone() };
            let remove_label = if self.remove_armed.as_deref() == Some(key.as_str()) { "Really remove?" } else { "Remove" };
            let mut buttons = row![].spacing(8);
            if contact.address.is_empty() {
                buttons = buttons.push(button("Call"));
            } else {
                buttons = buttons.push(button("Call").style(button::success).on_press(Message::CallContact(key.clone())));
            }
            buttons = buttons
                .push(button("Chat").style(button::secondary).on_press(Message::OpenChat(key.clone())))
                .push(button(remove_label).style(button::danger).on_press(Message::RemoveContact(key.clone())));
            if contact.verified {
                buttons = buttons.push(button("Unverify").style(button::text).on_press(Message::MarkVerified(key, false)));
            }
            content = content.push(card(
                column![
                    text(contact.name.as_str()).size(18),
                    help(details.join(" · ")).wrapping(Wrapping::Glyph),
                    help(address).wrapping(Wrapping::Glyph),
                    buttons,
                ]
                .spacing(6),
            ));
        }
        content.into()
    }

    /// The Recent screen: past calls, newest first, each with a button to
    /// call back and, when the key is known, to open the chat.
    pub(super) fn recent_view(&self) -> View<'_> {
        let now = now_secs();
        let mut content = column![
            text("Recent calls").size(22),
            help("Kept on this phone only, encrypted with the same key as your messages."),
        ]
        .spacing(10);
        if self.calls.is_empty() {
            content = content.push(help("No calls yet."));
        }
        for (index, record) in self.calls.iter().enumerate() {
            let who = if record.peer_key.is_empty() { record.remote.clone() } else { self.display_name_for(&record.peer_key) };
            let direction = if record.outgoing { "Outgoing" } else { "Incoming" };
            let outcome = match record.duration_secs {
                Some(secs) => format!("{direction}, {} · {}", data_rules::clock(secs), record.outcome),
                None => format!("{direction} · {}", record.outcome),
            };
            let mut buttons = row![button("Call back").style(button::success).on_press(Message::CallBack(index))].spacing(8);
            if !record.peer_key.is_empty() {
                buttons = buttons.push(button("Chat").style(button::secondary).on_press(Message::OpenChat(record.peer_key.clone())));
            }
            content = content.push(card(
                column![
                    text(who).wrapping(Wrapping::WordOrGlyph),
                    help(format!("{} · {outcome}", data_rules::ago(now, record.started_secs))),
                    buttons,
                ]
                .spacing(6),
            ));
        }
        if !self.calls.is_empty() {
            let label = if self.clear_calls_armed { "Really clear all?" } else { "Clear call history" };
            content = content.push(button(label).style(button::danger).on_press(Message::ClearCalls));
        }
        content.into()
    }

    /// The Settings screen: your display name, then the data and battery
    /// rules, with the network and battery saver state as they are now
    /// beside the switches they affect.
    pub(super) fn settings_view(&self) -> View<'_> {
        let settings = &self.settings;
        let setting = |toggle: iced_widget::Toggler<'static, Message, Theme, Renderer>, line: &'static str| -> View<'static> {
            column![toggle, help(line)].spacing(4).into()
        };
        let network = match self.link() {
            Link::Wifi => "Wi-Fi",
            Link::Metered => "mobile data",
            Link::Offline => "no connection",
            Link::Unknown => "a network this phone does not describe",
        };
        let used = data_rules::size(self.meter.used(now_secs()));

        column![
            text("Settings").size(22),
            text("Your name").size(18),
            text_input("Name shown to people you call", &self.settings.display_name).on_input(Message::DisplayName).padding(10),
            help("Sent to the other side when a call connects. Their contacts still show the name they gave you."),
            text("Data").size(18),
            help(format!("Connected over {network} now.")),
            setting(
                toggler(settings.wifi_only_transfers).label("Files only on Wi-Fi").on_toggle(Message::WifiOnly),
                "Files wait in the queue while you are on mobile data, and send once you are on Wi-Fi.",
            ),
            row![
                text("Mobile data limit for files (MB a month)").size(14).width(Length::Fill),
                text_input("none", &self.limit_text).on_input(Message::CellularLimit).padding(8).width(Length::Fixed(90.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            help(format!("With Wi-Fi only off, transfers stop for the month once they have used this much mobile data; empty means no limit. Used this month: {used}.")),
            setting(
                toggler(settings.data_saver).label("Save data on mobile networks").on_toggle(Message::DataSaver),
                "On mobile data, calls drop their spare audio copy, files are sent without preview pictures and uploads are slowed.",
            ),
            text("Battery").size(18),
            help(if self.battery_saver { "Battery saver is on now." } else { "Battery saver is off now." }),
            setting(
                toggler(settings.keep_listening_in_background).label("Keep waiting for calls in the background").on_toggle(Message::KeepListening),
                "When you switch away from the app while waiting for a call, keep waiting instead of stopping.",
            ),
            setting(
                toggler(settings.listen_on_battery_saver).label("Keep waiting even on battery saver").on_toggle(Message::ListenOnSaver),
                "Normally battery saver stops background waiting, since it keeps the phone awake; this keeps it going anyway.",
            ),
            help("A call or a file already in progress always carries on in the background."),
        ]
        .spacing(10)
        .into()
    }
}

/// One line of reactions for a message, in the fixed order, with counts.
fn reaction_summary(all: &[reactions::Reaction], message_id: u64) -> String {
    reactions::AVAILABLE
        .iter()
        .filter_map(|emoji| {
            let count = all.iter().filter(|r| r.message_id == message_id && r.emoji == *emoji).count();
            match count {
                0 => None,
                1 => Some(reaction_label(emoji)),
                n => Some(format!("{} x{n}", reaction_label(emoji))),
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}
