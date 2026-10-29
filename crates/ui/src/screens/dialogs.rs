//! The dialogs: your invite, adding someone, and the two are-you-sures.

use iced_core::text::Wrapping;
use iced_core::{Alignment, Color, Length};
use iced_widget::{column, container, qr_code, row};

use super::clip;
use crate::icons::{icon, Icon};
use crate::theme::{self, Btn};
use crate::widgets::*;
use crate::{Action, Dialog, El, Model, Msg, Ui};

/// Draws dialog `d` as a card over a dimmed window; `rise` (0 to 1) slides it in.
pub fn view<'a>(ui: &'a Ui, m: &'a Model, d: &'a Dialog, rise: f32) -> El<'a, Msg> {
    let close = Msg::Dialog(None);
    match d {
        Dialog::Invite => modal("Your invite", invite(m), None, 520.0, close, rise),
        Dialog::AddPerson => {
            let footer = row![btn("Cancel", Btn::Ghost, Some(Msg::Dialog(None))), btn("Add", Btn::Primary, Some(Msg::AddSubmit))].spacing(8);
            modal("Add someone", add(ui), Some(footer.into()), 500.0, close, rise)
        }
        Dialog::ConfirmRemove(id) => {
            let name = m.people.iter().find(|p| &p.id == id).map(|p| p.name.as_str()).unwrap_or("them");
            let footer = row![btn("Cancel", Btn::Ghost, Some(Msg::Dialog(None))), btn("Remove", Btn::DangerSolid, Some(Msg::Do(Action::Remove(id.clone()))))].spacing(8);
            modal(
                "Remove from your people?",
                muted(format!("{name} and your history with them leave this device. They can still add you again from your invite.")).size(14),
                Some(footer.into()),
                460.0,
                close,
                rise,
            )
        }
        Dialog::ConfirmBlock(id) => {
            let name = m.people.iter().find(|p| &p.id == id).map(|p| p.name.as_str()).unwrap_or("them");
            let footer = row![btn("Cancel", Btn::Ghost, Some(Msg::Dialog(None))), btn("Block", Btn::DangerSolid, Some(Msg::Do(Action::Block(id.clone()))))].spacing(8);
            modal(
                "Block them?",
                muted(format!("{name} will not be able to call or message you, even with your invite. This device turns them away; nobody else is told.")).size(14),
                Some(footer.into()),
                460.0,
                close,
                rise,
            )
        }
        Dialog::ConfirmRestore => {
            let phrase = ui.restore_edit.trim().to_string();
            let footer = row![btn("Cancel", Btn::Ghost, Some(Msg::Dialog(None))), btn("Replace", Btn::DangerSolid, Some(Msg::Do(Action::Restore(phrase))))].spacing(8);
            modal(
                "Replace this identity?",
                muted("This device becomes the identity the phrase restores. The one it has now is gone unless you saved its phrase, and people who verified it will see a new key.").size(14),
                Some(footer.into()),
                460.0,
                close,
                rise,
            )
        }
        Dialog::ConfirmImport => {
            let open = Action::BackupOpen { passphrase: ui.import_pass.clone() };
            let footer = row![btn("Cancel", Btn::Ghost, Some(Msg::Dialog(None))), btn("Choose file and replace", Btn::DangerSolid, Some(Msg::Do(open)))].spacing(8);
            modal(
                "Replace this account?",
                muted("This device becomes the account in the backup: its identity, contacts, profile and settings. What is here now is moved into a \"replaced\" folder beside the app's data, not deleted. Stoop restarts when it's done.").size(14),
                Some(footer.into()),
                480.0,
                close,
                rise,
            )
        }
        Dialog::NewSpace => {
            let footer = row![btn("Cancel", Btn::Ghost, Some(Msg::Dialog(None))), btn("Start it", Btn::Primary, (!ui.space_name.trim().is_empty()).then_some(Msg::SpaceCreate))].spacing(8);
            modal(if ui.space_group { "Start a group chat" } else { "Start a community" }, super::community::new_space(ui), Some(footer.into()), 480.0, close, rise)
        }
        Dialog::EmojiPicker(id) => modal("Emoji", super::community::emoji_picker(m, id), None, 420.0, close, rise),
        Dialog::SpaceEmoji(id) => modal("Custom emoji", super::community::emoji_manager(m, id), None, 460.0, close, rise),
        Dialog::SpaceNotify(id) => modal("Notifications", super::community::notify(m, id), None, 520.0, close, rise),
        Dialog::InviteToSpace(id) => modal("Invite someone", super::community::invite(m, id), None, 460.0, close, rise),
        Dialog::AddChannel(id) => {
            let footer = row![btn("Cancel", Btn::Ghost, Some(Msg::Dialog(None))), btn("Add channel", Btn::Primary, Some(Msg::ChannelCreate(id.clone())))].spacing(8);
            modal("Add a channel", super::community::add_channel(ui, id), Some(footer.into()), 440.0, close, rise)
        }
        Dialog::ChannelSettings(id, channel) => modal("Who can read it", super::community::channel_settings(m, id, *channel), None, 480.0, close, rise),
        Dialog::Roles(id) => modal("Roles", super::community::roles(ui, m, id), None, 560.0, close, rise),
        Dialog::Moderation(id) => modal("Moderation", super::community::moderation(m, id), None, 600.0, close, rise),
        Dialog::Report(id, channel, _) => {
            let send = (!ui.report_pick.is_empty()).then(|| Msg::ReportSend(id.clone(), *channel));
            let footer = row![btn("Cancel", Btn::Ghost, Some(Msg::Dialog(None))), btn("Report", Btn::DangerSolid, send)].spacing(8);
            modal("Report to the moderators", super::community::report(ui, m, id, *channel), Some(footer.into()), 520.0, close, rise)
        }
        Dialog::ConfirmLeave(id) => {
            let name = m.communities.iter().find(|c| &c.id == id).map(|c| c.name.as_str()).unwrap_or("it");
            let footer = row![btn("Cancel", Btn::Ghost, Some(Msg::Dialog(None))), btn("Leave", Btn::DangerSolid, Some(Msg::Do(Action::LeaveSpace(id.clone()))))].spacing(8);
            modal(
                "Leave?",
                muted(format!("You stop getting {name}'s messages, and its history leaves this device. Someone in it can invite you back.")).size(14),
                Some(footer.into()),
                460.0,
                close,
                rise,
            )
        }
        Dialog::ConfirmReceive => {
            let receive = Action::MoveReceive(ui.receive_link.trim().to_string());
            let footer = row![btn("Cancel", Btn::Ghost, Some(Msg::Dialog(None))), btn("Receive and replace", Btn::DangerSolid, Some(Msg::Do(receive)))].spacing(8);
            modal(
                "Bring your account here?",
                muted("This device becomes the account your other device is sending: its identity, contacts, profile and settings. What is here now is moved into a \"replaced\" folder, not deleted. Stoop restarts when it's done.").size(14),
                Some(footer.into()),
                480.0,
                close,
                rise,
            )
        }
    }
}

