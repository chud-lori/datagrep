use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::ffi::Core;
use crate::model::{CatalogNode, Enumeration};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    Object,
    /// A level costly enough that the arrow alone must not enumerate it.
    Consent,
    Notice,
}

mod node_imp {
    use super::*;

    #[derive(Default)]
    pub struct SchemaNode {
        pub name: RefCell<String>,
        pub icon: Cell<&'static str>,
        pub path: RefCell<Vec<String>>,
        pub path_json: RefCell<String>,
        pub browsable: Cell<bool>,
        pub enumeration: Cell<Enumeration>,
        pub role: Cell<Role>,
        pub children: RefCell<Option<gio::ListStore>>,
        pub expandable: Cell<bool>,
        pub loaded: Cell<bool>,
        pub consented: Cell<bool>,
        pub described: Cell<bool>,
        pub describe_json: RefCell<String>,
        pub describe_error: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SchemaNode {
        const NAME: &'static str = "DgSchemaNode";
        type Type = super::SchemaNode;
    }

    impl ObjectImpl for SchemaNode {}
}

glib::wrapper! {
    pub struct SchemaNode(ObjectSubclass<node_imp::SchemaNode>);
}

impl SchemaNode {
    pub fn object(parent: &[String], node: &CatalogNode) -> Self {
        let mut path = parent.to_vec();
        path.push(node.name.clone());
        let this: Self = glib::Object::new();
        let imp = this.imp();
        *imp.name.borrow_mut() = node.name.clone();
        imp.icon.set(node.icon_name());
        *imp.path_json.borrow_mut() = encode_path(&path);
        *imp.path.borrow_mut() = path;
        imp.enumeration.set(node.enumeration);
        imp.expandable.set(node.has_children);
        imp.browsable.set(CatalogNode::browsable_kind(&node.kind));
        this
    }

    fn leaf(role: Role, icon: &'static str, name: &str) -> Self {
        let this: Self = glib::Object::new();
        let imp = this.imp();
        *imp.name.borrow_mut() = name.to_owned();
        imp.icon.set(icon);
        *imp.path_json.borrow_mut() = "[]".to_owned();
        imp.role.set(role);
        this
    }

    pub fn notice(text: &str) -> Self {
        Self::leaf(Role::Notice, "dialog-information-symbolic", text)
    }

    pub fn consent() -> Self {
        Self::leaf(
            Role::Consent,
            "dialog-warning-symbolic",
            "Listing this level scans the whole keyspace — click to list it anyway",
        )
    }

    pub fn name(&self) -> String {
        self.imp().name.borrow().clone()
    }

    pub fn icon(&self) -> &'static str {
        self.imp().icon.get()
    }

    pub fn role(&self) -> Role {
        self.imp().role.get()
    }

    pub fn path_json(&self) -> String {
        self.imp().path_json.borrow().clone()
    }

    /// Whether activating this node has rows to open — a schema or a Redis key has not.
    pub fn browsable(&self) -> bool {
        self.imp().browsable.get()
    }

    /// Empty until the row is expanded, so drawing an arrow costs no catalog call.
    pub fn children_store(&self) -> Option<gio::ListStore> {
        let imp = self.imp();
        if !imp.expandable.get() {
            return None;
        }
        let mut children = imp.children.borrow_mut();
        Some(
            children
                .get_or_insert_with(gio::ListStore::new::<SchemaNode>)
                .clone(),
        )
    }
}

