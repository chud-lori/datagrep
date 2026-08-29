use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::model::update::{self, UpdateCheck};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct UpdateNotice {
        pub label: gtk::Label,
        pub version: RefCell<String>,
        pub url: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for UpdateNotice {
        const NAME: &'static str = "DgUpdateNotice";
        type Type = super::UpdateNotice;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for UpdateNotice {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.add_css_class("dg-update-notice");
            obj.set_visible(false);

            let view = gtk::Button::with_label("View release");
            view.add_css_class("flat");
            let notice = obj.downgrade();
            view.connect_clicked(move |_| {
                if let Some(notice) = notice.upgrade() {
                    notice.open_release();
                }
            });

            let menu = gio::Menu::new();
            menu.append(Some("Skip This Version"), Some("notice.skip"));
            menu.append(Some("Turn Off Update Checks"), Some("notice.mute"));
            let more = gtk::MenuButton::builder()
                .icon_name("view-more-symbolic")
                .menu_model(&menu)
                .tooltip_text("Skip this version, or turn update checks off")
                .build();
            more.add_css_class("flat");

            let dismiss = gtk::Button::from_icon_name("window-close-symbolic");
            dismiss.add_css_class("flat");
            dismiss.set_tooltip_text(Some("Dismiss (until the next launch)"));
            let notice = obj.downgrade();
            dismiss.connect_clicked(move |_| {
                if let Some(notice) = notice.upgrade() {
                    notice.set_visible(false);
                }
            });

            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            self.label.set_hexpand(true);
            self.label.set_xalign(0.0);
            row.append(&self.label);
            row.append(&view);
            row.append(&more);
            row.append(&dismiss);
            obj.set_child(Some(&row));

            let actions = gio::SimpleActionGroup::new();
            let skip = gio::SimpleAction::new("skip", None);
            let notice = obj.downgrade();
            skip.connect_activate(move |_, _| {
                if let Some(notice) = notice.upgrade() {
                    UpdateCheck::skip(&notice.imp().version.borrow());
                    notice.set_visible(false);
                }
            });
            actions.add_action(&skip);
            let mute = gio::SimpleAction::new("mute", None);
            let notice = obj.downgrade();
            mute.connect_activate(move |_, _| {
                UpdateCheck::set_check_on_launch_enabled(false);
                if let Some(notice) = notice.upgrade() {
                    notice.set_visible(false);
                }
            });
            actions.add_action(&mute);
            obj.insert_action_group("notice", Some(&actions));
        }
    }

    impl WidgetImpl for UpdateNotice {}
    impl BinImpl for UpdateNotice {}
}

glib::wrapper! {
    pub struct UpdateNotice(ObjectSubclass<imp::UpdateNotice>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl UpdateNotice {
    pub fn new(check: &UpdateCheck) -> Self {
        let notice: Self = glib::Object::new();
        let weak = notice.downgrade();
        check.connect_update_available(move |_, version, url| {
            if let Some(notice) = weak.upgrade() {
                notice.show_release(version, url);
            }
        });
        notice
    }

    fn show_release(&self, version: &str, url: &str) {
        let imp = self.imp();
        imp.version.replace(version.to_string());
        imp.url.replace(url.to_string());
        imp.label.set_text(&format!(
            "datagrep {} is available",
            update::normalize(version)
        ));
        self.set_visible(true);
    }

    fn open_release(&self) {
        let url = self.imp().url.borrow().clone();
        let url = if url.is_empty() {
            update::RELEASES_URL.to_string()
        } else {
            url
        };
        gtk::UriLauncher::new(&url).launch(
            self.root().and_downcast_ref::<gtk::Window>(),
            gio::Cancellable::NONE,
            |_| {},
        );
    }
}
