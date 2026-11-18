//! Discover SSH hosts from `~/.ssh/config`.
//!
//! Ported from T3 Code's `packages/ssh/src/config.ts` (MIT). Concrete `Host`
//! aliases become candidates; patterns (`*`, `?`, `!`) are skipped. `HostName`
//! and `Port` are resolved the way OpenSSH does (first match wins), and any
//! value that cannot be resolved statically (a `Match` block, tokens other than
//! `%h`, unreadable `Include`) leaves the value unknown instead of guessing.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;

const MAX_INCLUDE_DEPTH: usize = 16;

/// A concrete host alias from the user's SSH config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SshHost {
    /// The alias to pass to `ssh`, which applies the user's config.
    pub alias: String,
    /// The resolved `HostName`, when it is known statically.
    pub hostname: Option<String>,
    /// The resolved `Port`, when it is set and known statically.
    pub port: Option<u16>,
}

/// Reads host aliases from an SSH config file and the files it includes.
#[derive(Debug, Clone)]
pub struct SshConfigSource {
    home_dir: PathBuf,
    config_path: PathBuf,
}

impl SshConfigSource {
    /// Uses `~/.ssh/config` for the current user.
    pub fn from_home() -> Option<Self> {
        dirs::home_dir().map(Self::for_home)
    }

    /// Uses `HOME/.ssh/config`.
    pub fn for_home(home_dir: PathBuf) -> Self {
        let config_path = home_dir.join(".ssh").join("config");
        Self {
            home_dir,
            config_path,
        }
    }

    /// Returns the concrete aliases, sorted by name. A missing config yields no
    /// hosts.
    pub fn hosts(&self) -> io::Result<Vec<SshHost>> {
        let mut state = ParseState::default();
        collect_from_file(
            &self.config_path,
            &self.home_dir,
            &mut state,
            &[],
            /*depth*/ 0,
        )?;
        Ok(state.into_hosts())
    }
}

/// Parses SSH config text without following `Include` directives.
pub fn parse_ssh_config(contents: &str) -> Vec<SshHost> {
    let mut state = ParseState::default();
    collect_from_str(
        contents,
        Path::new(""),
        &mut state,
        &[],
        /*depth*/ MAX_INCLUDE_DEPTH,
    );
    state.into_hosts()
}

/// A `Host` pattern list, or `None` for a block whose applicability is unknown
/// (`Match`).
type Condition = Option<Vec<String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Directive {
    HostName,
    Port,
}

#[derive(Debug)]
struct Rule {
    conditions: Vec<Condition>,
    /// `None` means "may set anything" (an Include we could not read).
    directive: Option<Directive>,
    value: Option<String>,
}

#[derive(Debug, Default)]
struct ParseState {
    aliases: Vec<String>,
    rules: Vec<Rule>,
    visited: HashSet<PathBuf>,
}

impl ParseState {
    fn into_hosts(self) -> Vec<SshHost> {
        let mut hosts = BTreeMap::new();
        for alias in self.aliases {
            let hostname = resolve_hostname(&alias, &self.rules);
            let port = match resolve_value(&alias, Directive::Port, &self.rules) {
                Resolved::Known(port) => port.parse().ok(),
                Resolved::Unset | Resolved::Unknown => None,
            };
            hosts.insert(
                alias.clone(),
                SshHost {
                    alias,
                    hostname,
                    port,
                },
            );
        }
        hosts.into_values().collect()
    }
}

fn collect_from_file(
    path: &Path,
    home_dir: &Path,
    state: &mut ParseState,
    inherited: &[Condition],
    depth: usize,
) -> io::Result<()> {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if state.visited.contains(&resolved) || depth > MAX_INCLUDE_DEPTH {
        state.rules.push(Rule {
            conditions: inherited.to_vec(),
            directive: None,
            value: None,
        });
        return Ok(());
    }
    if !resolved.is_file() {
        return Ok(());
    }
    state.visited.insert(resolved.clone());
    let contents = std::fs::read_to_string(&resolved)?;
    collect_from_str(&contents, home_dir, state, inherited, depth);
    Ok(())
}