fn encode_path(path: &[String]) -> String {
    serde_json::to_string(path).unwrap_or_else(|_| "[]".to_owned())
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct SchemaTree {
        pub core: RefCell<Option<Arc<Core>>>,
        pub profile: RefCell<String>,
        pub roots: gio::ListStore,
        pub view: gtk::ListView,
        pub selection: gtk::SingleSelection,
        pub watched: RefCell<HashMap<gtk::ListItem, glib::SignalHandlerId>>,
        // Bumped whenever the tree changes what it is showing, so an answer that
        // arrives for a connection nobody is looking at any more is dropped.
        pub generation: Cell<u64>,
    }

    impl Default for SchemaTree {
        fn default() -> Self {
            let roots = gio::ListStore::new::<SchemaNode>();
            let tree = gtk::TreeListModel::new(roots.clone(), false, false, |item| {
                item.downcast_ref::<SchemaNode>()?
                    .children_store()
                    .map(Cast::upcast)
            });
            let selection = gtk::SingleSelection::new(Some(tree));
            Self {
                core: RefCell::new(None),
                profile: RefCell::new(String::new()),
                roots,
                view: gtk::ListView::new(Some(selection.clone()), None::<gtk::ListItemFactory>),
                selection,
                watched: RefCell::new(HashMap::new()),
                generation: Cell::new(0),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SchemaTree {
        const NAME: &'static str = "DgSchemaTree";
        type Type = super::SchemaTree;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for SchemaTree {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("object-activated")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    Signal::builder("object-described")
                        .param_types([
                            String::static_type(),
                            String::static_type(),
                            String::static_type(),
                        ])
                        .build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.view.add_css_class("navigation-sidebar");
            self.view.set_factory(Some(&self.factory()));

            let tree = self.obj().downgrade();
            self.selection
                .connect_selected_item_notify(move |selection| {
                    if let Some(tree) = tree.upgrade() {
                        tree.imp().on_selected(selection);
                    }
                });

            let tree = self.obj().downgrade();
            self.view.connect_activate(move |_, position| {
                if let Some(tree) = tree.upgrade() {
                    tree.imp().on_activate(position);
                }
            });

            self.obj().set_child(Some(
                &gtk::ScrolledWindow::builder()
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .vexpand(true)
                    .child(&self.view)
                    .build(),
            ));
        }
    }

    impl WidgetImpl for SchemaTree {}
    impl BinImpl for SchemaTree {}

    impl SchemaTree {
        fn factory(&self) -> gtk::SignalListItemFactory {
            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(|_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                row.append(&gtk::Image::new());
                row.append(&gtk::Inscription::builder().hexpand(true).build());
                let expander = gtk::TreeExpander::new();
                expander.set_child(Some(&row));
                item.set_child(Some(&expander));
            });

            let tree = self.obj().downgrade();
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let (Some(expander), Some(row)) = (
                    item.child().and_downcast::<gtk::TreeExpander>(),
                    item.item().and_downcast::<gtk::TreeListRow>(),
                ) else {
                    return;
                };
                expander.set_list_row(Some(&row));
                let Some(node) = row.item().and_downcast::<SchemaNode>() else {
                    return;
                };
                if let Some(content) = expander.child().and_downcast::<gtk::Box>() {
                    if let Some(icon) = content.first_child().and_downcast::<gtk::Image>() {
                        icon.set_icon_name(Some(node.icon()));
                    }
                    if let Some(label) = content.last_child().and_downcast::<gtk::Inscription>() {
                        label.set_text(Some(&node.name()));
                        label.set_text_overflow(gtk::InscriptionOverflow::EllipsizeEnd);
                    }
                }
                item.set_activatable(node.role() != Role::Notice);

                let Some(tree) = tree.upgrade() else {
                    return;
                };
                let owner = tree.downgrade();
                let handler = row.connect_expanded_notify(move |row| {
                    if !row.is_expanded() {
                        return;
                    }
                    if let (Some(tree), Some(node)) =
                        (owner.upgrade(), row.item().and_downcast::<SchemaNode>())
                    {
                        tree.imp().load(&node);
                    }
                });
                tree.imp()
                    .watched
                    .borrow_mut()
                    .insert(item.clone(), handler);
                if row.is_expanded() {
                    tree.imp().load(&node);
                }
            });

            let tree = self.obj().downgrade();
            factory.connect_unbind(move |_, item| {
                let (Some(tree), Some(item)) =
                    (tree.upgrade(), item.downcast_ref::<gtk::ListItem>())
                else {
                    return;
                };
                let handler = tree.imp().watched.borrow_mut().remove(item);
                if let (Some(handler), Some(row)) =
                    (handler, item.item().and_downcast::<gtk::TreeListRow>())
                {
                    row.disconnect(handler);
                }
            });
            factory
        }

        fn on_activate(&self, position: u32) {
            let Some(row) = self
                .selection
                .item(position)
                .and_downcast::<gtk::TreeListRow>()
            else {
                return;
            };
            let Some(node) = row.item().and_downcast::<SchemaNode>() else {
                return;
            };
            match node.role() {
                Role::Consent => self.consent(&row),
                Role::Notice => (),
                Role::Object if node.browsable() => self.obj().emit_by_name::<()>(
                    "object-activated",
                    &[&node.path_json(), &node.name()],
                ),
                Role::Object => (),
            }
        }

        /// Describes once per node — a remembered failure cannot loop on the engine.
        fn on_selected(&self, selection: &gtk::SingleSelection) {
            let selected = selection
                .selected_item()
                .and_downcast::<gtk::TreeListRow>()
                .and_then(|row| row.item())
                .and_downcast::<SchemaNode>()
                .filter(|node| node.role() == Role::Object);
            let Some(node) = selected else {
                self.obj()
                    .emit_by_name::<()>("object-described", &[&"", &"", &""]);
                return;
            };
            if node.imp().described.replace(true) {
                return self.announce(&node);
            }
            // An empty detail with no error is the reading state, not a failure.
            self.obj()
                .emit_by_name::<()>("object-described", &[&node.path_json(), &"", &""]);
            self.describe(&node);
        }

        fn announce(&self, node: &SchemaNode) {
            let imp = node.imp();
            self.obj().emit_by_name::<()>(
                "object-described",
                &[
                    &node.path_json(),
                    &*imp.describe_json.borrow(),
                    &*imp.describe_error.borrow(),
                ],
            );
        }

        /// Off the main loop: a describe is a round trip, and the window must
        /// stay answerable while it is in flight.
        fn describe(&self, node: &SchemaNode) {
            let Some(core) = self.core.borrow().clone() else {
                return;
            };
            let (profile, path_json) = (self.profile.borrow().clone(), node.path_json());
            let (tree, node) = (self.obj().downgrade(), node.clone());
            let generation = self.generation.get();
            glib::spawn_future_local(async move {
                let described = gio::spawn_blocking(move || {
                    core.catalog_describe_json(&profile, &path_json)
                        .map_err(|e| e.0)
                })
                .await;
                let Some(tree) = tree.upgrade() else {
                    return;
                };
                let imp = node.imp();
                match described {
                    Ok(Ok(json)) => *imp.describe_json.borrow_mut() = json,
                    Ok(Err(error)) => *imp.describe_error.borrow_mut() = error,
                    Err(_) => *imp.describe_error.borrow_mut() =
                        "the describe did not finish".to_owned(),
                }
                // Only the selection this answer is about gets to redraw the inspector.
                if tree.imp().generation.get() == generation && tree.imp().is_selected(&node) {
                    tree.imp().announce(&node);
                }
            });
        }

        fn is_selected(&self, node: &SchemaNode) -> bool {
            self.selection
                .selected_item()
                .and_downcast::<gtk::TreeListRow>()
                .and_then(|row| row.item())
                .and_downcast::<SchemaNode>()
                .is_some_and(|selected| selected == *node)
        }

        fn consent(&self, consent_row: &gtk::TreeListRow) {
            let Some(parent) = consent_row
                .parent()
                .and_then(|row| row.item())
                .and_downcast::<SchemaNode>()
            else {
                return;
            };
            parent.imp().consented.set(true);
            parent.imp().loaded.set(false);
            self.load(&parent);
        }

        pub(super) fn load(&self, node: &SchemaNode) {
            let imp = node.imp();
            if imp.loaded.get() {
                return;
            }
            imp.loaded.set(true);
            let Some(store) = node.children_store() else {
                return;
            };
            store.remove_all();
            if imp.enumeration.get().needs_consent() && !imp.consented.get() {
                store.append(&SchemaNode::consent());
                return;
            }
            self.fetch(&store, &imp.path.borrow(), &imp.path_json.borrow());
        }

        /// Off the main loop: listing a level is the unbounded call, and it sits
        /// on the click path of every expander.
        pub(super) fn fetch(&self, store: &gio::ListStore, path: &[String], path_json: &str) {
            let Some(core) = self.core.borrow().clone() else {
                return;
            };
            let profile = self.profile.borrow().clone();
            let generation = self.generation.get();
            store.remove_all();
            store.append(&SchemaNode::notice("Listing…"));
            let (tree, store, path) = (self.obj().downgrade(), store.clone(), path.to_vec());
            let path_json = path_json.to_owned();
            glib::spawn_future_local(async move {
                let listed = gio::spawn_blocking(move || {
                    core.catalog_children_json(&profile, &path_json)
                        .map_err(|e| e.0)
                        .and_then(|json| CatalogNode::parse_list(&json))
                })
                .await;
                let Some(tree) = tree.upgrade() else {
                    return;
                };
                // A level listed for a connection nobody is looking at any more is dropped.
                if tree.imp().generation.get() != generation {
                    return;
                }
                store.remove_all();
                match listed {
                    Ok(Ok(nodes)) if nodes.is_empty() => {
                        store.append(&SchemaNode::notice("Empty"))
                    }
                    Ok(Ok(nodes)) => {
                        for node in &nodes {
                            store.append(&SchemaNode::object(&path, node));
                        }
                    }
                    Ok(Err(message)) => store.append(&SchemaNode::notice(&message)),
                    Err(_) => store.append(&SchemaNode::notice("the listing did not finish")),
                }
            });
        }
    }
}