/// The QR code on white (so any camera reads it), the link, and the ways
/// to share it.
fn invite<'a>(m: &'a Model) -> El<'a, Msg> {
    let p = theme::look();
    let code: El<'a, Msg> = match m.me.qr.as_deref() {
        Some(data) => container(qr_code(data).cell_size(5).style(|_| qr_code::Style { cell: Color::from_rgb8(22, 22, 23), background: Color::WHITE }))
            .padding(14)
            .style(|_| iced_widget::container::Style {
                background: Some(iced_core::Background::Color(Color::WHITE)),
                border: iced_core::Border { radius: 16.0.into(), ..Default::default() },
                ..Default::default()
            })
            .into(),
        None => container(faint("Making your invite…")).center(220).style(theme::inset).into(),
    };
    let link = container(
        row![
            icon(Icon::Link, 15.0, p.faint),
            t(clip(&m.me.invite, 44)).size(12.5).font(theme::mono()).color(p.muted).wrapping(Wrapping::None).width(Length::Fill),
            btn_icon(Icon::Copy, "Copy", Btn::Primary, (!m.me.invite.is_empty()).then(|| Msg::Copy(m.me.invite.clone(), "Invite link"))),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding(pad(6.0, 6.0, 6.0, 12.0))
    .style(theme::composer);
    column![
        muted("Anyone who scans this or opens the link can add you. They still need your door open to call.").size(13.5),
        container(code).width(Length::Fill).center_x(Length::Fill).padding([6, 0]),
        link,
        row![
            faint("Shared it too widely? A new link stops new people using the old one.").size(12).width(Length::Fill),
            btn_icon(Icon::Refresh, "New link", Btn::Ghost, Some(Msg::Do(Action::RefreshInvite))),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    ]
    .spacing(14)
    .into()
}

/// Paste an invite (or an address) and give them a name.
fn add<'a>(ui: &'a Ui) -> El<'a, Msg> {
    let looks_like_address = {
        let s = ui.add_invite.trim();
        !s.is_empty() && !s.contains("://") && s.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    };
    let mut col = column![
        muted("Paste the invite link they sent you. Their name comes with it, or give them one of your own.").size(13.5),
        field(
            "Invite link",
            input("Invite link", "stoop-add-invite", "stoop://… or antiphon://…", &ui.add_invite).on_input(Msg::AddInvite).on_submit(Msg::AddSubmit).padding([10, 12]).size(14)
        ),
        field("Name (optional)", input("Name (optional)", "stoop-add-name", "What you call them", &ui.add_name).on_input(Msg::AddName).on_submit(Msg::AddSubmit).padding([10, 12]).size(14)),
    ]
    .spacing(14);
    if !ui.add_error.is_empty() {
        col = col.push(error_box(&ui.add_error));
    }
    if looks_like_address {
        col = col.push(warn_box(
            row![
                t("That looks like an address, not an invite.").size(13).width(Length::Fill),
                btn_icon(Icon::Phone, "Call it", Btn::Default, Some(Msg::AddCall)),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        ));
    }
    col.into()
}
