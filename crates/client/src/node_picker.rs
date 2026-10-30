//! Adding a mailbox node to the delivery routes (spec §8 node picker):
//! by address, from the nodes found on this network, or by QR code -- a
//! picture of the code a node prints when it starts, read here.

pub use antiphon_relay::node_link::NodeLink;

use crate::delivery::{RouteConfig, RouteKind};

/// The node link in a picture of its QR code, or why there isn't one.
pub fn read_qr(path: &std::path::Path) -> Result<NodeLink, String> {
    let picture = image::open(path).map_err(|e| format!("That picture couldn't be opened: {e}"))?.to_luma8();
    let (w, h) = picture.dimensions();
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(w as usize, h as usize, |x, y| picture.get_pixel(x as u32, y as u32).0[0]);
    for grid in prepared.detect_grids() {
        if let Ok((_, text)) = grid.decode() {
            if let Some(link) = NodeLink::parse(&text) {
                return Ok(link);
            }
            return Err("That QR code isn't a mailbox node's".into());
        }
    }
    Err("No QR code found in that picture".into())
}

/// A node from what was typed: a node link, or a bare `host:port`.
pub fn parse(text: &str) -> Option<NodeLink> {
    let text = text.trim();
    if let Some(link) = NodeLink::parse(text) {
        return Some(link);
    }
    let (host, port) = text.rsplit_once(':')?;
    (!host.is_empty() && port.parse::<u16>().is_ok() && !text.contains(char::is_whitespace)).then(|| NodeLink { address: text.to_string(), code: String::new() })
}

/// Puts `node` on the right route: a community node when it has a
/// members code (which that route then carries), a node you trust when
/// it doesn't. Returns the route, or why it wasn't added.
pub fn add(routes: &mut Vec<RouteConfig>, node: &NodeLink) -> Result<RouteKind, String> {
    crate::delivery::normalize(routes);
    let kind = if node.code.is_empty() { RouteKind::TrustedNode } else { RouteKind::CommunityNode };
    if routes.iter().any(|r| r.nodes.iter().any(|n| n.eq_ignore_ascii_case(&node.address))) {
        return Err(format!("{} is already on your list", node.address));
    }
    let route = routes.iter_mut().find(|r| r.kind == kind).expect("normalize puts every route in");
    if kind == RouteKind::CommunityNode && !route.code.is_empty() && route.code != node.code {
        return Err("Your community route already has a different members code; remove its nodes first".into());
    }
    route.nodes.push(node.address.clone());
    if kind == RouteKind::CommunityNode {
        route.code.clone_from(&node.code);
    }
    route.enabled = true;
    Ok(kind)
}

/// Takes a node off whichever route has it.
pub fn remove(routes: &mut [RouteConfig], address: &str) {
    for r in routes.iter_mut() {
        r.nodes.retain(|n| n != address);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_goes_on_the_route_its_code_says() {
        let mut routes = crate::delivery::default_routes();
        let open = parse("mail.example.org:7000").unwrap();
        assert_eq!(add(&mut routes, &open), Ok(RouteKind::TrustedNode));
        assert!(add(&mut routes, &open).is_err(), "not twice");
        let members = NodeLink { address: "club.example.org:7000".into(), code: "choir".into() };
        assert_eq!(add(&mut routes, &members), Ok(RouteKind::CommunityNode));
        let community = routes.iter().find(|r| r.kind == RouteKind::CommunityNode).unwrap();
        assert_eq!((community.nodes.len(), community.code.as_str()), (1, "choir"));
        let other = NodeLink { address: "other.example.org:7000".into(), code: "else".into() };
        assert!(add(&mut routes, &other).is_err());
        remove(&mut routes, "club.example.org:7000");
        assert!(routes.iter().all(|r| !r.nodes.iter().any(|n| n == "club.example.org:7000")));
        assert_eq!(parse("not an address"), None);
    }

    #[test]
    fn a_node_is_read_from_a_picture_of_its_code() {
        let link = NodeLink { address: "mail.example.org:7000".into(), code: String::new() };
        let code = qrcode_for_test(&link.to_link());
        let path = std::env::temp_dir().join(format!("antiphon-node-qr-{}.png", std::process::id()));
        code.save(&path).unwrap();
        let read = read_qr(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(read, Ok(link));
    }

    /// A QR picture drawn from scratch, 8 pixels a module with a quiet
    /// zone: what a phone photo of a node's screen comes down to.
    fn qrcode_for_test(text: &str) -> image::GrayImage {
        let code = qrcode::QrCode::new(text.as_bytes()).unwrap();
        let n = code.width();
        let scale = 8;
        let quiet = 4;
        let size = ((n + 2 * quiet) * scale) as u32;
        image::GrayImage::from_fn(size, size, |x, y| {
            let (mx, my) = ((x as usize / scale).wrapping_sub(quiet), (y as usize / scale).wrapping_sub(quiet));
            let dark = mx < n && my < n && code[(mx, my)] == qrcode::Color::Dark;
            image::Luma([if dark { 0 } else { 255 }])
        })
    }
}
