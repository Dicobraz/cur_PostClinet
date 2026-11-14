//! Ссылки Matrix (ADR 0012 п. 4): `https://matrix.to/#/<комната|псевдоним|человек>[/<событие>]` и URI `matrix:`
//! (`matrix:roomid/…`, `matrix:r/…`, `matrix:u/…`, спецификация 1.2). Наружу уходит `matrix.to` — его открывает любой
//! клиент; человек для карточки контакта — `matrix:u/<user>:<server>` (vCard `IMPP`, JSContact `onlineServices`).

/// На что ссылка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `!room:server`.
    Room(String),
    /// `#alias:server`.
    Alias(String),
    /// `@user:server`.
    User(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub target: Target,
    /// `$event` — сообщение в комнате.
    pub event: Option<String>,
}

fn decode(s: &str) -> Option<String> {
    postman_core::channel::decode_segment(s)
}

/// Кодирование части `matrix.to`: `#` и `?` в id — процентами, `:`, `!`, `@`, `$` — как есть.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '#' if !out.is_empty() => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '/' => out.push_str("%2F"),
            ' ' => out.push_str("%20"),
            c => out.push(c),
        }
    }
    out
}

fn target(id: &str) -> Option<Target> {
    let valid = |s: &str| s.len() > 3 && s.contains(':') && !s.contains(char::is_whitespace);
    if !valid(id) {
        return None;
    }
    match id.chars().next()? {
        '!' => Some(Target::Room(id.to_owned())),
        '#' => Some(Target::Alias(id.to_owned())),
        '@' => Some(Target::User(id.to_owned())),
        _ => None,
    }
}

/// Разобрать ссылку; чужое — `None`.
pub fn parse(url: &str) -> Option<Link> {
    let u = url.trim();
    if let Some(rest) = ["https://matrix.to/#/", "http://matrix.to/#/", "matrix.to/#/"].iter().find_map(|p| u.strip_prefix(p)) {
        let path = rest.split('?').next()?;
        let mut parts = path.split('/').filter(|p| !p.is_empty());
        let target = target(&decode(parts.next()?)?)?;
        let event = parts.next().and_then(decode).filter(|e| e.starts_with('$'));
        if event.is_some() && !matches!(target, Target::Room(_) | Target::Alias(_)) {
            return None;
        }
        return Some(Link { target, event });
    }
    let rest = u.strip_prefix("matrix:")?;
    let path = rest.split(['?', '#']).next()?;
    let parts: Vec<&str> = path.split('/').collect();
    let (target, event) = match parts.as_slice() {
        ["roomid", id, tail @ ..] => (Target::Room(format!("!{}", decode(id)?)), tail),
        ["r", alias, tail @ ..] => (Target::Alias(format!("#{}", decode(alias)?)), tail),
        ["u", user] => (Target::User(format!("@{}", decode(user)?)), &[][..]),
        _ => return None,
    };
    let event = match event {
        ["e", e] => Some(format!("${}", decode(e)?)),
        [] => None,
        _ => return None,
    };
    let id = match &target {
        Target::Room(s) | Target::Alias(s) | Target::User(s) => s.clone(),
    };
    target_ok(&id).then_some(Link { target, event })
}

fn target_ok(id: &str) -> bool {
    target(id).is_some()
}

/// Ссылка на комнату или сообщение в ней.
pub fn room(room: &str, event: Option<&str>) -> String {
    match event {
        Some(e) => format!("https://matrix.to/#/{}/{}", encode(room), encode(e)),
        None => format!("https://matrix.to/#/{}", encode(room)),
    }
}

/// Человек для карточки контакта: `matrix:u/<user>:<server>`.
pub fn person(user: &str) -> Option<String> {
    let rest = user.strip_prefix('@')?;
    target_ok(user).then(|| format!("matrix:u/{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_forms() {
        let l = parse("https://matrix.to/#/!abc:example.test/$ev:example.test?via=example.test").unwrap();
        assert_eq!(l.target, Target::Room("!abc:example.test".into()));
        assert_eq!(l.event.as_deref(), Some("$ev:example.test"));
        assert_eq!(parse("https://matrix.to/#/%23team:example.test").unwrap().target, Target::Alias("#team:example.test".into()));
        assert_eq!(parse("https://matrix.to/#/@anna:example.test").unwrap().target, Target::User("@anna:example.test".into()));
        let m = parse("matrix:roomid/abc:example.test/e/ev1?via=example.test").unwrap();
        assert_eq!((m.target, m.event.as_deref()), (Target::Room("!abc:example.test".into()), Some("$ev1")));
        assert_eq!(parse("matrix:u/anna:example.test").unwrap().target, Target::User("@anna:example.test".into()));
        for bad in ["https://example.test/#/!a:b", "https://matrix.to/#/abc", "matrix:x/y", "https://matrix.to/#/@a:b/$e"] {
            assert!(parse(bad).is_none(), "{bad}");
        }
        assert_eq!(room("!abc:example.test", None), "https://matrix.to/#/!abc:example.test");
        assert_eq!(person("@anna:example.test").as_deref(), Some("matrix:u/anna:example.test"));
        assert_eq!(person("anna"), None);
    }
}
