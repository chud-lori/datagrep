use std::collections::HashMap;
use std::path::PathBuf;

use crate::TunnelError;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct HostBlock {
    patterns: Vec<String>,
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_files: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SshConfig {
    blocks: Vec<HostBlock>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostConfig {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<PathBuf>,
}

impl SshConfig {
    pub fn parse(contents: &str) -> Result<Self, TunnelError> {
        let mut blocks = Vec::new();
        let mut current: Option<HostBlock> = None;

        for raw_line in contents.lines() {
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            let (keyword, rest) = split_keyword(line);
            let keyword_lc = keyword.to_ascii_lowercase();

            if keyword_lc == "host" {
                if let Some(block) = current.take() {
                    blocks.push(block);
                }
                current = Some(HostBlock {
                    patterns: rest.split_whitespace().map(str::to_owned).collect(),
                    ..Default::default()
                });
                continue;
            }

            let Some(block) = current.as_mut() else {
                current = Some(HostBlock {
                    patterns: vec!["*".to_owned()],
                    ..Default::default()
                });
                if let Some(b) = current.as_mut() {
                    apply_keyword(b, &keyword_lc, rest);
                }
                continue;
            };
            apply_keyword(block, &keyword_lc, rest);
        }
        if let Some(block) = current.take() {
            blocks.push(block);
        }
        Ok(Self { blocks })
    }

    pub fn lookup(&self, alias: &str) -> HostConfig {
        let mut resolved = HostConfig::default();
        let mut seen_identity_files: HashMap<&str, ()> = HashMap::new();

        for block in &self.blocks {
            if !block.patterns.iter().any(|p| glob_match(p, alias)) {
                continue;
            }
            if resolved.hostname.is_none() {
                resolved.hostname.clone_from(&block.hostname);
            }
            if resolved.user.is_none() {
                resolved.user.clone_from(&block.user);
            }
            if resolved.port.is_none() {
                resolved.port = block.port;
            }
            for f in &block.identity_files {
                if seen_identity_files.insert(f.as_str(), ()).is_none() {
                    resolved.identity_files.push(expand_tilde(f));
                }
            }
        }
        resolved
    }

    pub fn load_default() -> Result<Option<Self>, TunnelError> {
        let Some(home) = dirs::home_dir() else {
            return Ok(None);
        };
        Self::load_path(home.join(".ssh").join("config"))
    }

    pub fn load_path(path: impl Into<PathBuf>) -> Result<Option<Self>, TunnelError> {
        let path = path.into();
        match std::fs::read_to_string(&path) {
            Ok(contents) => Self::parse(&contents).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(TunnelError::Io(source)),
        }
    }
}

fn apply_keyword(block: &mut HostBlock, keyword_lc: &str, rest: &str) {
    match keyword_lc {
        "hostname" => {
            if block.hostname.is_none() {
                block.hostname = Some(rest.to_owned());
            }
        }
        "user" => {
            if block.user.is_none() {
                block.user = Some(rest.to_owned());
            }
        }
        "port" => {
            if block.port.is_none() {
                block.port = rest.parse().ok();
            }
        }
        "identityfile" => block.identity_files.push(rest.to_owned()),
        "proxyjump" => {
            tracing::debug!(target: "datagrep_tunnel::ssh_config", "ignoring unsupported ProxyJump directive");
        }
        _ => {}
    }
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
}

fn split_keyword(line: &str) -> (&str, &str) {
    if let Some(eq) = line.find('=') {
        let (k, v) = line.split_at(eq);
        (k.trim(), v[1..].trim())
    } else {
        match line.split_once(char::is_whitespace) {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (line.trim(), ""),
        }
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Standard DP for `*`/`?` globbing.
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for (i, &pc) in p.iter().enumerate() {
        if pc == '*' {
            dp[i + 1][0] = dp[i][0];
        }
    }
    for (i, &pc) in p.iter().enumerate() {
        for (j, &tc) in t.iter().enumerate() {
            dp[i + 1][j + 1] = match pc {
                '*' => dp[i][j + 1] || dp[i + 1][j],
                '?' => dp[i][j],
                c => dp[i][j] && c == tc,
            };
        }
    }
    dp[p.len()][t.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("prod-*", "prod-db-1"));
        assert!(!glob_match("prod-*", "staging-db-1"));
        assert!(glob_match("db?", "db1"));
        assert!(!glob_match("db?", "db12"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
    }

    #[test]
    fn single_block_all_fields() {
        let cfg = SshConfig::parse(
            "Host myhost\n  HostName 10.0.0.5\n  User alice\n  Port 2222\n  IdentityFile ~/.ssh/id_ed25519\n",
        )
        .unwrap();
        let resolved = cfg.lookup("myhost");
        assert_eq!(resolved.hostname.as_deref(), Some("10.0.0.5"));
        assert_eq!(resolved.user.as_deref(), Some("alice"));
        assert_eq!(resolved.port, Some(2222));
        assert_eq!(resolved.identity_files.len(), 1);
    }

    #[test]
    fn glob_pattern_in_host_line() {
        let cfg = SshConfig::parse("Host prod-*\n  User deploy\n  Port 22\n").unwrap();
        let resolved = cfg.lookup("prod-db-1");
        assert_eq!(resolved.user.as_deref(), Some("deploy"));
        let none = cfg.lookup("staging-db-1");
        assert_eq!(none.user, None);
    }

    #[test]
    fn multiple_blocks_first_match_wins_per_key() {
        let cfg = SshConfig::parse(
            "Host specific\n  User specific-user\n\n\
             Host *\n  User default-user\n  Port 22\n",
        )
        .unwrap();
        let resolved = cfg.lookup("specific");
        // `User` came from the first (more specific) matching block...
        assert_eq!(resolved.user.as_deref(), Some("specific-user"));
        // ...but `Port`, absent there, falls through to the catch-all.
        assert_eq!(resolved.port, Some(22));
    }

    #[test]
    fn identity_file_accumulates_across_matching_blocks() {
        let cfg = SshConfig::parse(
            "Host bastion\n  IdentityFile ~/.ssh/bastion_key\n\n\
             Host *\n  IdentityFile ~/.ssh/id_ed25519\n",
        )
        .unwrap();
        let resolved = cfg.lookup("bastion");
        assert_eq!(resolved.identity_files.len(), 2);
    }

    #[test]
    fn multiple_host_patterns_on_one_line() {
        let cfg = SshConfig::parse("Host foo bar baz\n  User shared\n").unwrap();
        for alias in ["foo", "bar", "baz"] {
            assert_eq!(cfg.lookup(alias).user.as_deref(), Some("shared"));
        }
        assert_eq!(cfg.lookup("quux").user, None);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let cfg = SshConfig::parse(
            "# a comment\n\nHost h\n  # nested comment\n  User u # trailing comment\n",
        )
        .unwrap();
        assert_eq!(cfg.lookup("h").user.as_deref(), Some("u"));
    }

    #[test]
    fn proxy_jump_is_ignored_not_a_parse_error() {
        let cfg = SshConfig::parse("Host jumped\n  ProxyJump bastion\n  User via-jump\n").unwrap();
        let resolved = cfg.lookup("jumped");
        assert_eq!(resolved.user.as_deref(), Some("via-jump"));
    }

    #[test]
    fn keyword_equals_value_syntax() {
        let cfg = SshConfig::parse("Host h\n  User=eq-user\n  Port=2200\n").unwrap();
        let resolved = cfg.lookup("h");
        assert_eq!(resolved.user.as_deref(), Some("eq-user"));
        assert_eq!(resolved.port, Some(2200));
    }
}
