use http::HeaderValue;
use http::header::{HeaderMap, SET_COOKIE};

/// Removes `Domain=<domain>` (with or without a leading dot) from every
/// `Set-Cookie` header, leaving the cookies host-only.
pub(crate) fn strip_cookie_domains(headers: &mut HeaderMap, domain: &str) {
    if let http::header::Entry::Occupied(mut entry) = headers.entry(SET_COOKIE) {
        for value in entry.iter_mut() {
            if let Some(stripped) = value.to_str().ok().and_then(|v| strip_domain(v, domain))
                && let Ok(stripped) = HeaderValue::from_str(&stripped)
            {
                *value = stripped;
            }
        }
    }
}

/// The cookie without a matching `Domain` attribute, or `None` if it had none.
fn strip_domain(cookie: &str, domain: &str) -> Option<String> {
    let domain = domain.trim_start_matches('.');
    let mut parts = cookie.split(';');
    let mut kept = vec![parts.next()?];
    let mut stripped = false;
    for attribute in parts {
        let (name, value) = attribute.split_once('=').unwrap_or((attribute, ""));
        let matches = name.trim().eq_ignore_ascii_case("domain")
            && value
                .trim()
                .trim_start_matches('.')
                .eq_ignore_ascii_case(domain);
        if matches {
            stripped = true;
        } else {
            kept.push(attribute);
        }
    }
    stripped.then(|| kept.join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(cookie: &str) -> String {
        strip_domain(cookie, "share.example").unwrap_or_else(|| cookie.to_string())
    }

    #[test]
    fn strips_the_share_domain() {
        assert_eq!(
            strip("session=abc; Domain=share.example; Path=/"),
            "session=abc; Path=/"
        );
    }

    #[test]
    fn strips_with_a_leading_dot() {
        assert_eq!(
            strip("session=abc; Domain=.share.example; Path=/"),
            "session=abc; Path=/"
        );
    }

    #[test]
    fn matches_attribute_and_domain_case_insensitively() {
        assert_eq!(strip("a=1; DOMAIN=Share.Example"), "a=1");
        assert_eq!(strip("a=1; domain=.SHARE.example; secure"), "a=1; secure");
    }

    #[test]
    fn tolerates_whitespace_around_the_attribute() {
        assert_eq!(strip("a=1;  Domain = share.example ;Path=/"), "a=1;Path=/");
    }

    #[test]
    fn preserves_other_attributes_in_order() {
        assert_eq!(
            strip(
                "id=x; Path=/app; Domain=share.example; Expires=Wed, 21 Oct 2026 07:28:00 GMT; \
                 Max-Age=60; Secure; HttpOnly; SameSite=Lax"
            ),
            "id=x; Path=/app; Expires=Wed, 21 Oct 2026 07:28:00 GMT; Max-Age=60; Secure; \
             HttpOnly; SameSite=Lax"
        );
    }

    #[test]
    fn keeps_other_domains() {
        for cookie in [
            "a=1; Domain=app--feature-x--nova.share.example",
            "a=1; Domain=example",
            "a=1; Domain=notshare.example",
            "a=1; Domain=share.example.evil",
            "a=1",
        ] {
            assert_eq!(strip_domain(cookie, "share.example"), None, "{cookie}");
        }
    }

    #[test]
    fn leaves_the_cookie_value_alone() {
        assert_eq!(
            strip("domain=share.example; Domain=share.example"),
            "domain=share.example"
        );
    }

    #[test]
    fn strips_every_matching_attribute() {
        assert_eq!(
            strip("a=1; Domain=share.example; Domain=.share.example; Path=/"),
            "a=1; Path=/"
        );
    }

    #[test]
    fn accepts_a_configured_domain_with_a_leading_dot() {
        assert_eq!(
            strip_domain("a=1; Domain=share.example", ".share.example").as_deref(),
            Some("a=1")
        );
    }

    #[test]
    fn rewrites_every_set_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("a=1; Domain=share.example; Path=/"),
        );
        headers.append(SET_COOKIE, HeaderValue::from_static("b=2; Path=/"));
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("c=3; domain=.SHARE.EXAMPLE"),
        );
        headers.insert("x-other", HeaderValue::from_static("Domain=share.example"));

        strip_cookie_domains(&mut headers, "share.example");

        let cookies: Vec<_> = headers.get_all(SET_COOKIE).iter().collect();
        assert_eq!(cookies, ["a=1; Path=/", "b=2; Path=/", "c=3"]);
        assert_eq!(headers["x-other"], "Domain=share.example");
    }
}
