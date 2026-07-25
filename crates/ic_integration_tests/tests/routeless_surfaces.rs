//! The tripwire for the three dashboard panels that have no backend.
//!
//! Memory browser, audit log, and run history are in the Phase 2 plan and are
//! shown in the dashboard as **unavailable, with the reason** — listed rather
//! than faked, because the honest alternatives are worse:
//!
//! - **Memory** lives in the gateway's *private* libSQL store
//!   (`root_filesystem_entries WHERE path LIKE '/memory/%'`), reachable only
//!   from inside the agent loop through its `memory_*` tools.
//! - **Audit** lives in the same private store
//!   (`root_filesystem_events WHERE path LIKE '/events/audit/%'`) as an
//!   unversioned internal schema.
//! - **Run history** has no cross-thread enumeration at all; a conversation's
//!   own history is its Chats timeline.
//!
//! Reading either table directly would couple the widget to internals upstream
//! is free to change without notice — the coupling is the objection, not the
//! difficulty. See `docs/desktop/dashboard-gaps.md`, which names the three
//! routes that would unblock them; this test probes exactly those paths.
//!
//! ## Why this file exists
//!
//! The skills panel used to be on that list and left it in 8c — but it left by
//! the widget owning its data on disk, **not** by upstream shipping a route. So
//! nothing in the repo currently notices if a route does appear, and a
//! dashboard that says "unavailable" about something that has since become
//! available is a lie the user has no way to catch. This is the 8d pattern
//! applied to a second verified negative: assert the absence, so the day it
//! stops being absent the build says so instead of the UI going quietly stale.
//!
//! A failure here is **good news** — it means the panel can be built. Read the
//! new route's descriptor, build the panel, and delete the case from this file.
#![cfg(feature = "webui-v2-beta")]

use ironclaw_host_api::ingress::IngressRouteDescriptor;

/// The two run-scoped routes that legitimately exist. Both act on a run the
/// caller *already has the id of* (from its own thread's timeline), which is
/// precisely why they are not run history: neither can enumerate.
const KNOWN_RUN_ROUTES: &[&str] = &[
    "/api/webchat/v2/threads/{thread_id}/runs/{run_id}/cancel",
    "/api/webchat/v2/threads/{thread_id}/runs/{run_id}/gates/{gate_ref}/resolve",
];

/// Substrings that would mark a route as backing one of the three panels.
const PANEL_MARKERS: &[(&str, &str)] = &[
    ("memor", "memory browser"),
    ("audit", "audit log"),
    ("/runs", "run history"),
];

/// Which panel `pattern` would unblock, if any.
fn panel_for(pattern: &str) -> Option<&'static str> {
    let lower = pattern.to_ascii_lowercase();
    if KNOWN_RUN_ROUTES.contains(&pattern) {
        return None;
    }
    PANEL_MARKERS
        .iter()
        .find(|(marker, _)| lower.contains(marker))
        .map(|(_, panel)| *panel)
}

/// The runtime's own route table still carries nothing for the three panels.
///
/// Reads `ironclaw_webui_v2::webui_v2_routes()` — the canonical descriptor set
/// the host composes against, where "adding a new route requires a matching
/// descriptor" — rather than a list of our own that could agree with itself
/// forever.
#[test]
fn the_serve_route_table_still_backs_none_of_the_three_panels() {
    let routes: Vec<IngressRouteDescriptor> = ironclaw_webui_v2::webui_v2_routes();
    assert!(
        !routes.is_empty(),
        "the route table came back empty — this test would pass vacuously"
    );

    let found: Vec<String> = routes
        .iter()
        .filter_map(|route| {
            let pattern = route.route_pattern().as_str();
            panel_for(pattern).map(|panel| {
                format!(
                    "{panel}: {:?} {pattern} (route_id {})",
                    route.method(),
                    route.route_id().as_str()
                )
            })
        })
        .collect();

    assert!(
        found.is_empty(),
        "a route now exists for a panel the dashboard reports as unavailable. \
         This is good news — build the panel and delete its case here.\n  {}",
        found.join("\n  ")
    );

    // Guard the guard: the two run-scoped routes must still be present, or the
    // allow-list above has gone stale and is masking whatever replaced them.
    for known in KNOWN_RUN_ROUTES {
        assert!(
            routes
                .iter()
                .any(|route| route.route_pattern().as_str() == *known),
            "{known} is gone from the route table — the allow-list in this test \
             is stale and may now be hiding a real run route"
        );
    }
}

/// The live half: a running gateway 404s the three paths the dashboard would
/// call.
///
/// The descriptor check above cannot see routes mounted from *outside*
/// `webui_v2_routes()` — the product-auth mount and the host-supplied public
/// SSO mount both add routes at composition time. And the Phase 4 lesson stands:
/// a source trace is not a running gateway. So this asks the real thing.
///
/// The paths are the ones `docs/desktop/dashboard-gaps.md` names as what would
/// unblock each panel, so a `404` here is the same claim the doc makes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_gateway_404s_every_path_the_missing_panels_would_call() {
    use ic_integration_tests::RebornServer;

    let server = RebornServer::start().await;
    let thread = server.create_thread().await;
    let client = reqwest::Client::new();

    // (panel, path) — the routes dashboard-gaps.md says would unblock each one,
    // plus the per-thread shape run history could plausibly take instead.
    let probes = [
        ("memory browser", "/api/webchat/v2/memory".to_string()),
        (
            "memory browser",
            "/api/webchat/v2/memory?query=anything".to_string(),
        ),
        ("audit log", "/api/webchat/v2/audit".to_string()),
        ("run history", "/api/webchat/v2/runs".to_string()),
        (
            "run history",
            format!("/api/webchat/v2/runs?thread_id={thread}"),
        ),
        (
            "run history",
            format!("/api/webchat/v2/threads/{thread}/runs"),
        ),
    ];

    for (panel, path) in probes {
        let response = client
            .get(format!("{}{path}", server.base_url))
            .bearer_auth(&server.token)
            .send()
            .await
            .expect("the probe request should reach the gateway");
        let status = response.status();
        // Deliberately exact. A 200 means the panel can be built; a 401 or 405
        // means something *is* mounted there and only the method or the auth
        // differs — which is still a route, and still news.
        assert_eq!(
            status.as_u16(),
            404,
            "GET {path} answered {status} on a real gateway, so the {panel} may \
             have a backend now. Read its descriptor and build the panel.\n\
             --- serve stderr ---\n{}",
            server.stderr_snapshot()
        );
    }

    // The control: the same client, token, and base URL do reach a route that
    // exists. Without this, a typo in the base URL would 404 everything and the
    // test would pass while proving nothing.
    let control = client
        .get(format!("{}/api/webchat/v2/threads", server.base_url))
        .bearer_auth(&server.token)
        .send()
        .await
        .expect("the control request should reach the gateway");
    assert!(
        control.status().is_success(),
        "the control route failed ({}) — the 404s above prove nothing",
        control.status()
    );
}
