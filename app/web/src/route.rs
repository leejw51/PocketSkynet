//! Routing (DESIGN.md §4).
//!
//! Hand-rolled rather than pulled from `yew-router`: the whole route table is
//! seven shapes, the parser below is exhaustively unit-tested on the host, and
//! avoiding the dependency keeps the `.wasm` smaller — which is the entire
//! premise of this client.
//!
//! Real paths (History API), not hash fragments, because DESIGN.md §4 specifies
//! `/rooms/:id`; the server serves `index.html` for unknown paths.

use pocketskynet_core::RoomId;

/// Every addressable screen.
///
/// `NotFound` carries the offending path so the 404 screen can decide where its
/// CTA should go without re-reading `location`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `/login` — the only unauthenticated route.
    Login,
    /// `/rooms` — shell with an empty detail pane.
    Rooms,
    /// `/rooms/:id` — shell with the chat view.
    Room(RoomId),
    /// `/rooms/:id/members` — shell with the member roster.
    Members(RoomId),
    /// `/rooms/:id/gallery` — the room's media, as a grid.
    Gallery(RoomId),
    /// `/invitations` — the invitations inbox.
    Invitations,
    /// `/knowledge` — search everything, teach the server (docs/SEARCH.md).
    Knowledge,
    /// `/publish` — paid web hosting: publish a page, browse and prune the
    /// wall of hosted sites (docs/API.md §16.2).
    Publish,
    /// `/bank` — the universal wallet, a full screen since 2026-07 (it outgrew
    /// its dialog: six tabs, an agent chat and a portfolio don't fit a modal).
    Bank,
    /// `/operator` — the operator's file: clearance, standing orders, the
    /// trophy wall, and this server's ladder.
    Operator,
    /// `/settings` — settings and profile.
    Settings,
    /// Anything else.
    NotFound,
}

impl Route {
    /// Parse a URL path. Query strings and fragments are ignored — this app
    /// keeps no state in either, and silently accepting them means a shared
    /// link with a stray `?utm_source=` still lands on the right screen.
    pub fn parse(path: &str) -> Self {
        let path = path.split(['?', '#']).next().unwrap_or("");
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        match segments.as_slice() {
            // `/` is not a screen; it redirects to `/rooms` at the auth gate.
            [] => Route::Rooms,
            ["login"] => Route::Login,
            ["rooms"] => Route::Rooms,
            ["invitations"] => Route::Invitations,
            ["knowledge"] => Route::Knowledge,
            ["publish"] => Route::Publish,
            ["bank"] => Route::Bank,
            ["operator"] => Route::Operator,
            ["settings"] => Route::Settings,
            // A malformed room id is a 404, not a room screen that will 400 on
            // its first fetch: the id charset is validated by the newtype.
            ["rooms", id] => RoomId::new(id).map(Route::Room).unwrap_or(Route::NotFound),
            ["rooms", id, "members"] => RoomId::new(id)
                .map(Route::Members)
                .unwrap_or(Route::NotFound),
            ["rooms", id, "gallery"] => RoomId::new(id)
                .map(Route::Gallery)
                .unwrap_or(Route::NotFound),
            _ => Route::NotFound,
        }
    }

    /// The canonical path for this route, suitable for `pushState`.
    pub fn to_path(&self) -> String {
        match self {
            Route::Login => "/login".into(),
            Route::Rooms => "/rooms".into(),
            Route::Room(id) => format!("/rooms/{id}"),
            Route::Members(id) => format!("/rooms/{id}/members"),
            Route::Gallery(id) => format!("/rooms/{id}/gallery"),
            Route::Invitations => "/invitations".into(),
            Route::Knowledge => "/knowledge".into(),
            Route::Publish => "/publish".into(),
            Route::Bank => "/bank".into(),
            Route::Operator => "/operator".into(),
            Route::Settings => "/settings".into(),
            // A 404 has no canonical path; going "back" to it is meaningless.
            Route::NotFound => "/".into(),
        }
    }

