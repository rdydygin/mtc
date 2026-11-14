//! Recognize only routes underneath the signed-in server's exact origin and base path.
use url::Url;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    Post(String),
    Channel { team: String, name: String },
}

pub(crate) fn destination(server: &Url, link: &str) -> Option<Destination> {
    let link = Url::parse(link).ok()?;
    if !matches!(link.scheme(), "http" | "https")
        || link.origin() != server.origin()
        || !link.username().is_empty()
        || link.password().is_some()
    {
        return None;
    }
    let prefix = format!("{}/", server.path().trim_end_matches('/'));
    let path = link.path().strip_prefix(&prefix)?.trim_end_matches('/');
    let parts = path
        .split('/')
        .map(decode_segment)
        .collect::<Option<Vec<_>>>()?;
    match parts.as_slice() {
        [team, route, id] if !team.is_empty() && route == "pl" => {
            Some(Destination::Post(id.clone()))
        }
        [team, route, name] if team != "_redirect" && route == "channels" => {
            Some(Destination::Channel {
                team: team.clone(),
                name: name.clone(),
            })
        }
        _ => None,
    }
}

// Route names and Mattermost ids are ASCII. Decode once, rejecting separators,
// dot segments and escapes that could change the REST endpoint being requested.
fn decode_segment(segment: &str) -> Option<String> {
    let mut bytes = segment.bytes();
    let mut decoded = String::new();
    while let Some(b) = bytes.next() {
        let b = if b == b'%' {
            let hi = (bytes.next()? as char).to_digit(16)?;
            let lo = (bytes.next()? as char).to_digit(16)?;
            (hi * 16 + lo) as u8
        } else {
            b
        };
        if !b.is_ascii_alphanumeric() && !matches!(b, b'-' | b'_') {
            return None;
        }
        decoded.push(b as char);
    }
    (!decoded.is_empty()).then_some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonicalizes_origins_and_dot_segments() {
        for (base, link) in [
            (
                "https://chat.example/mm/",
                "https://CHAT.EXAMPLE:443/mm/t/pl/a",
            ),
            (
                "https://bücher.example/mm/",
                "https://xn--bcher-kva.example/mm/t/pl/a",
            ),
            (
                "https://chat.example/mm/",
                "https://chat.example/mm/x/../t/pl/a",
            ),
            (
                "https://chat.example/mm/",
                "https://chat.example/mm/x/%2e%2e/t/pl/a",
            ),
        ] {
            assert_eq!(
                destination(&Url::parse(base).unwrap(), link),
                Some(Destination::Post("a".into()))
            );
        }
        assert_eq!(
            destination(
                &Url::parse("https://chat.example/mm/").unwrap(),
                "https://chat.example/mm/../t/pl/a"
            ),
            None
        );
    }

    #[test]
    fn matches_only_server_routes() {
        for base in [
            "https://chat.example",
            "https://chat.example/",
            "https://chat.example/mm/",
            "https://chat.example/mm",
        ] {
            let server = Url::parse(base).unwrap();
            let prefix = base.trim_end_matches('/');
            for suffix in [
                "/team/pl/abc",
                "/_redirect/pl/abc/",
                "/team/pl/%61bc?q=x#reply",
            ] {
                assert_eq!(
                    destination(&server, &format!("{prefix}{suffix}")),
                    Some(Destination::Post("abc".into()))
                );
            }
            assert_eq!(
                destination(&server, &format!("{prefix}/team/channels/town-square")),
                Some(Destination::Channel {
                    team: "team".into(),
                    name: "town-square".into()
                })
            );
            for suffix in [
                "/team/pl/a%2fb",
                "/team/pl/%252f",
                "/team/pl/",
                "/team/pl/a/extra",
                "/team/pl/%3f",
                "/team/pl/%zz",
            ] {
                assert_eq!(destination(&server, &format!("{prefix}{suffix}")), None);
            }
        }
        let server = Url::parse("https://chat.example/mm/").unwrap();
        for url in [
            "https://chat.example.evil/mm/t/pl/a",
            "https://elsewhere/mm/t/pl/a",
            "http://chat.example/mm/t/pl/a",
            "https://chat.example:444/mm/t/pl/a",
            "https://chat.example/mm2/t/pl/a",
            "https://chat.example/t/pl/a",
            "https://chat.example@evil/mm/t/pl/a",
        ] {
            assert_eq!(destination(&server, url), None);
        }
    }
}
