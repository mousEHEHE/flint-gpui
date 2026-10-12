//! Parsing of a typed or pasted SSH destination: `user@host:port`,
//! `ssh user@host -p 2222 -J bastion`, `ssh://user@host:2222`.

use uuid::Uuid;

use crate::vault::ServerEntrySummary;

/// ssh options that take a value, so the next word is not the destination.
const OPTIONS_WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// A destination read from text. `None` fields were not given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    /// Raw `-J` value, not yet matched to a saved host.
    pub jump: Option<String>,
    /// Options that were understood as ssh flags but are not applied
    /// (`-i`, `-L`, ...), so the caller can say so instead of dropping them.
    pub ignored_options: Vec<String>,
}

impl SshTarget {
    /// `user@host:port`, with the defaults filled in for display.
    pub fn address(&self) -> String {
        let port = self.port.unwrap_or(22);
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match &self.user {
            Some(user) => format!("{user}@{host}:{port}"),
            None => format!("{host}:{port}"),
        }
    }
}

/// Read an SSH destination out of free text.
///
/// Only text that starts with `ssh ` or contains `user@` counts, so ordinary
/// search text is never taken for a host. A trailing remote command
/// (`ssh host uptime`) is rejected: that is not a request for a shell.
pub fn parse_ssh_target(input: &str) -> Option<SshTarget> {
    let mut words = input.split_whitespace().peekable();
    let has_ssh_prefix = words.peek().is_some_and(|w| *w == "ssh");
    if has_ssh_prefix {
        words.next();
    }

    let mut destination: Option<&str> = None;
    let mut option_user: Option<String> = None;
    let mut option_port: Option<u16> = None;
    let mut jump: Option<String> = None;
    let mut ignored_options: Vec<String> = Vec::new();

    while let Some(word) = words.next() {
        let Some(flags) = word.strip_prefix('-') else {
            if destination.is_some() {
                return None;
            }
            destination = Some(word);
            continue;
        };
        // ssh has no long options, and a lone `-` is not a flag.
        if flags.is_empty() || flags.starts_with('-') {
            return None;
        }
        // Flags can share one dash (`-tp2222`). The first one that takes a
        // value owns the rest of the word, or the next word.
        let mut letters = flags.chars();
        while let Some(letter) = letters.next() {
            if !OPTIONS_WITH_VALUE.contains(letter) {
                ignored_options.push(format!("-{letter}"));
                continue;
            }
            let attached = letters.as_str();
            let value = if attached.is_empty() {
                words.next()?
            } else {
                attached
            };
            match letter {
                'p' => option_port = Some(parse_port(value)?),
                'l' => option_user = Some(value.to_string()),
                'J' => jump = Some(value.to_string()),
                _ => ignored_options.push(format!("-{letter}")),
            }
            break;
        }
    }

    let destination = destination?;
    let destination = match destination.strip_prefix("ssh://") {
        Some(rest) => rest.trim_end_matches('/'),
        None => destination,
    };
    let (address_user, host, address_port) = split_hop_address(destination)?;
    if !is_plausible_host(&host) || address_port == Some(0) {
        return None;
    }
    if !has_ssh_prefix && address_user.is_none() {
        return None;
    }

    // Same order as OpenSSH: an option beats the same thing in the address.
    Some(SshTarget {
        user: option_user.or(address_user),
        host,
        port: option_port.or(address_port),
        jump,
        ignored_options,
    })
}

/// Read what was pasted into a "Host" field. Unlike [`parse_ssh_target`] this
/// also takes a bare `host:port`, and it returns `None` when the text is only
/// a host name, because then there is nothing to move to other fields.
pub fn parse_host_field(input: &str) -> Option<SshTarget> {
    let target = parse_ssh_target(input).or_else(|| {
        let (user, host, port) = split_hop_address(input)?;
        if !is_plausible_host(&host) || port == Some(0) {
            return None;
        }
        Some(SshTarget {
            user,
            host,
            port,
            jump: None,
            ignored_options: Vec::new(),
        })
    })?;
    let has_extra_parts = target.user.is_some() || target.port.is_some() || target.jump.is_some();
    // `ssh host` has no extra parts but the field should still end up as `host`.
    let is_command = input.trim() != target.host;
    (has_extra_parts || is_command).then_some(target)
}