glib::wrapper! {
    pub struct SchemaTree(ObjectSubclass<imp::SchemaTree>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for SchemaTree {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaTree {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_core(&self, core: Arc<Core>) {
        *self.imp().core.borrow_mut() = Some(core);
    }

    /// One cheap query on connect — the top level and nothing under it.
    pub fn show_profile(&self, profile: &str) {
        let imp = self.imp();
        imp.generation.set(imp.generation.get() + 1);
        *imp.profile.borrow_mut() = profile.to_owned();
        imp.roots.remove_all();
        if profile.is_empty() {
            return;
        }
        imp.fetch(&imp.roots, &[], "[]");
    }

    pub fn connect_object_activated<F: Fn(&Self, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("object-activated", false, move |values| {
            let tree = values[0]
                .get::<Self>()
                .expect("the signal carries the tree");
            let path = values[1].get::<String>().unwrap_or_default();
            let name = values[2].get::<String>().unwrap_or_default();
            f(&tree, &path, &name);
            None
        })
    }

    /// The selected object's describe payload, or its failure; the tree made the call.
    pub fn connect_object_described<F: Fn(&Self, &str, &str, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("object-described", false, move |values| {
            let tree = values[0]
                .get::<Self>()
                .expect("the signal carries the tree");
            let path = values[1].get::<String>().unwrap_or_default();
            let detail = values[2].get::<String>().unwrap_or_default();
            let error = values[3].get::<String>().unwrap_or_default();
            f(&tree, &path, &detail, &error);
            None
        })
    }
}
