use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use datagrep_gtk::{CatalogNode, Core, Enumeration, Profile, QueryStatus};

struct Fixture {
    core: Core,
    db: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let db = std::env::temp_dir().join(format!(
            "datagrep-gtk-catalog-{}-{}.db",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_file(&db);
        let core = Core::open(":memory:").expect("an in-memory profile store opens");
        core.profiles_add("t", &format!("sqlite://{}", db.display()))
            .expect("the sqlite profile is accepted");
        let ddl = core
            .query(
                "t",
                "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)",
            )
            .expect("the DDL starts");
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let status = QueryStatus::parse(&ddl.status_json().expect("the status decodes"));
            if status.state.is_terminal() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        Self { core, db }
    }

    fn children(&self, path_json: &str) -> Vec<CatalogNode> {
        let json = self
            .core
            .catalog_children_json("t", path_json)
            .expect("the catalog answers");
        CatalogNode::parse_list(&json).expect("the catalog page decodes")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.db);
    }
}

#[test]
fn the_sidebar_walks_the_catalog_one_level_per_call() {
    let fixture = Fixture::new("levels");

    let roots = fixture.children("[]");
    let root = roots.first().expect("sqlite reports one database");
    assert!(root.has_children);
    assert_eq!(root.enumeration, Enumeration::Cheap);

    let tables = fixture.children(&format!("[\"{}\"]", root.name));
    let people = tables
        .iter()
        .find(|node| node.name == "people")
        .expect("the table just created is listed");
    assert_eq!(people.kind, "table");
    assert!(people.has_children);

    let columns = fixture.children(&format!("[\"{}\",\"people\"]", root.name));
    let names: Vec<&str> = columns.iter().map(|node| node.name.as_str()).collect();
    assert_eq!(names, ["id", "name"]);
    assert!(columns.iter().all(|node| !node.has_children));
}

#[test]
fn a_path_that_does_not_exist_is_an_error_the_tree_can_show() {
    let fixture = Fixture::new("missing");
    let failure = fixture
        .core
        .catalog_children_json("t", "[\"nope\",\"nope\"]");
    assert!(failure.is_err(), "expected an error, got {failure:?}");
}

#[test]
fn the_connections_list_reads_back_what_the_store_holds() {
    let fixture = Fixture::new("profiles");
    let json = fixture
        .core
        .profiles_list_json()
        .expect("the profile store answers");
    let profiles = Profile::parse_list(&json).expect("the profile list decodes");
    let profile = profiles
        .iter()
        .find(|p| p.name == "t")
        .expect("the profile just added is listed");
    assert_eq!(profile.driver, "sqlite");
    assert!(!profile.has_secret);
}
