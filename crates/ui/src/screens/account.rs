//! Settings > Account & backup and Settings > Devices: how not to lose
//! your account.
//!
//! There is no server to sign back in to, so this section is the whole
//! story, in the order people need it (moving lives under Devices, the
//! rest in Account & backup's tabs):
//!
//! 1. **Move to another device** -- the easy path when the old device is
//!    still in hand: a link and QR code, the new device opens it. The
//!    device that sends approves the new one into the account.
//! 2. **Your devices** -- every device on the account, and removing one
//!    that was lost or stolen.
//! 3. **Backup file** -- a passphrase-locked file to keep somewhere safe,
//!    for when the device is lost.
//! 4. **Recovery phrase** -- 24 words on paper, the last resort: it brings
//!    back who you are, not your contacts.
//!
//! Each restore replaces this device's account and asks first (see
//! `dialogs.rs`); nothing here deletes the old one outright.

use iced_core::{Alignment, Color, Length};
use iced_widget::{column, container, qr_code, row};

use crate::icons::Icon;
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, Dialog, El, Model, Msg, Ui, MIN_PASSPHRASE_CHARS};

/// A warning while a backup, restore or move is under way, or nothing.
fn busy_note<'a>(m: &'a Model) -> Option<El<'a, Msg>> {
    m.account.busy.as_ref().map(|busy| warn_box(t(busy.as_str()).size(13.5)))
}

/// The cards of Account & backup's tab `tab` (see
/// [`crate::Section::tabs`]): 0 your identity, 1 backup files, 2 the
/// recovery phrase.
pub fn view<'a>(ui: &'a Ui, m: &'a Model, tab: usize) -> El<'a, Msg> {
    let idle = m.account.busy.is_none();
    let mut cards = column![].spacing(16);
    if let Some(note) = busy_note(m) {
        cards = cards.push(note);
    }
    let cards = match tab {
        1 => cards
            .push(titled_card(
                "Back up to a file",
                Some("Your identity, contacts, profile and settings in one file, locked with a passphrase. Keep it somewhere other than this device."),
                backup(ui, m, idle),
            ))
            .push(titled_card("Restore from a backup file", Some("Replaces this device's account with the one in the file."), import(ui, idle))),
        2 => cards
            .push(titled_card(
                "Recovery phrase",
                Some("24 words that bring back who you are on any device, so your contacts still recognise you. They don't bring back contacts or settings; a backup file does. Anyone who has them can be you."),
                phrase(m),
            ))
            .push(titled_card(
                "Restore from a phrase",
                Some("Type the 24 words, or paste enough recovery shares, one per line. Your contacts and settings here stay."),
                restore(ui),
            )),
        _ => cards.push(identity(m)),
    };
    cards.into()
}

/// The fingerprint of the key that proves who you are, with ways to
/// share it.
fn identity<'a>(m: &'a Model) -> El<'a, Msg> {
    let body = column![
        container(t(m.me.fingerprint.as_str()).size(12.5).font(theme::mono()).color(theme::look().muted)).padding([10, 12]).width(Length::Fill).style(theme::inset),
        row![
            btn_icon(Icon::Copy, "Copy fingerprint", Btn::Default, Some(Msg::Copy(m.me.fingerprint.clone(), "Fingerprint"))),
            btn_icon(Icon::Qr, "Show invite", Btn::Ghost, Some(Msg::Dialog(Some(Dialog::Invite)))),
        ]
        .spacing(8),
    ]
    .spacing(12);
    titled_card("Your identity", Some("Made on this device and never sent anywhere. People who verified you know this fingerprint."), body)
}

/// Settings > Devices: moving this account to another device, and the
/// devices already on it.
pub fn devices<'a>(ui: &'a Ui, m: &'a Model) -> El<'a, Msg> {
    let idle = m.account.busy.is_none();
    let mut cards = column![].spacing(16);
    if let Some(note) = busy_note(m) {
        cards = cards.push(note);
    }
    cards = cards
        .push(titled_card(
            "Move to another device",
            Some("Old device still with you? Open this here, then open the link on the new one. Both need to be on the same Wi-Fi."),
            moving(ui, m, idle),
        ))
        .push(titled_card(
            "Your devices",
            Some("Every device signed in to this account. Each has its own key; removing one stops your other devices sharing with it and signs it out."),
            device_list(m),
        ));
    if let Some(list) = &m.account.mailboxes {
        cards = cards.push(titled_card(
            "Mailboxes for your devices",
            Some("Where your devices leave each other notes, removals and keys, sealed so only each device can read what's meant for it. Use the same ones as your computer's Settings > Delivery."),
            mailboxes(ui, list),
        ));
    }
    cards.into()
}

