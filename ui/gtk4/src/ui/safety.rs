use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use serde_json::json;

use crate::ffi::Core;
use crate::model::{Requirement, SafetyDecision};
use crate::ui::editing::confirm;

/// Performs the ceremony a decision asks for; `on_cleared` runs only after the engine granted it.
pub fn clear_challenge(
    parent: &gtk::Widget,
    core: Core,
    decision: SafetyDecision,
    on_cleared: impl Fn() + 'static,
    on_refused: impl Fn(&str) + 'static,
) {
    let on_cleared: Rc<dyn Fn()> = Rc::new(on_cleared);
    let on_refused: Rc<dyn Fn(&str)> = Rc::new(on_refused);
    let Some(challenge) = decision.challenge.clone() else {
        return on_cleared();
    };
    match decision.requires {
        Requirement::None => on_cleared(),
        Requirement::Warn => {
            let (parent2, core, cleared, refused) =
                (parent.clone(), core.clone(), on_cleared, on_refused);
            let profile = decision.profile.clone();
            let dialog = confirm(&decision.heading(), &decision.body(), "Run", move || {
                satisfy(
                    core.clone(),
                    profile.clone(),
                    challenge.clone(),
                    json!({"kind": "acknowledged"}).to_string(),
                    cleared.clone(),
                    {
                        let refused = refused.clone();
                        move |why| refused(&why)
                    },
                );
            });
            dialog.present(Some(&parent2));
        }
        Requirement::Authenticate => {
            let (parent, core) = (parent.clone(), core.clone());
            glib::spawn_future_local(async move {
                // polkit first where the desktop offers it; the typed phrase everywhere else.
                match crate::auth::system_auth().await {
                    Some(method) => satisfy(
                        core.clone(),
                        decision.profile.clone(),
                        challenge.clone(),
                        json!({"kind": "system_auth", "method": method}).to_string(),
                        on_cleared.clone(),
                        move |why| {
                            typed_phrase_dialog(
                                parent.clone(),
                                core.clone(),
                                decision.clone(),
                                Some(why),
                                on_cleared.clone(),
                                on_refused.clone(),
                            )
                        },
                    ),
                    None => {
                        typed_phrase_dialog(parent, core, decision, None, on_cleared, on_refused)
                    }
                }
            });
        }
    }
}

fn typed_phrase_dialog(
    parent: gtk::Widget,
    core: Core,
    decision: SafetyDecision,
    hint: Option<String>,
    on_cleared: Rc<dyn Fn()>,
    on_refused: Rc<dyn Fn(&str)>,
) {
    let Some(challenge) = decision.challenge.clone() else {
        return on_cleared();
    };
    let dialog = adw::AlertDialog::new(Some(&decision.heading()), None);
    dialog.set_body(&format!(
        "{} Type the connection name to continue.",
        decision.body()
    ));

    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("Connection name"));
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    column.append(&entry);
    if let Some(hint) = &hint {
        let refused = gtk::Label::new(Some(hint));
        refused.add_css_class("error");
        refused.set_wrap(true);
        refused.set_xalign(0.0);
        column.append(&refused);
    }
    dialog.set_extra_child(Some(&column));

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("authenticate", "Authenticate");
    dialog.set_response_appearance("authenticate", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("authenticate", false);
    let watched = dialog.clone();
    entry.connect_changed(move |entry| {
        watched.set_response_enabled("authenticate", !entry.text().trim().is_empty());
    });

    let anchor = parent.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "authenticate" {
            return;
        }
        let typed = entry.text().trim().to_string();
        let (parent, core2, decision, cleared, refused) = (
            anchor.clone(),
            core.clone(),
            decision.clone(),
            on_cleared.clone(),
            on_refused.clone(),
        );
        satisfy(
            core.clone(),
            decision.profile.clone(),
            challenge.clone(),
            json!({"kind": "typed_phrase", "typed": typed}).to_string(),
            cleared.clone(),
            // A wrong phrase re-asks with the engine's verdict; the dialog never lies about why.
            move |why| {
                typed_phrase_dialog(
                    parent.clone(),
                    core2.clone(),
                    decision.clone(),
                    Some(why),
                    cleared.clone(),
                    refused.clone(),
                )
            },
        );
    });
    dialog.present(Some(&parent));
}

fn satisfy(
    core: Core,
    profile: String,
    challenge: String,
    attestation: String,
    on_cleared: Rc<dyn Fn()>,
    on_refused: impl Fn(String) + 'static,
) {
    glib::spawn_future_local(async move {
        let judged =
            gio::spawn_blocking(move || core.safety_satisfy(&profile, &challenge, &attestation))
                .await;
        match judged {
            Ok(Ok(())) => on_cleared(),
            Ok(Err(error)) => on_refused(error.0),
            Err(_) => on_refused("the authentication check never finished".to_owned()),
        }
    });
}
