use std::collections::HashMap;

use glib::prelude::*;

pub const POLKIT_ACTION: &str = "io.github.chud_lori.datagrep.run-guarded-statement";

/// `Some(method)` only when the system authenticated the user; `None` means fall back to the typed phrase.
pub async fn system_auth() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    polkit_auth().await
}

// Any failure — no system bus, polkit absent, action not installed, refusal — lands on None.
async fn polkit_auth() -> Option<String> {
    let bus = gio::bus_get_future(gio::BusType::System).await.ok()?;
    let unique = bus.unique_name()?;
    let mut subject = HashMap::<String, glib::Variant>::new();
    subject.insert("name".into(), unique.as_str().to_variant());
    let params = (
        ("system-bus-name", subject),
        POLKIT_ACTION,
        HashMap::<String, String>::new(),
        1u32, // ALLOW_USER_INTERACTION: the desktop's agent puts up its own prompt
        "",
    );
    let reply = bus
        .call_future(
            Some("org.freedesktop.PolicyKit1"),
            "/org/freedesktop/PolicyKit1/Authority",
            "org.freedesktop.PolicyKit1.Authority",
            "CheckAuthorization",
            Some(&params.to_variant()),
            Some(glib::VariantTy::new("((bba{ss}))").ok()?),
            gio::DBusCallFlags::NONE,
            120_000,
        )
        .await
        .ok()?;
    let authorized = reply.child_value(0).child_value(0).get::<bool>()?;
    authorized.then(|| "polkit".to_string())
}