/// The mailboxes this device shares with the others, each with a way to
/// take it off, and a box to add one.
fn mailboxes<'a>(ui: &'a Ui, list: &'a [String]) -> El<'a, Msg> {
    let mut col = column![].spacing(10);
    for node in list {
        let rest: Vec<String> = list.iter().filter(|n| *n != node).cloned().collect();
        col = col.push(
            row![t(node.as_str()).size(14).font(theme::mono()).width(Length::Fill), btn("Remove", Btn::Ghost, Some(Msg::Do(Action::DeviceMailboxes(rest))))]
                .spacing(10)
                .align_y(Alignment::Center),
        );
    }
    if list.is_empty() {
        col = col.push(faint("None yet. Your devices can't reach each other until you add one.").size(12.5));
    }
    col.push(
        row![
            input("Mailbox address", "stoop-device-mailbox", "host:3479", &ui.mailbox_draft).on_input(Msg::MailboxDraft).on_submit(Msg::MailboxAdd).padding([9, 12]).size(14),
            btn("Add", Btn::Default, (!ui.mailbox_draft.trim().is_empty()).then_some(Msg::MailboxAdd)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .into()
}

/// The history switch, shared by the move and backup cards.
fn history_switch<'a>(ui: &'a Ui) -> El<'a, Msg> {
    setting("Include message history", Some("Bigger, and everything you've said comes along. Off brings only the account."), switch("Include message history", ui.include_history, Msg::IncludeHistory))
}

/// Move: a button to open an offer, or the open offer's code and link.
fn moving<'a>(ui: &'a Ui, m: &'a Model, idle: bool) -> El<'a, Msg> {
    let Some(mv) = &m.account.moving else {
        let receive = column![
            faint("Or, on the new device: paste the link from the old one.").size(12.5),
            row![
                input("Move link from the old device", "stoop-move-link", "stoop-move:…", &ui.receive_link).on_input(Msg::ReceiveLink).padding([9, 12]).size(14),
                btn("Receive", Btn::Default, (idle && !ui.receive_link.trim().is_empty()).then_some(Msg::Dialog(Some(Dialog::ConfirmReceive)))),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(8);
        return column![
            history_switch(ui),
            row![btn_icon(Icon::Qr, "Send this account to a device", Btn::Primary, idle.then_some(Msg::Do(Action::MoveStart { history: ui.include_history })))],
            divider(),
            receive,
        ]
        .spacing(12)
        .into();
    };
    let code: El<'a, Msg> = match mv.qr.as_deref() {
        Some(data) => container(qr_code(data).cell_size(4).style(|_| qr_code::Style { cell: Color::from_rgb8(22, 22, 23), background: Color::WHITE }))
            .padding(12)
            .style(|_| container::Style {
                background: Some(iced_core::Background::Color(Color::WHITE)),
                border: iced_core::Border { radius: 14.0.into(), ..Default::default() },
                ..Default::default()
            })
            .into(),
        None => container(faint("Making the code…")).center(180).style(theme::inset).into(),
    };
    let mut col = column![t(mv.status.as_str()).size(14).font(theme::semibold())].spacing(12);
    if !mv.done {
        col = col
            .push(code)
            .push(container(t(mv.link.as_str()).size(12).font(theme::mono())).padding([8, 10]).width(Length::Fill).style(theme::inset))
            .push(faint("This link is your whole account until it closes in 10 minutes. Only open it on your own device.").size(12.5));
    }
    let mut buttons = row![].spacing(8);
    if !mv.done {
        buttons = buttons.push(btn_icon(Icon::Copy, "Copy link", Btn::Default, Some(Msg::Copy(mv.link.clone(), "Move link"))));
    }
    buttons = buttons.push(btn(if mv.done { "Done" } else { "Stop" }, Btn::Ghost, Some(Msg::Do(Action::MoveStop))));
    col.push(buttons).into()
}

/// The account's devices, each with a way to remove it (but this one).
fn device_list<'a>(m: &'a Model) -> El<'a, Msg> {
    let a = &m.account;
    let mut col = column![].spacing(10);
    if a.devices.is_empty() {
        col = col.push(faint("Your device list hasn't loaded.").size(13));
    }
    for d in &a.devices {
        let mut name = row![t(d.name.as_str()).size(14).font(theme::semibold())].spacing(8).align_y(Alignment::Center);
        if d.this {
            name = name.push(faint("This device").size(12));
        }
        let info = column![name, faint(format!("{} · {}", d.added, d.fingerprint)).size(12)].spacing(2).width(Length::Fill);
        let mut r = row![info].spacing(10).align_y(Alignment::Center);
        if !d.this && a.can_remove_devices {
            r = r.push(btn("Remove", Btn::Ghost, Some(Msg::Dialog(Some(Dialog::ConfirmRevokeDevice(d.id.clone()))))));
        }
        col = col.push(r);
    }
    if !a.device_sync.is_empty() {
        col = col.push(divider()).push(faint(a.device_sync.as_str()).size(12.5));
    }
    col.into()
}

