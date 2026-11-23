//! `atlas://` links, e.g. from Reimagined: `atlas://install/map/<id>`,
//! `atlas://install/collection/<id>`, `atlas://open/map/<id>`, `atlas://open/collection/<id>`.
//! Any web page can open such a link, so a link only ever carries a public id, and Atlas asks
//! the player before installing anything.

use serde::Serialize;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct AtlasLink {
    pub action: &'static str,
    pub kind: &'static str,
    pub id: i64,
}

pub fn parse(link: &str) -> Option<AtlasLink> {
    if link.len() > 100 {
        return None;
    }
    let rest = link.strip_prefix("atlas://")?.trim_end_matches('/');
    let mut parts = rest.split('/');
    let action = match parts.next()? {
        "install" => "install",
        "open" => "open",
        _ => return None,
    };
    let kind = match parts.next()? {
        "map" => "map",
        "collection" => "collection",
        _ => return None,
    };
    let id = parts.next()?;
    let digits = (1..=10).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit()) && !id.starts_with('0');
    if !digits || parts.next().is_some() {
        return None;
    }
    Some(AtlasLink { action, kind, id: id.parse().ok()? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_links() {
        assert_eq!(parse("atlas://install/map/21355"), Some(AtlasLink { action: "install", kind: "map", id: 21355 }));
        assert_eq!(parse("atlas://open/collection/4029/"), Some(AtlasLink { action: "open", kind: "collection", id: 4029 }));
        for bad in [
            "atlas://install/map/0",
            "atlas://install/map/012",
            "atlas://install/map/12?x=1",
            "atlas://install/map/12/x",
            "atlas://install/map/12345678901",
            "atlas://delete/map/12",
            "atlas://install/player/12",
            "rhythia://map/12",
            "atlas://install/map/-1",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
