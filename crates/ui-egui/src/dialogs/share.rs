//! Share Document and Open Shared (WordCraft Collaborative, milestone C4.4): the server, the
//! sign-in token and the document's title or id, then `ui.collab.publish` / `ui.collab.connect`.

use super::chrome::buttons;
use super::{Ui, WordApp, json};
use crate::collab::ShareForm;

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