/// Backup: the passphrase twice, the history switch, Save.
fn backup<'a>(ui: &'a Ui, m: &'a Model, idle: bool) -> El<'a, Msg> {
    let save = if m.account.can_pick_file { "Save backup file…" } else { "Save to Downloads" };
    let ready = idle && !ui.backup_pass.is_empty() && !ui.backup_pass2.is_empty();
    column![
        field(
            "Passphrase",
            secret_input("Backup passphrase", "stoop-backup-pass", passphrase_hint(), &ui.backup_pass).on_input(Msg::BackupPass).padding([9, 12]).size(14)
        ),
        field(
            "Type it again",
            secret_input("Type the passphrase again", "stoop-backup-pass2", "", &ui.backup_pass2).on_input(Msg::BackupPass2).on_submit(Msg::BackupSave).padding([9, 12]).size(14)
        ),
        error_box(&ui.backup_error),
        history_switch(ui),
        faint("Nobody can reset this passphrase, us included. Forget it and the file can't be opened.").size(12.5),
        row![btn_icon(Icon::Download, save, Btn::Primary, ready.then_some(Msg::BackupSave))],
    ]
    .spacing(12)
    .into()
}

/// The placeholder in the passphrase box.
fn passphrase_hint() -> &'static str {
    const _: () = assert!(MIN_PASSPHRASE_CHARS == 8, "update the hint below");
    "At least 8 characters"
}

/// Import: the passphrase, then choose the file (after the dialog asks).
fn import<'a>(ui: &'a Ui, idle: bool) -> El<'a, Msg> {
    column![
        field(
            "Passphrase",
            secret_input("Backup passphrase to import", "stoop-import-pass", "What the backup was locked with", &ui.import_pass).on_input(Msg::ImportPass).padding([9, 12]).size(14)
        ),
        row![btn_icon(Icon::File, "Choose backup file…", Btn::Danger, (idle && !ui.import_pass.is_empty()).then_some(Msg::Dialog(Some(Dialog::ConfirmImport))))],
    ]
    .spacing(12)
    .into()
}

/// The phrase, hidden behind a button until asked for.
fn phrase<'a>(m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    match m.account.phrase.as_deref() {
        Some(words) => column![
            container(t(words).size(13.5).font(theme::mono()).color(p.text)).padding([10, 12]).width(Length::Fill).style(theme::inset),
            row![
                btn_icon(Icon::Copy, "Copy", Btn::Default, Some(Msg::Copy(words.to_string(), "Recovery phrase"))),
                btn("Hide", Btn::Ghost, Some(Msg::Do(Action::ShowPhrase(false)))),
            ]
            .spacing(8),
        ]
        .spacing(10)
        .into(),
        None => row![btn_icon(Icon::Lock, "Show my recovery phrase", Btn::Default, Some(Msg::Do(Action::ShowPhrase(true))))].into(),
    }
}

/// The restore-from-phrase box.
fn restore<'a>(ui: &'a Ui) -> El<'a, Msg> {
    let ready = !ui.restore_edit.trim().is_empty();
    column![
        input("Recovery phrase to restore", "stoop-restore", "The 24 words, in order", &ui.restore_edit).on_input(Msg::RestoreEdit).padding([10, 12]).size(14),
        row![btn("Restore", Btn::Danger, ready.then_some(Msg::Dialog(Some(Dialog::ConfirmRestore))))],
    ]
    .spacing(10)
    .into()
}
