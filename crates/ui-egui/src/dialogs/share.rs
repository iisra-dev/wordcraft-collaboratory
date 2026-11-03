//! Share Document and Open Shared (WordCraft Collaborative, milestone C4.4): the server, the
//! sign-in token and the document's title or id, then `ui.collab.publish` / `ui.collab.connect`.
//! Participants (C5.5): who has access; the owner finds people by name and invites them, changes
//! what they may do or removes them (`ui.collab.members`, `findPeople`, `invite`, `removeMember`).

use super::chrome::buttons;
use super::{Ui, Value, WordApp, json};
use crate::collab::{ParticipantsForm, ShareForm};

/// Returns true to close.
pub(super) fn share(app: &mut WordApp, ui: &mut Ui, f: &mut ShareForm) -> bool {
    // A sign-in kept in the system credential store fills the token when it is renewed.
    crate::collab::restore_sign_in(app);
    let width = 360.0;
    ui.set_max_width(width);
    let label = ui.label(tl!("Server"));
    let server = ui
        .add(egui::TextEdit::singleline(&mut f.server).hint_text(tl!("wss://server/api/v1/ws")).desired_width(width).char_limit(500))
        .labelled_by(label.id);
    if f.focus {
        server.request_focus();
        f.focus = false;
    }
    let label = ui.label(tl!("Sign-in token"));
    // OIDC access tokens are JWTs, often over a thousand characters.
    ui.add(egui::TextEdit::singleline(&mut f.token).password(true).desired_width(width).char_limit(8192)).labelled_by(label.id);
    ui.collapsing(tl!("Sign in with an identity provider"), |ui| {
        let label = ui.label(tl!("Identity provider"));
        ui.add(
            egui::TextEdit::singleline(&mut f.issuer)
                .hint_text(tl!("https://auth.example.com/application/o/wordcraft/"))
                .desired_width(width)
                .char_limit(2048),
        )
        .labelled_by(label.id);
        let label = ui.label(tl!("Client ID"));
        ui.add(egui::TextEdit::singleline(&mut f.client_id).desired_width(width).char_limit(255)).labelled_by(label.id);
        let waiting = crate::collab::signing_in(app);
        let button = ui.add_enabled(!waiting, egui::Button::new(if waiting { tl!("Waiting for the browser…") } else { tl!("Sign In in Browser") }));
        if button.clicked()
            && let Err(e) = app.run("ui.collab.signIn", json!({"issuer": f.issuer.trim(), "clientId": f.client_id.trim()}))
        {
            f.message = e;
        }
    });
    if f.publish {
        let label = ui.label(tl!("Title"));
        ui.add(egui::TextEdit::singleline(&mut f.title).desired_width(width).char_limit(200)).labelled_by(label.id);
    } else {
        let label = ui.label(tl!("Document ID"));
        ui.add(egui::TextEdit::singleline(&mut f.document).hint_text("0192f7a0-…").desired_width(width).char_limit(64)).labelled_by(label.id);
    }
    ui.add_space(6.0);
    ui.label(tl!("The token stays in memory and is never saved to preferences."));
    ui.label(tl!("Shared documents currently support text and paragraph edits only. Undo and formatting are unavailable."));
    if !f.message.is_empty() {
        ui.colored_label(ui.visuals().error_fg_color, f.message.as_str());
    }
    let (ok, cancel) = buttons(ui, if f.publish { tl!("Share") } else { tl!("Open") });
    if cancel {
        return true;
    }
    if !ok {
        return false;
    }
    let (id, params) = if f.publish {
        ("ui.collab.publish", json!({"server": f.server.trim(), "token": f.token.trim(), "title": f.title.trim()}))
    } else {
        ("ui.collab.connect", json!({"server": f.server.trim(), "token": f.token.trim(), "document": f.document.trim()}))
    };
    match app.run(id, params) {
        Ok(_) => true,
        Err(e) => {
            f.message = e;
            false
        }
    }
}

fn role_label(role: &str) -> &'static str {
    match role {
        "owner" => tl!("Owner"),
        "editor" => tl!("Can edit"),
        "viewer" => tl!("Can view"),
        _ => "",
    }
}