fn collect_from_str(
    contents: &str,
    home_dir: &Path,
    state: &mut ParseState,
    inherited: &[Condition],
    depth: usize,
) {
    let mut condition: Condition = Some(vec!["*".to_string()]);
    for line in contents.lines() {
        let Some((directive, args)) = parse_line(line) else {
            continue;
        };
        let mut conditions = inherited.to_vec();
        conditions.push(condition.clone());
        match directive.as_str() {
            "host" => {
                for alias in args.iter().flatten() {
                    if !alias.is_empty() && !has_pattern(alias) && !state.aliases.contains(alias) {
                        state.aliases.push(alias.clone());
                    }
                }
                condition = args;
            }
            "match" => condition = None,
            "hostname" => state.rules.push(Rule {
                conditions,
                directive: Some(Directive::HostName),
                value: args
                    .filter(|args| args.len() == 1)
                    .map(|mut args| args.remove(0)),
            }),
            "port" => state.rules.push(Rule {
                conditions,
                directive: Some(Directive::Port),
                value: args
                    .filter(|args| args.len() == 1)
                    .and_then(|args| args[0].parse::<u16>().ok())
                    .filter(|port| *port > 0)
                    .map(|port| port.to_string()),
            }),
            "include" => {
                let Some(patterns) = args else {
                    state.rules.push(Rule {
                        conditions,
                        directive: None,
                        value: None,
                    });
                    continue;
                };
                for pattern in patterns {
                    let unsupported = pattern.contains('%')
                        || pattern.contains("${")
                        || pattern.contains('[')
                        || (pattern.starts_with('~')
                            && !(pattern == "~" || pattern.starts_with("~/")));
                    if unsupported || depth >= MAX_INCLUDE_DEPTH {
                        state.rules.push(Rule {
                            conditions: conditions.clone(),
                            directive: None,
                            value: None,
                        });
                        continue;
                    }
                    let expanded = expand_home(&pattern, home_dir);
                    let pattern_path = if expanded.is_absolute() {
                        expanded
                    } else {
                        // OpenSSH resolves relative user-config includes in ~/.ssh.
                        home_dir.join(".ssh").join(expanded)
                    };
                    for included in expand_glob(&pattern_path) {
                        if let Err(_err) =
                            collect_from_file(&included, home_dir, state, &conditions, depth + 1)
                        {
                            state.rules.push(Rule {
                                conditions: conditions.clone(),
                                directive: None,
                                value: None,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Splits a config line into a lowercase keyword and its arguments, following
/// OpenSSH's `argv_split()` quoting. `None` args mean the quoting was invalid.
fn parse_line(line: &str) -> Option<(String, Option<Vec<String>>)> {
    let line = line.trim_end_matches([' ', '\t', '\r', '\x0c']);
    let line = line.trim_start_matches([' ', '\t', '\r']);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let keyword_end = line.find([' ', '\t', '\r', '=']).unwrap_or(line.len());
    let directive = line[..keyword_end].to_ascii_lowercase();
    let rest = line[keyword_end..].trim_start_matches([' ', '\t', '\r']);
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    let rest = rest.trim_start_matches([' ', '\t', '\r']);

    let mut args = Vec::new();
    let mut token: Option<String> = None;
    let mut quote: Option<char> = None;
    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.next() {
        let next = chars.peek().copied();
        let escaped = ch == '\\'
            && match next {
                Some('"' | '\'' | '\\') => true,
                Some(' ') => quote.is_none(),
                _ => false,
            };
        if escaped {
            if let Some(next) = next {
                token.get_or_insert_with(String::new).push(next);
            }
            chars.next();
        } else if let Some(open) = quote {
            if ch == open {
                quote = None;
            } else {
                token.get_or_insert_with(String::new).push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
            token.get_or_insert_with(String::new);
        } else if ch == ' ' || ch == '\t' {
            if let Some(done) = token.take() {
                args.push(done);
            }
        } else if ch == '#' && token.is_none() {
            break;
        } else {
            token.get_or_insert_with(String::new).push(ch);
        }
    }
    if quote.is_some() {
        return Some((directive, None));
    }
    if let Some(done) = token {
        args.push(done);
    }
    Some((directive, Some(args)))
}

fn has_pattern(value: &str) -> bool {
    value.contains('*') || value.contains('?') || value.starts_with('!')
}

fn expand_home(pattern: &str, home_dir: &Path) -> PathBuf {
    match pattern.strip_prefix('~') {
        Some(rest) => home_dir.join(rest.trim_start_matches('/')),
        None => PathBuf::from(pattern),
    }
}

/// Expands `*` and `?` in the final path component; directories with
/// wildcards are not expanded.
fn expand_glob(pattern: &Path) -> Vec<PathBuf> {
    let Some(file_pattern) = pattern.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if !file_pattern.contains(['*', '?']) {
        return if pattern.is_file() {
            vec![pattern.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let Some(directory) = pattern.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut matches = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if name.starts_with('.') && !file_pattern.starts_with('.') {
                return None;
            }
            glob_matches(file_pattern, &name).then(|| entry.path())
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    fn matches(pattern: &[u8], value: &[u8]) -> bool {
        match (pattern.first(), value.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                matches(&pattern[1..], value)
                    || (!value.is_empty() && matches(pattern, &value[1..]))
            }
            (Some(b'?'), Some(_)) => matches(&pattern[1..], &value[1..]),
            (Some(expected), Some(actual)) if expected == actual => {
                matches(&pattern[1..], &value[1..])
            }
            _ => false,
        }
    }
    matches(pattern.as_bytes(), value.as_bytes())
}

fn matches_host_patterns(alias: &str, patterns: &[String]) -> bool {
    let mut matched = false;
    for pattern in patterns {
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern.as_str()),
        };
        let hit = pattern == "*"
            || if has_pattern(pattern) {
                glob_matches(pattern, alias)
            } else {
                pattern == alias
            };
        if !hit {
            continue;
        }
        if negated {
            return false;
        }
        matched = true;
    }
    matched
}

enum Resolved {
    Known(String),
    Unset,
    Unknown,
}

fn resolve_value(alias: &str, directive: Directive, rules: &[Rule]) -> Resolved {
    for rule in rules {
        if rule
            .directive
            .is_some_and(|candidate| candidate != directive)
        {
            continue;
        }
        let applies = rule.conditions.iter().all(|condition| match condition {
            Some(patterns) => matches_host_patterns(alias, patterns),
            None => true,
        });
        if !applies {
            continue;
        }
        // SSH takes the first value; an uncertain earlier value blocks later ones.
        if rule.conditions.iter().any(Option::is_none) {
            return Resolved::Unknown;
        }
        return match &rule.value {
            Some(value) => Resolved::Known(value.clone()),
            None => Resolved::Unknown,
        };
    }
    Resolved::Unset
}

fn resolve_hostname(alias: &str, rules: &[Rule]) -> Option<String> {
    match resolve_value(alias, Directive::HostName, rules) {
        Resolved::Known(hostname) => {
            let without_alias_token = hostname.replace("%h", "");
            if without_alias_token.contains('%') || hostname.contains('$') {
                None
            } else {
                Some(hostname.replace("%h", alias))
            }
        }
        Resolved::Unset | Resolved::Unknown => None,
    }
}

#[cfg(test)]
#[path = "ssh_config_tests.rs"]
mod tests;
