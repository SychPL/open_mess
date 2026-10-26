//! A module's host list: exact host names or `*.` suffix patterns (spec 8.3, R7).

use crate::error::HostError;

/// Hosts a module may reach, in the form URLs carry them (punycode, lowercase).
#[derive(Clone, Debug, Default)]
pub struct HostList {
    exact: Vec<String>,
    /// `*.example.com` is kept as `.example.com`.
    suffixes: Vec<String>,
}

/// Punycode and lowercase, the form `url` gives a URL's host; IP literals in their URL spelling.
fn normalize(name: &str) -> Result<String, HostError> {
    let refuse = || HostError::Load(format!("invalid host pattern {name:?}"));
    if name.contains('*') {
        return Err(refuse());
    }
    let host = url::Host::parse(name).map_err(|_| refuse())?.to_string();
    Ok(host.trim_end_matches('.').to_string())
}

impl HostList {
    /// Parses a manifest's host list. A `*.` pattern must sit below a registrable domain of the
    /// public suffix list, so `*.com`, `*.co.uk` or `*.github.io` are refused (spec 8.3, R7).
    pub fn parse(patterns: &[String]) -> Result<HostList, HostError> {
        let mut list = HostList::default();
        for pattern in patterns {
            match pattern.strip_prefix("*.") {
                Some(base) => {
                    let base = normalize(base)?;
                    if psl::domain_str(&base).is_none() {
                        return Err(HostError::Load(format!("host pattern {pattern:?} is not below a registrable domain")));
                    }
                    list.suffixes.push(format!(".{base}"));
                }
                None => list.exact.push(normalize(pattern)?),
            }
        }
        Ok(list)
    }

    /// Whether a URL's host (as `url::Url::host_str` gives it) is on the list.
    pub fn allows(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.exact.contains(&host) || self.suffixes.iter().any(|suffix| host.ends_with(suffix.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(patterns: &[&str]) -> Result<HostList, HostError> {
        HostList::parse(&patterns.iter().map(|p| p.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn suffix_pattern_matches_subdomains_only() {
        let hosts = list(&["*.sharepoint.com", "teams.microsoft.com"]).unwrap();
        assert!(hosts.allows("contoso.sharepoint.com"));
        assert!(hosts.allows("a.b.sharepoint.com"));
        assert!(!hosts.allows("sharepoint.com"));
        assert!(!hosts.allows("evilsharepoint.com"));
        assert!(hosts.allows("teams.microsoft.com"));
        assert!(!hosts.allows("x.teams.microsoft.com"));
        assert!(!hosts.allows("microsoft.com"));
    }

    #[test]
    fn bare_public_suffix_is_rejected() {
        for pattern in ["*.com", "*.co.uk", "*.github.io", "*", "*.*.example.com", "a.*.example.com", ""] {
            assert!(matches!(list(&[pattern]), Err(HostError::Load(_))), "{pattern}");
        }
    }

    #[test]
    fn match_is_case_insensitive() {
        let hosts = list(&["*.SharePoint.com", "Login.MicrosoftOnline.com"]).unwrap();
        assert!(hosts.allows("CONTOSO.sharepoint.COM"));
        assert!(hosts.allows("login.microsoftonline.com"));
        assert!(hosts.allows("login.microsoftonline.com."));
    }

    #[test]
    fn unicode_patterns_match_their_punycode() {
        let hosts = list(&["*.bücher.example"]).unwrap();
        assert!(hosts.allows("shop.xn--bcher-kva.example"));
    }

    #[test]
    fn teams_patterns_are_accepted() {
        list(&["*.trouter.teams.microsoft.com", "*.asyncgw.teams.microsoft.com", "*.asm.skype.com", "*.sharepoint.com"]).unwrap();
    }
}