/// Find the saved host a `-J` value names: by label first, then by address.
/// A list of hops (`a,b`) never matches, because an entry links to one jump host.
pub fn find_saved_jump_host(entries: &[ServerEntrySummary], jump: &str) -> Option<Uuid> {
    if jump.contains(',') {
        return None;
    }
    let (user, host, port) = split_hop_address(jump)?;
    find_saved_host_by_name(entries, user.as_deref(), &host, port).map(|e| e.id)
}

/// Find the saved host a name means: by label first, then by address.
/// A user or port that is not given matches any label; an address without a
/// port means port 22, as it does for ssh.
pub fn find_saved_host_by_name<'a>(
    entries: &'a [ServerEntrySummary],
    user: Option<&str>,
    name: &str,
    port: Option<u16>,
) -> Option<&'a ServerEntrySummary> {
    let is_same_user = |e: &ServerEntrySummary| user.is_none_or(|u| u == e.username);
    let is_same_port = |e: &ServerEntrySummary| port.is_none_or(|p| p == e.port);
    entries
        .iter()
        .find(|e| e.label == name && is_same_user(e) && is_same_port(e))
        .or_else(|| {
            entries.iter().find(|e| {
                e.host.eq_ignore_ascii_case(name) && e.port == port.unwrap_or(22) && is_same_user(e)
            })
        })
}

/// Find a saved host with exactly this login, host and port.
pub fn find_saved_host(
    entries: &[ServerEntrySummary],
    user: &str,
    host: &str,
    port: u16,
) -> Option<Uuid> {
    entries
        .iter()
        .find(|e| e.username == user && e.host.eq_ignore_ascii_case(host) && e.port == port)
        .map(|e| e.id)
}

/// Split `[user@]host[:port]`.
///
/// An IPv6 literal only carries a port when it is bracketed; a bare one
/// (`2001:db8::1`) has none, and splitting on the last colon would silently
/// eat part of the address.
pub fn split_hop_address(spec: &str) -> Option<(Option<String>, String, Option<u16>)> {
    let spec = spec.trim();
    let (user, rest) = match spec.rsplit_once('@') {
        Some((user, rest)) => ((!user.is_empty()).then(|| user.to_string()), rest),
        None => (None, spec),
    };

    if let Some(bracketed) = rest.strip_prefix('[') {
        let (host, tail) = bracketed.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        if tail.is_empty() {
            return Some((user, host.to_string(), None));
        }
        let port = tail.strip_prefix(':')?.parse().ok()?;
        return Some((user, host.to_string(), Some(port)));
    }

    if rest.is_empty() {
        return None;
    }
    if rest.matches(':').count() > 1 {
        return Some((user, rest.to_string(), None));
    }
    let Some((host, port)) = rest.split_once(':') else {
        return Some((user, rest.to_string(), None));
    };
    if host.is_empty() {
        return None;
    }
    Some((user, host.to_string(), Some(port.parse().ok()?)))
}

fn parse_port(value: &str) -> Option<u16> {
    value.parse().ok().filter(|port| *port > 0)
}