    /// The room this route is about, if any. Used to decide which room the
    /// realtime layer should treat as focused.
    pub fn room_id(&self) -> Option<&RoomId> {
        match self {
            Route::Room(id) | Route::Members(id) | Route::Gallery(id) => Some(id),
            _ => None,
        }
    }

    /// Whether this route requires a session. Everything except `/login` and
    /// the 404 does.
    pub fn needs_auth(&self) -> bool {
        !matches!(self, Route::Login | Route::NotFound)
    }

    /// Which pane `.fn-panes[data-view]` should show on a narrow viewport
    /// (DESIGN.md §16). Both panes stay mounted; this only picks the visible one.
    pub fn pane_view(&self) -> &'static str {
        match self {
            Route::Room(_) => "chat",
            Route::Members(_) => "members",
            // The gallery replaces the chat pane for its room, exactly as the
            // roster does: on a phone it *is* the screen.
            Route::Gallery(_) => "members",
            Route::Invitations
            | Route::Settings
            | Route::Knowledge
            | Route::Publish
            | Route::Operator
            | Route::Bank => "settings",
            _ => "rooms",
        }
    }

    /// Which bottom-nav item is `aria-current="page"`.
    pub fn nav_key(&self) -> &'static str {
        match self {
            Route::Rooms => "rooms",
            Route::Room(_) => "chat",
            Route::Members(_) => "members",
            Route::Gallery(_) => "chat",
            Route::Invitations => "invites",
            Route::Knowledge => "knowledge",
            Route::Publish => "publish",
            Route::Bank => "bank",
            Route::Operator => "operator",
            Route::Settings => "settings",
            _ => "",
        }
    }

    /// The document title for this route. Set on every navigation so browser
    /// history and tab titles are meaningful.
    pub fn title(&self) -> &'static str {
        match self {
            Route::Login => "Sign in · PocketSkynet",
            Route::Rooms | Route::Room(_) => "PocketSkynet",
            Route::Members(_) => "Members · PocketSkynet",
            Route::Gallery(_) => "Gallery · PocketSkynet",
            Route::Invitations => "Invitations · PocketSkynet",
            Route::Knowledge => "Knowledge · PocketSkynet",
            Route::Publish => "Publish · PocketSkynet",
            Route::Bank => "Bank · PocketSkynet",
            Route::Operator => "Operator · PocketSkynet",
            Route::Settings => "Settings · PocketSkynet",
            Route::NotFound => "Page not found · PocketSkynet",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(s: &str) -> RoomId {
        RoomId::new(s).unwrap()
    }

    const ROOM: &str = "room_1749652739650_304e0eaf";

    #[test]
    fn every_documented_route_parses() {
        assert_eq!(Route::parse("/login"), Route::Login);
        assert_eq!(Route::parse("/"), Route::Rooms);
        assert_eq!(Route::parse("/rooms"), Route::Rooms);
        assert_eq!(
            Route::parse(&format!("/rooms/{ROOM}")),
            Route::Room(rid(ROOM))
        );
        assert_eq!(
            Route::parse(&format!("/rooms/{ROOM}/members")),
            Route::Members(rid(ROOM))
        );
        assert_eq!(
            Route::parse(&format!("/rooms/{ROOM}/gallery")),
            Route::Gallery(rid(ROOM))
        );
        assert_eq!(Route::parse("/invitations"), Route::Invitations);
        assert_eq!(Route::parse("/knowledge"), Route::Knowledge);
        assert_eq!(Route::parse("/publish"), Route::Publish);
        assert_eq!(Route::parse("/bank"), Route::Bank);
        assert_eq!(Route::parse("/settings"), Route::Settings);
    }

    #[test]
    fn unknown_paths_are_not_found() {
        for p in ["/nope", "/rooms/a/b/c", "/settings/deep", "/login/extra"] {
            assert_eq!(Route::parse(p), Route::NotFound, "for {p}");
        }
    }

    #[test]
    fn a_room_id_that_the_api_would_reject_is_a_404_here() {
        // The opaque-id newtype forbids path separators and traversal; a route
        // that accepted them would send `../etc/passwd` to the server.
        assert_eq!(Route::parse("/rooms/bad%20id"), Route::NotFound);
        assert_eq!(Route::parse("/rooms/"), Route::Rooms);
        // 129 chars exceeds the id bound.
        assert_eq!(
            Route::parse(&format!("/rooms/{}", "a".repeat(129))),
            Route::NotFound
        );
    }

    #[test]
    fn trailing_slashes_and_repeated_separators_are_tolerated() {
        assert_eq!(Route::parse("/rooms/"), Route::Rooms);
        assert_eq!(Route::parse("//rooms//"), Route::Rooms);
        assert_eq!(
            Route::parse(&format!("/rooms/{ROOM}/members/")),
            Route::Members(rid(ROOM))
        );
    }

    #[test]
    fn query_and_fragment_are_ignored() {
        assert_eq!(Route::parse("/settings?tab=1"), Route::Settings);
        assert_eq!(Route::parse("/settings#top"), Route::Settings);
        assert_eq!(Route::parse("/rooms?x=1#y"), Route::Rooms);
    }

    #[test]
    fn paths_round_trip() {
        let routes = [
            Route::Login,
            Route::Rooms,
            Route::Room(rid(ROOM)),
            Route::Members(rid(ROOM)),
            Route::Gallery(rid(ROOM)),
            Route::Invitations,
            Route::Knowledge,
            Route::Publish,
            Route::Bank,
            Route::Settings,
        ];
        for r in routes {
            assert_eq!(Route::parse(&r.to_path()), r, "round trip failed for {r:?}");
        }
    }

    #[test]
    fn auth_gate_covers_exactly_the_authenticated_routes() {
        assert!(!Route::Login.needs_auth());
        assert!(!Route::NotFound.needs_auth());
        assert!(Route::Rooms.needs_auth());
        assert!(Route::Room(rid(ROOM)).needs_auth());
        assert!(Route::Members(rid(ROOM)).needs_auth());
        assert!(Route::Gallery(rid(ROOM)).needs_auth());
        assert!(Route::Invitations.needs_auth());
        assert!(Route::Knowledge.needs_auth());
        assert!(Route::Publish.needs_auth());
        assert!(Route::Bank.needs_auth());
        assert!(Route::Settings.needs_auth());
    }

    #[test]
    fn room_id_is_exposed_only_by_room_scoped_routes() {
        assert_eq!(Route::Room(rid(ROOM)).room_id().unwrap().as_str(), ROOM);
        assert_eq!(Route::Members(rid(ROOM)).room_id().unwrap().as_str(), ROOM);
        assert_eq!(Route::Gallery(rid(ROOM)).room_id().unwrap().as_str(), ROOM);
        assert!(Route::Rooms.room_id().is_none());
        assert!(Route::Settings.room_id().is_none());
    }

    #[test]
    fn pane_view_hides_the_list_only_where_the_spec_says_it_should() {
        assert_eq!(Route::Rooms.pane_view(), "rooms");
        assert_eq!(Route::Room(rid(ROOM)).pane_view(), "chat");
        assert_eq!(Route::Members(rid(ROOM)).pane_view(), "members");
        assert_eq!(Route::Gallery(rid(ROOM)).pane_view(), "members");
        assert_eq!(Route::Settings.pane_view(), "settings");
        assert_eq!(Route::Invitations.pane_view(), "settings");
        assert_eq!(Route::Knowledge.pane_view(), "settings");
        assert_eq!(Route::Publish.pane_view(), "settings");
        assert_eq!(Route::Bank.pane_view(), "settings");
    }
}
