use std::path::PathBuf;

use datagrep_api::SecretString;

#[derive(Debug)]
pub enum Auth {
    Agent,
    KeyFile {
        path: PathBuf,
        passphrase: Option<SecretString>,
    },
    Password(SecretString),
}