/// Host names, IPv4 and IPv6 literals (with a zone id) only use these characters.
fn is_plausible_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::ProxyChoice;

    fn target(user: Option<&str>, host: &str, port: Option<u16>) -> SshTarget {
        SshTarget {
            user: user.map(str::to_string),
            host: host.to_string(),
            port,
            jump: None,
            ignored_options: Vec::new(),
        }
    }

    fn saved(label: &str, user: &str, host: &str, port: u16) -> ServerEntrySummary {
        ServerEntrySummary {
            id: Uuid::new_v4(),
            label: label.to_string(),
            host: host.to_string(),
            port,
            username: user.to_string(),
            os: None,
            last_opened_at: None,
            group_name: None,
            tag_names: Vec::new(),
            proxy: ProxyChoice::Default,
            team_name: None,
        }
    }

    #[test]
    fn parses_user_at_host() {
        assert_eq!(
            parse_ssh_target("deploy@10.0.3.7"),
            Some(target(Some("deploy"), "10.0.3.7", None))
        );
        assert_eq!(
            parse_ssh_target("  deploy@10.0.3.7:2222 "),
            Some(target(Some("deploy"), "10.0.3.7", Some(2222))),
        );
    }

    #[test]
    fn parses_an_ssh_command() {
        assert_eq!(
            parse_ssh_target("ssh deploy@10.0.3.7 -p 2222"),
            Some(target(Some("deploy"), "10.0.3.7", Some(2222))),
        );
        assert_eq!(
            parse_ssh_target("ssh -p2222 -l deploy web.example.com"),
            Some(target(Some("deploy"), "web.example.com", Some(2222))),
        );
        assert_eq!(parse_ssh_target("ssh web"), Some(target(None, "web", None)));
    }

    #[test]
    fn keeps_the_case_of_the_user_name() {
        let parsed = parse_ssh_target("ssh Deploy@Web").unwrap();
        assert_eq!(parsed.user.as_deref(), Some("Deploy"));
    }

    #[test]
    fn options_beat_the_address() {
        let parsed = parse_ssh_target("ssh -l admin -p 22 deploy@web:2222").unwrap();
        assert_eq!(parsed.user.as_deref(), Some("admin"));
        assert_eq!(parsed.port, Some(22));
    }

    #[test]
    fn parses_a_jump_host() {
        let parsed = parse_ssh_target("ssh deploy@10.0.3.7 -p 2222 -J bastion").unwrap();
        assert_eq!(parsed.jump.as_deref(), Some("bastion"));
        assert_eq!(parsed.host, "10.0.3.7");
        let glued = parse_ssh_target("ssh -Jops@bastion:2200 deploy@web").unwrap();
        assert_eq!(glued.jump.as_deref(), Some("ops@bastion:2200"));
    }

    #[test]
    fn reports_options_it_does_not_apply() {
        let parsed =
            parse_ssh_target("ssh -A -i ~/.ssh/id_ed25519 -L 8080:localhost:80 deploy@web")
                .unwrap();
        assert_eq!(parsed.host, "web");
        assert_eq!(parsed.ignored_options, vec!["-A", "-i", "-L"]);
    }

    #[test]
    fn flags_can_share_one_dash() {
        let parsed = parse_ssh_target("ssh -tp2222 deploy@web").unwrap();
        assert_eq!(parsed.port, Some(2222));
        assert_eq!(parsed.ignored_options, vec!["-t"]);
        let parsed = parse_ssh_target("ssh -Av -p 2200 deploy@web").unwrap();
        assert_eq!(parsed.port, Some(2200));
        assert_eq!(parsed.ignored_options, vec!["-A", "-v"]);
        let parsed = parse_ssh_target("ssh -vl admin web").unwrap();
        assert_eq!(parsed.user.as_deref(), Some("admin"));
        assert_eq!(parse_ssh_target("ssh --verbose deploy@web"), None);
        assert_eq!(parse_ssh_target("ssh - deploy@web"), None);
    }

    #[test]
    fn a_name_without_a_user_matches_a_saved_label_or_address() {
        let entries = vec![
            saved("prod", "deploy", "10.0.0.5", 2222),
            saved("edge", "ops", "bastion", 22),
        ];
        assert_eq!(
            find_saved_host_by_name(&entries, None, "prod", None).map(|e| e.id),
            Some(entries[0].id)
        );
        assert_eq!(
            find_saved_host_by_name(&entries, None, "bastion", None).map(|e| e.id),
            Some(entries[1].id)
        );
        assert_eq!(
            find_saved_host_by_name(&entries, None, "prod", Some(22)).map(|e| e.id),
            None
        );
        // The address alone means port 22, which is not where this host listens.
        assert_eq!(
            find_saved_host_by_name(&entries, None, "10.0.0.5", None).map(|e| e.id),
            None
        );
        assert_eq!(
            find_saved_host_by_name(&entries, None, "pro", None).map(|e| e.id),
            None
        );
    }

    #[test]
    fn parses_an_ssh_url_and_ipv6() {
        assert_eq!(
            parse_ssh_target("ssh://deploy@web.example.com:2222/"),
            Some(target(Some("deploy"), "web.example.com", Some(2222))),
        );
        assert_eq!(
            parse_ssh_target("root@[2001:db8::1]:2222"),
            Some(target(Some("root"), "2001:db8::1", Some(2222))),
        );
        assert_eq!(
            parse_ssh_target("ssh 2001:db8::1"),
            Some(target(None, "2001:db8::1", None))
        );
    }

    #[test]
    fn ordinary_search_text_is_not_a_target() {
        assert_eq!(parse_ssh_target(""), None);
        assert_eq!(parse_ssh_target("ssh"), None);
        assert_eq!(parse_ssh_target("web"), None);
        assert_eq!(parse_ssh_target("web:2222"), None);
        assert_eq!(parse_ssh_target("restart nginx"), None);
        assert_eq!(parse_ssh_target("deploy@"), None);
        assert_eq!(parse_ssh_target("@web"), None);
        assert_eq!(parse_ssh_target("git@github.com:org/repo.git"), None);
    }

    #[test]
    fn rejects_broken_commands() {
        assert_eq!(parse_ssh_target("ssh deploy@web uptime"), None);
        assert_eq!(parse_ssh_target("ssh deploy@web -p"), None);
        assert_eq!(parse_ssh_target("ssh deploy@web -p 0"), None);
        assert_eq!(parse_ssh_target("ssh deploy@web -p 99999"), None);
        assert_eq!(parse_ssh_target("deploy@web:0"), None);
        assert_eq!(parse_ssh_target("ssh deploy@we/b"), None);
    }

    #[test]
    fn address_fills_in_the_default_port() {
        assert_eq!(
            target(Some("deploy"), "web", None).address(),
            "deploy@web:22"
        );
        assert_eq!(
            target(Some("root"), "2001:db8::1", Some(2222)).address(),
            "root@[2001:db8::1]:2222"
        );
        assert_eq!(target(None, "web", Some(2200)).address(), "web:2200");
    }

    #[test]
    fn host_field_splits_only_when_there_is_something_to_move() {
        assert_eq!(parse_host_field("web.example.com"), None);
        assert_eq!(parse_host_field("2001:db8::1"), None);
        assert_eq!(
            parse_host_field("web:2222"),
            Some(target(None, "web", Some(2222)))
        );
        assert_eq!(
            parse_host_field("deploy@web"),
            Some(target(Some("deploy"), "web", None))
        );
        assert_eq!(parse_host_field("ssh web"), Some(target(None, "web", None)));
        assert_eq!(
            parse_host_field("[2001:db8::1]:22"),
            Some(target(None, "2001:db8::1", Some(22)))
        );
        assert_eq!(parse_host_field("not a host"), None);
    }

    #[test]
    fn splits_a_hop_address() {
        assert_eq!(
            split_hop_address("bastion"),
            Some((None, "bastion".to_string(), None))
        );
        assert_eq!(
            split_hop_address("ops@bastion:2200"),
            Some((Some("ops".to_string()), "bastion".to_string(), Some(2200))),
        );
        assert_eq!(split_hop_address("bastion:port"), None);
        assert_eq!(split_hop_address("[::1"), None);
    }

    #[test]
    fn jump_host_matches_a_label_before_an_address() {
        let entries = vec![
            saved("edge", "ops", "bastion", 22),
            saved("bastion", "ops", "10.0.0.1", 2200),
        ];
        assert_eq!(
            find_saved_jump_host(&entries, "bastion"),
            Some(entries[1].id)
        );
        assert_eq!(
            find_saved_jump_host(&entries, "BASTION:22"),
            Some(entries[0].id)
        );
        assert_eq!(
            find_saved_jump_host(&entries, "ops@10.0.0.1:2200"),
            Some(entries[1].id)
        );
    }

    #[test]
    fn jump_host_that_is_not_saved_does_not_match() {
        let entries = vec![saved("bastion", "ops", "10.0.0.1", 2200)];
        assert_eq!(find_saved_jump_host(&entries, "other"), None);
        assert_eq!(find_saved_jump_host(&entries, "root@bastion"), None);
        assert_eq!(find_saved_jump_host(&entries, "bastion:22"), None);
        assert_eq!(find_saved_jump_host(&entries, "10.0.0.1"), None);
        assert_eq!(find_saved_jump_host(&entries, "bastion,other"), None);
    }

    #[test]
    fn saved_host_needs_the_same_login_host_and_port() {
        let entries = vec![saved("web", "deploy", "Web.Example.com", 2222)];
        assert_eq!(
            find_saved_host(&entries, "deploy", "web.example.com", 2222),
            Some(entries[0].id)
        );
        assert_eq!(
            find_saved_host(&entries, "Deploy", "web.example.com", 2222),
            None
        );
        assert_eq!(
            find_saved_host(&entries, "deploy", "web.example.com", 22),
            None
        );
    }
}
