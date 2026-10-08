//! Local HTTP caller authentication and browser-origin checks.

use crate::http::Request;

const TOKEN_HEX_LEN: usize = 64;

/// A validated bearer credential. Its contents are deliberately not printable.
pub(crate) struct AuthToken(String);

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AuthToken([redacted])")
    }
}

impl AuthToken {
    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        if value.len() != TOKEN_HEX_LEN || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("token must be 64 hexadecimal characters");
        }
        Ok(Self(value.to_owned()))
    }

    fn matches(&self, candidate: &str) -> bool {
        let Some((scheme, candidate)) = candidate.split_once(' ') else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("Bearer") {
            return false;
        }
        let candidate = candidate.as_bytes();
        let expected = self.0.as_bytes();
        if candidate.len() != expected.len() {
            return false;
        }
        candidate
            .iter()
            .zip(expected)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
    }
}

/// Whether a request can proceed at the HTTP trust boundary.
pub(crate) enum AccessError {
    Unauthorized,
    Forbidden,
    NotReady,
}

/// Checks Host and optional Origin before authentication/business handling.
pub(crate) fn authorize(
    request: &Request,
    expected_port: u16,
    token: Option<&AuthToken>,
    required: bool,
) -> Result<(), AccessError> {
    let host = request.header("host").ok_or(AccessError::Forbidden)?;
    let (host_name, port) = parse_host(host, expected_port).ok_or(AccessError::Forbidden)?;

    if let Some(origin) = request.header("origin") {
        if origin == "null" {
            return Err(AccessError::Forbidden);
        }
        let Some(origin_host) = origin.strip_prefix("http://") else {
            return Err(AccessError::Forbidden);
        };
        let origin_host = origin_host.strip_suffix('/').unwrap_or(origin_host);
        if origin_host.contains('/') || origin_host.contains('?') || origin_host.contains('#') {
            return Err(AccessError::Forbidden);
        }
        let Some((origin_name, origin_port)) = parse_host(origin_host, expected_port) else {
            return Err(AccessError::Forbidden);
        };
        if origin_name != host_name || origin_port != port {
            return Err(AccessError::Forbidden);
        }
    }

    if required {
        let Some(token) = token else {
            return Err(AccessError::NotReady);
        };
        if !request
            .header("authorization")
            .is_some_and(|value| token.matches(value))
        {
            return Err(AccessError::Unauthorized);
        }
    } else if let (Some(token), Some(value)) = (token, request.header("authorization"))
        && !token.matches(value)
    {
        return Err(AccessError::Unauthorized);
    }
    Ok(())
}

fn parse_host(value: &str, expected_port: u16) -> Option<(&'static str, u16)> {
    let (name, port) = match value.rsplit_once(':') {
        Some((name, port)) => (name, port.parse::<u16>().ok()?),
        None if expected_port == 80 => (value, 80),
        None => return None,
    };
    let name = match name.to_ascii_lowercase().as_str() {
        "127.0.0.1" => "127.0.0.1",
        "localhost" => "localhost",
        _ => return None,
    };
    (port == expected_port).then_some((name, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(headers: &[(&str, &str)]) -> Request {
        Request {
            method: "GET".into(),
            path: "/health".into(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn validates_token_and_http_origin_policy() {
        let token = AuthToken::parse(&"a".repeat(64)).unwrap();
        assert!(
            authorize(
                &request(&[
                    ("host", "127.0.0.1:8471"),
                    ("authorization", &format!("Bearer {}", "a".repeat(64)))
                ]),
                8471,
                Some(&token),
                true,
            )
            .is_ok()
        );
        assert!(matches!(
            authorize(
                &request(&[("host", "evil.test:8471")]),
                8471,
                Some(&token),
                true
            ),
            Err(AccessError::Forbidden)
        ));
        assert!(matches!(
            authorize(
                &request(&[
                    ("host", "localhost:8471"),
                    ("origin", "http://127.0.0.1:8471")
                ]),
                8471,
                Some(&token),
                true,
            ),
            Err(AccessError::Forbidden)
        ));
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_but_token_is_exact() {
        let token = AuthToken::parse(&"a".repeat(64)).unwrap();
        for scheme in ["Bearer", "bearer", "BEARER", "bEaReR"] {
            let value = format!("{scheme} {}", "a".repeat(64));
            assert!(token.matches(&value), "accepted scheme {scheme}");
        }
        assert!(!token.matches(&format!("Basic {}", "a".repeat(64))));
        assert!(!token.matches(&format!("bearer {}", "A".repeat(64))));
    }
}