/// Returns true to close.
pub(super) fn participants(app: &mut WordApp, ui: &mut Ui, f: &mut ParticipantsForm) -> bool {
    if !f.loaded {
        f.loaded = true;
        // An error (not shared, a request already waiting) shows below like any other answer.
        let _ = app.run("ui.collab.members", json!({}));
    }
    let view = crate::collab::people(app);
    let width = 400.0;
    ui.set_max_width(width);
    // What the person asked for this frame; run once the widgets are drawn.
    let mut action: Option<(&'static str, Value)> = None;
    if !view.shared {
        ui.label(tl!("Share or open a shared document to see who has access."));
    } else {
        ui.strong(tl!("People with access"));
        if view.members.is_empty() && view.busy {
            ui.label(tl!("Loading…"));
        }
        egui::Grid::new("collab-members").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
            for m in &view.members {
                if m.me {
                    ui.label(format!("{} {}", m.name, tl!("(you)")));
                } else {
                    ui.label(m.name.as_str());
                }
                if view.can_manage && m.role != "owner" {
                    let mut role = m.role;
                    ui.add_enabled_ui(!view.busy, |ui| {
                        craft_ui::ComboBox::from_id_salt(("collab-role", m.id.as_str())).selected_text(role_label(role)).show_ui(ui, |ui| {
                            ui.selectable_value(&mut role, "editor", tl!("Can edit"));
                            ui.selectable_value(&mut role, "viewer", tl!("Can view"));
                        });
                    });
                    if role != m.role {
                        action = Some(("ui.collab.invite", json!({"user": m.id, "role": role})));
                    }
                    if ui.add_enabled(!view.busy, egui::Button::new(tl!("Remove"))).clicked() {
                        action = Some(("ui.collab.removeMember", json!({"user": m.id})));
                    }
                } else {
                    ui.label(role_label(m.role));
                    ui.label("");
                }
                ui.end_row();
            }
        });
        ui.add_space(6.0);
        if view.can_manage {
            ui.separator();
            ui.strong(tl!("Invite people"));
            ui.horizontal(|ui| {
                let label = ui.label(tl!("Name"));
                let edit = ui
                    .add(egui::TextEdit::singleline(&mut f.query).hint_text(tl!("At least two letters")).desired_width(220.0).char_limit(100))
                    .labelled_by(label.id);
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.add_enabled(!view.busy, egui::Button::new(tl!("Search"))).clicked() || (enter && !view.busy) {
                    action = Some(("ui.collab.findPeople", json!({"query": f.query.trim()})));
                }
            });
            ui.horizontal(|ui| {
                ui.label(tl!("Invite as"));
                ui.radio_value(&mut f.role, "editor".to_string(), tl!("Can edit"));
                ui.radio_value(&mut f.role, "viewer".to_string(), tl!("Can view"));
            });
            for p in &view.matches {
                ui.horizontal(|ui| {
                    ui.label(p.name.as_str());
                    if !p.role.is_empty() {
                        ui.weak(tl!("Has access"));
                    } else if ui.add_enabled(!view.busy, egui::Button::new(tl!("Invite"))).clicked() {
                        action = Some(("ui.collab.invite", json!({"user": p.id, "role": f.role})));
                    }
                });
            }
            ui.weak(tl!("People appear here once they have signed in to this server."));
        } else if !view.members.is_empty() {
            ui.weak(tl!("Only the owner can invite people or change access."));
        }
    }
    if let Some((id, params)) = action
        && let Err(e) = app.run(id, params)
    {
        crate::collab::people_failed(app, e);
    }
    let view = crate::collab::people(app);
    if !view.message.is_empty() {
        if view.error {
            ui.colored_label(ui.visuals().error_fg_color, view.message.as_str());
        } else {
            ui.label(view.message.as_str());
        }
    }
    ui.add_space(6.0);
    let mut close = false;
    // One row tall: a bare right-to-left layout would take the window's whole height.
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            close = ui.button(tl!("Close")).clicked();
        });
    });
    close
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    fn harness(name: &str) -> egui_kittest::Harness<'static, WordApp> {
        let mut app = WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::from_text("Private notes")), crate::Services::default());
        assert!(app.run("ui.dialog", json!({"name": name})).is_ok());
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(900.0, 700.0)).build_ui_state(
            |ui, app: &mut WordApp| {
                let ctx = ui.ctx().clone();
                super::super::show(app, &ctx);
            },
            app,
        );
        h.run_steps(4);
        h
    }

    #[test]
    fn share_forms_serialize_and_debug_without_tokens() {
        let mut h = harness("shareOpen");
        let Some(super::super::Dialog::Share(f)) = &mut h.state_mut().dialog else { panic!("share form") };
        f.token = "private-bearer-token".into();
        let value = serde_json::to_value(&f).unwrap();
        assert_eq!(value["hasToken"], true);
        assert!(value.get("token").is_none());
        assert!(!value.to_string().contains("private-bearer-token"));
        assert!(!format!("{f:?}").contains("private-bearer-token"));
        let dump = serde_json::to_string(&h.state().dialog).unwrap();
        assert!(!dump.contains("private-bearer-token"));
    }

    #[test]
    fn participants_on_a_local_document_explain_and_close() {
        let mut h = harness("shareMembers");
        assert!(matches!(h.state().dialog, Some(super::super::Dialog::Participants(_))));
        h.get_by_label("Share or open a shared document to see who has access.");
        // Nothing to load: the request is refused in place, not sent.
        assert!(!crate::collab::people(h.state()).busy);
        // The window's title bar has its own close button; this is the dialog's.
        h.query_all_by_label("Close").last().unwrap().click();
        h.run_steps(3);
        assert!(h.state().dialog.is_none());
    }

    #[test]
    fn share_dialogs_validate_in_place_and_cancel_without_changing_the_document() {
        for (name, action) in [("shareOpen", "Open"), ("sharePublish", "Share")] {
            let mut h = harness(name);
            let before = h.state().session.doc.clone();
            let id = h.state().session.document_id();
            h.get_by_label(action).click();
            h.run_steps(3);
            let Some(super::super::Dialog::Share(f)) = &h.state().dialog else { panic!("invalid form stays open") };
            assert!(!f.message.is_empty());
            assert_eq!(h.state().session.doc, before);
            assert_eq!(h.state().session.document_id(), id);
            h.get_by_label("Cancel").click();
            h.run_steps(3);
            assert!(h.state().dialog.is_none());
            assert_eq!(h.state().session.doc, before);
        }
    }
}
