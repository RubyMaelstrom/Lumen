//! WHATWG URL parsing and serialization shared by the JavaScript API and every network client.
//!
//! The state machine comes from the pure-Rust Servo `url` implementation. Its IDNA path uses
//! non-transitional UTS #46 processing with the URL Standard's forbidden-domain-code-point list.
//! Keeping the parsed record here prevents fetch, WebSocket, EventSource, and `URL` from applying
//! subtly different authority, IPv4/IPv6, percent-encoding, or relative-resolution rules.

use whatwg_url::Url as ParsedUrl;

/// Parsed components. `port` is `None` when absent or equal to the scheme's default port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Url {
    pub scheme: String,
    pub username: String,
    pub password: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: String,    // includes leading '?' when present
    pub fragment: String, // includes leading '#' when present
    parsed: ParsedUrl,
}

impl Url {
    fn from_parsed(parsed: ParsedUrl) -> Self {
        let query = parsed.query().map_or_else(String::new, |value| {
            let mut output = String::with_capacity(value.len() + 1);
            output.push('?');
            output.push_str(value);
            output
        });
        let fragment = parsed.fragment().map_or_else(String::new, |value| {
            let mut output = String::with_capacity(value.len() + 1);
            output.push('#');
            output.push_str(value);
            output
        });
        Self {
            scheme: parsed.scheme().to_string(),
            username: parsed.username().to_string(),
            password: parsed.password().unwrap_or_default().to_string(),
            host: parsed.host_str().unwrap_or_default().to_string(),
            port: parsed.port(),
            path: parsed.path().to_string(),
            query,
            fragment,
            parsed,
        }
    }

    pub fn href(&self) -> String {
        self.parsed.as_str().to_string()
    }

    pub fn origin(&self) -> String {
        self.parsed.origin().ascii_serialization()
    }

    pub fn set_fragment(&mut self, fragment: &str) {
        let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
        self.parsed
            .set_fragment((!fragment.is_empty()).then_some(fragment));
        self.fragment = self.parsed.fragment().map_or_else(String::new, |value| {
            let mut output = String::with_capacity(value.len() + 1);
            output.push('#');
            output.push_str(value);
            output
        });
    }
}

/// Parse `input` on its own, or against `base` when it is relative.
pub(crate) fn parse(input: &str, base: Option<&str>) -> Result<Url, String> {
    let parsed = match base {
        Some(base) => ParsedUrl::parse(base)
            .map_err(|error| format!("invalid base URL '{base}': {error}"))?
            .join(input)
            .map_err(|error| format!("invalid URL '{input}': {error}"))?,
        None => {
            ParsedUrl::parse(input).map_err(|error| format!("invalid URL '{input}': {error}"))?
        }
    };
    Ok(Url::from_parsed(parsed))
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "ftp" => Some(21),
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    }
}

/// Apply one URL API setter using a parsed URL record. Setter parse failures are ignored, as the
/// URL Standard requires, while the `href` setter continues to use `parse` and throw on failure.
pub(crate) fn mutate(href: &str, component: &str, value: &str) -> Result<Url, String> {
    let mut parsed =
        ParsedUrl::parse(href).map_err(|error| format!("invalid current URL '{href}': {error}"))?;
    match component {
        "protocol" => {
            let scheme = value.strip_suffix(':').unwrap_or(value);
            let _ = parsed.set_scheme(scheme);
        }
        "username" => {
            let _ = parsed.set_username(value);
        }
        "password" => {
            let _ = parsed.set_password((!value.is_empty()).then_some(value));
        }
        "host" => {
            // `Url::set_host` deliberately excludes a port. Parse the setter input as an
            // authority so host and port are changed atomically and malformed values are ignored.
            if !value
                .chars()
                .any(|character| matches!(character, '/' | '\\' | '?' | '#' | '@'))
            {
                let candidate = format!("{}://{value}/", parsed.scheme());
                if let Ok(authority) = ParsedUrl::parse(&candidate) {
                    if parsed.set_host(authority.host_str()).is_ok() {
                        let _ = parsed.set_port(authority.port());
                    }
                }
            }
        }
        "hostname" => {
            let _ = parsed.set_host(Some(value));
        }
        "port" => {
            if value.is_empty() {
                let _ = parsed.set_port(None);
            } else if value.bytes().all(|byte| byte.is_ascii_digit()) {
                if let Ok(port) = value.parse::<u16>() {
                    let port = (Some(port) != default_port(parsed.scheme())).then_some(port);
                    let _ = parsed.set_port(port);
                }
            }
        }
        "pathname" => parsed.set_path(value),
        "search" => {
            if value.is_empty() {
                parsed.set_query(None);
            } else {
                parsed.set_query(Some(value.strip_prefix('?').unwrap_or(value)));
            }
        }
        "hash" => {
            if value.is_empty() {
                parsed.set_fragment(None);
            } else {
                parsed.set_fragment(Some(value.strip_prefix('#').unwrap_or(value)));
            }
        }
        _ => return Err(format!("unknown URL component '{component}'")),
    }
    Ok(Url::from_parsed(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(input: &str) -> Url {
        parse(input, None).unwrap()
    }

    #[test]
    fn parses_serializes_and_resolves_special_urls() {
        let url = p("HTTP://User:Pw@Example.COM:8080/a/b/../c?q=1#frag");
        assert_eq!(url.href(), "http://User:Pw@example.com:8080/a/c?q=1#frag");
        assert_eq!(url.origin(), "http://example.com:8080");
        assert_eq!(url.username, "User");
        assert_eq!(url.password, "Pw");
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, Some(8080));
        assert_eq!(url.path, "/a/c");
        assert_eq!(
            parse("../d", Some("http://example.com/a/b/c?old#f"))
                .unwrap()
                .href(),
            "http://example.com/a/d"
        );
        assert_eq!(p("https:example.org").href(), "https://example.org/");
        assert_eq!(p("https://example.org\\a").path, "/a");
    }

    #[test]
    fn implements_uts46_ipv4_and_ipv6_host_algorithms() {
        assert_eq!(p("https://faß.example/").host, "xn--fa-hia.example");
        assert_eq!(p("https://①.②.③.④/").host, "1.2.3.4");
        assert_eq!(p("http://0x7f.1/").host, "127.0.0.1");
        assert_eq!(p("http://[2001:0db8::1]/").host, "[2001:db8::1]");
        assert!(parse("https://exa%23mple.org/", None).is_err());
        assert!(parse("http://[1::1::1]/", None).is_err());
    }

    #[test]
    fn preserves_opaque_paths_and_opaque_origins() {
        let url = p("mailto:some one@example.org?q=hello world#fragment");
        assert_eq!(
            url.href(),
            "mailto:some one@example.org?q=hello%20world#fragment"
        );
        assert_eq!(url.origin(), "null");
    }

    #[test]
    fn setters_use_component_state_overrides_and_ignore_invalid_input() {
        let url = mutate("http://example.com/a", "pathname", "next value").unwrap();
        assert_eq!(url.href(), "http://example.com/next%20value");
        let url = mutate(&url.href(), "host", "bücher.example:443").unwrap();
        assert_eq!(url.host, "xn--bcher-kva.example");
        assert_eq!(url.port, Some(443));
        let unchanged = mutate(&url.href(), "port", "70000").unwrap();
        assert_eq!(unchanged.href(), url.href());
    }
}
