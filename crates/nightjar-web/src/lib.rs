use std::fs::File;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use anyhow::{Result, bail};
use base64::Engine as _;
use nightjar_core::clock::{Clock, SystemClock};
use nightjar_core::paths::Paths;
use nightjar_store::Store;
use nightjar_store::run::Run;
use subtle::ConstantTimeEq as _;
use tiny_http::{Header, Method, Request, Response, ResponseBox, Server};
use zeroize::Zeroizing;

mod assets;
mod render;

pub const DEFAULT_PORT: u16 = 18734;

const MIN_TOKEN_LEN: usize = 32;

type SharedStore = Arc<Mutex<Store>>;

fn locked_store(store: &SharedStore) -> std::sync::MutexGuard<'_, Store> {
    store.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(PartialEq, Eq)]
enum PathMatch {
    Whole,
    Prefix,
}

struct Route {
    method: Method,
    path: &'static str,
    match_kind: PathMatch,
    handler: fn(&SharedStore, &Paths, &Request) -> ResponseBox,
}

const ROUTES: &[Route] = &[
    Route {
        method: Method::Get,
        path: "/",
        match_kind: PathMatch::Whole,
        handler: handle_jobs,
    },
    Route {
        method: Method::Get,
        path: "/runs",
        match_kind: PathMatch::Whole,
        handler: handle_runs,
    },
    Route {
        method: Method::Get,
        path: "/output/",
        match_kind: PathMatch::Prefix,
        handler: handle_output,
    },
    Route {
        method: Method::Get,
        path: "/status.json",
        match_kind: PathMatch::Whole,
        handler: handle_status_json,
    },
    Route {
        method: Method::Get,
        path: "/refresh.js",
        match_kind: PathMatch::Whole,
        handler: handle_refresh_js,
    },
    Route {
        method: Method::Get,
        path: "/style.css",
        match_kind: PathMatch::Whole,
        handler: handle_style_css,
    },
];

fn route_matches(route: &Route, path: &str) -> bool {
    match route.match_kind {
        PathMatch::Whole => route.path == path,
        PathMatch::Prefix => path.starts_with(route.path),
    }
}

fn handle_jobs(store: &SharedStore, paths: &Paths, _request: &Request) -> ResponseBox {
    let now = SystemClock.now();
    let store = locked_store(store);
    match render::jobs_page(&store, paths, now) {
        Ok(html) => html_response(html),
        Err(e) => server_error_for(&e),
    }
}

fn handle_runs(store: &SharedStore, paths: &Paths, request: &Request) -> ResponseBox {
    let now = SystemClock.now();
    let job = query_value(request.url(), "job").unwrap_or_default();
    let store = locked_store(store);
    match render::runs_page(&store, paths, &job, now) {
        Ok(Some(html)) => html_response(html),
        Ok(None) => not_found(),
        Err(e) => server_error_for(&e),
    }
}

const OUTPUT_PATH_PREFIX: &str = "/output/";

fn handle_output(store: &SharedStore, _paths: &Paths, request: &Request) -> ResponseBox {
    let path = path_without_query(request);
    let id = path.strip_prefix(OUTPUT_PATH_PREFIX).unwrap_or("");

    let Ok(Some(run)) = locked_store(store).get_run(id) else {
        return not_found();
    };

    let stream = query_value(request.url(), "stream");
    let file_path = match stream.as_deref() {
        Some("stderr") => run.stderr_path.as_deref(),
        _ => run.stdout_path.as_deref(),
    };
    serve_output(file_path)
}

fn handle_status_json(store: &SharedStore, _paths: &Paths, request: &Request) -> ResponseBox {
    let job = query_value(request.url(), "job");
    let reported_idle_streak: u32 = query_value(request.url(), "idle")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let (running, latest_run) = {
        let store = locked_store(store);
        let running = match store.running_runs() {
            Ok(runs) => job
                .as_deref()
                .map_or_else(|| !runs.is_empty(), |job| runs.iter().any(|r| r.job == job)),
            Err(e) => return server_error_for(&e),
        };
        match store.recent_runs(job.as_deref(), 1) {
            Ok(runs) => (running, runs.into_iter().next()),
            Err(e) => return server_error_for(&e),
        }
    };

    let idle_streak = if running {
        0
    } else {
        reported_idle_streak.saturating_add(1)
    };
    let next_poll_ms = poll_interval_ms(running, idle_streak);
    let body = serde_json::json!({
        "running": running,
        "idle_streak": idle_streak,
        "next_poll_ms": next_poll_ms,
        "latest_run": latest_run_token(latest_run.as_ref()),
    })
    .to_string();
    json_response(body)
}

fn latest_run_token(run: Option<&Run>) -> String {
    run.map_or_else(
        || "none".to_string(),
        |r| {
            format!(
                "{}:{}",
                r.id,
                r.finished_at.map_or(-1, jiff::Timestamp::as_millisecond)
            )
        },
    )
}

const RUNNING_POLL_MS: u64 = 3_000;
const IDLE_POLL_STEP_MS: u64 = 5_000;
const IDLE_MAX_POLL_MS: u64 = 300_000;

fn poll_interval_ms(running: bool, idle_streak: u32) -> u64 {
    if running {
        return RUNNING_POLL_MS;
    }
    IDLE_POLL_STEP_MS
        .saturating_mul(1u64 << idle_streak.min(6))
        .min(IDLE_MAX_POLL_MS)
}

fn handle_refresh_js(_store: &SharedStore, _paths: &Paths, _request: &Request) -> ResponseBox {
    Response::from_string(assets::SCRIPT)
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/javascript; charset=utf-8"[..])
                .expect("static header is valid ASCII"),
        )
        .boxed()
}

fn handle_style_css(_store: &SharedStore, _paths: &Paths, _request: &Request) -> ResponseBox {
    Response::from_string(assets::STYLE)
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/css; charset=utf-8"[..])
                .expect("static header is valid ASCII"),
        )
        .boxed()
}

fn json_response(body: String) -> ResponseBox {
    Response::from_string(body)
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header is valid ASCII"),
        )
        .boxed()
}

fn serve_output(path: Option<&Path>) -> ResponseBox {
    let Some(path) = path else {
        return output_text("no output\n");
    };
    match File::open(path) {
        Ok(file) => Response::from_file(file)
            .with_chunked_threshold(0)
            .with_header(text_plain_header())
            .boxed(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => output_text("output pruned\n"),
        Err(_) => output_text("error reading output\n"),
    }
}

fn output_text(body: &str) -> ResponseBox {
    Response::from_string(body.to_string())
        .with_header(text_plain_header())
        .boxed()
}

fn text_plain_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
        .expect("static header is valid ASCII")
}

fn nosniff_header() -> Header {
    Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..])
        .expect("static header is valid ASCII")
}

fn html_response(body: String) -> ResponseBox {
    Response::from_string(body)
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .expect("static header is valid ASCII"),
        )
        .boxed()
}

fn server_error() -> ResponseBox {
    Response::from_string("internal error")
        .with_status_code(500)
        .boxed()
}

fn server_error_for(e: &anyhow::Error) -> ResponseBox {
    if nightjar_store::is_database_busy(e) {
        Response::from_string("database busy, try again")
            .with_status_code(500)
            .boxed()
    } else {
        server_error()
    }
}

fn path_without_query(request: &Request) -> &str {
    request.url().split('?').next().unwrap_or("")
}

fn query_value(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key { percent_decode(v) } else { None }
    })
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..=i + 2]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) fn url_encode_query_value(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// # Errors
/// fails if the bind address is not loopback, or a token is shorter than 32 characters
pub fn check_bind(bind: SocketAddr, token: Option<&str>) -> Result<()> {
    if !bind.ip().is_loopback() {
        bail!(
            "refusing to bind {bind}: nightjar's embedded HTTP server cannot cap how much \
             memory or time an unauthenticated peer takes, so it listens on loopback only \
            ; a token does not change that. Reach it from another machine over an SSH \
             tunnel:\n\n    \
             ssh -L {port}:localhost:{port} <host>\n\n\
             then open http://localhost:{port} on your side of the tunnel",
            port = bind.port(),
        );
    }
    if let Some(token) = token
        && token.len() < MIN_TOKEN_LEN
    {
        bail!(
            "--token is {} characters, which is not enough to resist guessing from \
             another account on this machine; use at least {MIN_TOKEN_LEN} \
             (e.g. `openssl rand -hex 32`)",
            token.len(),
        );
    }
    Ok(())
}

/// # Errors
/// fails if the bind, token, store, or port is rejected
#[allow(clippy::needless_pass_by_value)]
pub fn serve(bind: SocketAddr, token: Option<String>, paths: &Paths) -> Result<()> {
    check_bind(bind, token.as_deref())?;
    let store: SharedStore = Arc::new(Mutex::new(Store::open(&paths.db_path)?));
    let server = Server::http(bind).map_err(|e| anyhow::anyhow!("binding to {bind}: {e}"))?;
    let note = if token.is_some() {
        " (token required)"
    } else {
        ""
    };
    eprintln!("nightjar: serving on http://{bind}{note}");
    serve_requests(&server, token.as_deref(), paths, &store);
    Ok(())
}

fn serve_requests(server: &Server, token: Option<&str>, paths: &Paths, store: &SharedStore) {
    for request in server.incoming_requests() {
        let paths = paths.clone();
        let store = Arc::clone(store);
        let token = token.map(|t| Zeroizing::new(t.to_string()));
        thread::spawn(move || {
            let response = dispatch(
                &store,
                &paths,
                token.as_deref().map(String::as_str),
                &request,
            );
            let _ = request.respond(response);
        });
    }
}

fn dispatch(
    store: &SharedStore,
    paths: &Paths,
    token: Option<&str>,
    request: &Request,
) -> ResponseBox {
    with_security_headers(dispatch_route(store, paths, token, request))
}

fn dispatch_route(
    store: &SharedStore,
    paths: &Paths,
    token: Option<&str>,
    request: &Request,
) -> ResponseBox {
    if !request_names_this_machine(request) {
        return foreign_host();
    }

    if let Some(expected) = token
        && !authorized(request, expected)
    {
        return unauthorized();
    }

    let path = path_without_query(request);
    let method = request.method();

    let route = ROUTES
        .iter()
        .find(|r| route_matches(r, path) && &r.method == method);
    let Some(route) = route else {
        return if ROUTES.iter().any(|r| route_matches(r, path)) {
            method_not_allowed()
        } else {
            not_found()
        };
    };

    (route.handler)(store, paths, request)
}

fn with_security_headers(response: ResponseBox) -> ResponseBox {
    response
        .with_header(content_security_policy_header())
        .with_header(nosniff_header())
        .with_header(referrer_policy_header())
        .with_header(no_store_header())
        .with_header(server_header())
}

fn no_store_header() -> Header {
    Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
        .expect("static header is valid ASCII")
}

fn request_names_this_machine(request: &Request) -> bool {
    request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Host"))
        .all(|h| is_loopback_authority(h.value.as_str()))
}

fn is_loopback_authority(host: &str) -> bool {
    let name = strip_port(host.trim());
    name.eq_ignore_ascii_case("localhost")
        || name.eq_ignore_ascii_case("localhost.")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn strip_port(authority: &str) -> &str {
    authority.strip_prefix('[').map_or_else(
        || {
            authority
                .split_once(':')
                .map_or(authority, |(name, _)| name)
        },
        |rest| rest.split_once(']').map_or(rest, |(inside, _)| inside),
    )
}

fn server_header() -> Header {
    Header::from_bytes(&b"Server"[..], &b"nightjar"[..]).expect("static header is valid ASCII")
}

fn content_security_policy_header() -> Header {
    Header::from_bytes(
        &b"Content-Security-Policy"[..],
        &b"default-src 'none'; script-src 'self'; style-src 'self'; \
           connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"[..],
    )
    .expect("static header is valid ASCII")
}

fn referrer_policy_header() -> Header {
    Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
        .expect("static header is valid ASCII")
}

fn authorized(request: &Request, expected_token: &str) -> bool {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| basic_auth_password(h.value.as_str()))
        .is_some_and(|given| constant_time_eq(given.as_bytes(), expected_token.as_bytes()))
}

fn basic_auth_password(header_value: &str) -> Option<String> {
    let (scheme, encoded) = header_value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (_, password) = decoded.split_once(':')?;
    Some(password.to_string())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    bool::from(a.ct_eq(b))
}

fn not_found() -> ResponseBox {
    Response::from_string("not found")
        .with_status_code(404)
        .boxed()
}

fn foreign_host() -> ResponseBox {
    Response::from_string(
        "forbidden: this server answers only to localhost, 127.0.0.1 or [::1] in the Host \
         header. Reach it through an SSH tunnel and open it as http://localhost:<port>.\n",
    )
    .with_status_code(403)
    .boxed()
}

fn method_not_allowed() -> ResponseBox {
    Response::from_string("method not allowed")
        .with_status_code(405)
        .boxed()
}

fn unauthorized() -> ResponseBox {
    Response::from_string("unauthorized")
        .with_status_code(401)
        .with_header(
            Header::from_bytes(&b"WWW-Authenticate"[..], &b"Basic realm=\"nightjar\""[..])
                .expect("static header is valid ASCII"),
        )
        .boxed()
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::Duration;

    use tiny_http::TestRequest;

    use super::*;

    fn get(path: &str) -> Request {
        TestRequest::new().with_path(path).into()
    }

    fn get_with_token(path: &str, token: &str) -> Request {
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!(":{token}"));
        TestRequest::new()
            .with_path(path)
            .with_header(
                Header::from_bytes(&b"Authorization"[..], format!("Basic {encoded}").as_bytes())
                    .unwrap(),
            )
            .into()
    }

    fn body_of(response: ResponseBox) -> Vec<u8> {
        let mut buf = Vec::new();
        response.into_reader().read_to_end(&mut buf).unwrap();
        buf
    }

    fn test_store(paths: &Paths) -> SharedStore {
        Arc::new(Mutex::new(Store::open(&paths.db_path).unwrap()))
    }

    fn dispatch(paths: &Paths, token: Option<&str>, request: &Request) -> ResponseBox {
        super::dispatch(&test_store(paths), paths, token, request)
    }

    #[test]
    fn loopback_needs_no_token() {
        assert!(check_bind("127.0.0.1:8080".parse().unwrap(), None).is_ok());
        assert!(check_bind("[::1]:8080".parse().unwrap(), None).is_ok());
    }

    #[test]
    fn bind_is_refused_when_address_is_not_loopback_and_no_token_is_given() {
        for addr in ["0.0.0.0:8080", "192.168.1.10:8080", "[::]:8080"] {
            assert!(
                check_bind(addr.parse().unwrap(), None).is_err(),
                "{addr} must be refused without a token"
            );
        }
    }

    #[test]
    fn bind_is_refused_when_address_is_not_loopback_even_with_a_valid_token() {
        let token = "a".repeat(MIN_TOKEN_LEN);
        for addr in ["0.0.0.0:8080", "192.168.1.10:8080", "[::]:8080"] {
            assert!(
                check_bind(addr.parse().unwrap(), Some(&token)).is_err(),
                "{addr} must be refused even with a valid token"
            );
        }
    }

    #[test]
    fn refusal_tells_the_user_how_to_proceed() {
        let err = check_bind("0.0.0.0:8080".parse().unwrap(), None).unwrap_err();
        assert!(err.to_string().contains("ssh -L"));
    }

    #[test]
    fn token_is_refused_when_it_is_too_short() {
        for weak in ["", " ", "abcd", &"x".repeat(MIN_TOKEN_LEN - 1)] {
            let err = check_bind("127.0.0.1:8080".parse().unwrap(), Some(weak))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("--token"),
                "a {}-character token must be refused before the socket opens, not \
                 accepted as if it protected anything",
                weak.len()
            );
        }
    }

    #[test]
    fn an_empty_password_never_satisfies_a_token() {
        let token = "k".repeat(MIN_TOKEN_LEN);
        assert!(!authorized(&get_with_token("/", ""), &token));

        let empty_pair = base64::engine::general_purpose::STANDARD.encode(":");
        let request: Request = TestRequest::new()
            .with_path("/")
            .with_header(
                Header::from_bytes(
                    &b"Authorization"[..],
                    format!("Basic {empty_pair}").as_bytes(),
                )
                .unwrap(),
            )
            .into();
        assert!(!authorized(&request, &token));
    }

    #[test]
    fn a_second_host_header_cannot_smuggle_a_foreign_origin_past_the_first() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();

        let request: Request = TestRequest::new()
            .with_path("/")
            .with_header(Header::from_bytes(&b"Host"[..], &b"localhost:18734"[..]).unwrap())
            .with_header(Header::from_bytes(&b"Host"[..], &b"evil.example"[..]).unwrap())
            .into();

        assert_eq!(dispatch(&paths, None, &request).status_code().0, 403);
    }

    #[test]
    fn serve_starts_when_the_daemon_is_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        assert!(
            !paths.lock_path.exists(),
            "the daemon must not have run for this test to prove anything"
        );

        let store = test_store(&paths);
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let addr = server.server_addr().to_ip().unwrap();
        let bg = Arc::clone(&server);
        let handle = thread::spawn(move || serve_requests(&bg, None, &paths, &store));

        let response = ureq::get(format!("http://{addr}/"))
            .config()
            .http_status_as_error(false)
            .build()
            .call()
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);

        server.unblock();
        handle.join().unwrap();
    }

    #[test]
    fn store_is_shared_across_requests_not_reopened_per_request() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        {
            let store = Store::open(&paths.db_path).unwrap();
            store
                .start_run(
                    "r1",
                    "backup",
                    nightjar_store::run::Trigger::Schedule,
                    "2026-06-01T00:00:00Z".parse().unwrap(),
                    Path::new("/tmp/o"),
                    Path::new("/tmp/e"),
                )
                .unwrap();
        }

        let shared: SharedStore = Arc::new(Mutex::new(Store::open(&paths.db_path).unwrap()));
        std::fs::remove_dir_all(tmp.path()).unwrap();

        let response = super::dispatch(&shared, &paths, None, &get("/output/r1"));
        assert_eq!(response.status_code().0, 200);
    }

    #[test]
    fn every_route_requires_the_token_when_one_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        let token = "b".repeat(MIN_TOKEN_LEN);

        for route in ROUTES {
            let request = get(route.path);
            let response = dispatch(&paths, Some(&token), &request);
            assert_eq!(
                response.status_code().0,
                401,
                "route {} answered without a token",
                route.path
            );

            let request = get_with_token(route.path, &token);
            let response = dispatch(&paths, Some(&token), &request);
            assert_ne!(
                response.status_code().0,
                401,
                "route {} refused the correct token",
                route.path
            );
        }
    }

    #[test]
    fn json_endpoint_serves_the_data_the_refresh_script_polls() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();

        let response = dispatch(&paths, None, &get("/status.json"));
        assert_eq!(response.status_code().0, 200);

        let content_type = response
            .headers()
            .iter()
            .find(|h| h.field.equiv("Content-Type"))
            .map(|h| h.value.as_str().to_string());
        assert_eq!(content_type.as_deref(), Some("application/json"));

        let body = String::from_utf8(body_of(response)).unwrap();
        assert!(body.contains("\"running\""), "got: {body}");
        assert!(body.contains("\"next_poll_ms\""), "got: {body}");
        assert!(body.contains("\"latest_run\""), "got: {body}");
    }

    #[test]
    fn style_css_is_served_same_origin_so_the_csp_can_forbid_inline_style() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();

        let response = dispatch(&paths, None, &get("/style.css"));
        assert_eq!(response.status_code().0, 200);
        let content_type = response
            .headers()
            .iter()
            .find(|h| h.field.equiv("Content-Type"))
            .map(|h| h.value.as_str().to_string());
        assert_eq!(content_type.as_deref(), Some("text/css; charset=utf-8"));
        assert_eq!(body_of(response), assets::STYLE.as_bytes());

        let csp = dispatch(&paths, None, &get("/"))
            .headers()
            .iter()
            .find(|h| h.field.equiv("Content-Security-Policy"))
            .map(|h| h.value.as_str().to_string())
            .unwrap();
        assert!(csp.contains("style-src 'self'"), "got: {csp}");
        assert!(!csp.contains("unsafe-inline"), "got: {csp}");
    }

    #[test]
    fn refresh_endpoint_requires_the_token_like_every_other_route() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        let token = "e".repeat(MIN_TOKEN_LEN);

        let response = dispatch(&paths, Some(&token), &get("/status.json"));
        assert_eq!(
            response.status_code().0,
            401,
            "the JSON endpoint answered without a token"
        );

        let response = dispatch(
            &paths,
            Some(&token),
            &get_with_token("/status.json", &token),
        );
        assert_eq!(response.status_code().0, 200);
    }

    #[test]
    fn polling_interval_backs_off_when_nothing_is_running() {
        assert_eq!(poll_interval_ms(true, 0), RUNNING_POLL_MS);
        assert_eq!(
            poll_interval_ms(true, 50),
            RUNNING_POLL_MS,
            "a long idle streak carried over from before a run started must not slow it down"
        );

        let first_idle = poll_interval_ms(false, 1);
        let later_idle = poll_interval_ms(false, 4);
        assert_eq!(first_idle, 10_000);
        assert!(
            first_idle >= RUNNING_POLL_MS,
            "idle polling must never be faster than active polling"
        );
        assert!(
            later_idle > first_idle,
            "a longer idle streak must back off further: {first_idle} then {later_idle}"
        );
        assert_eq!(
            poll_interval_ms(false, 100),
            IDLE_MAX_POLL_MS,
            "backoff must be capped, not grow without bound overnight"
        );
    }

    #[test]
    fn latest_run_token_changes_both_when_a_run_starts_and_when_it_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        let store = Store::open(&paths.db_path).unwrap();
        let t: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();

        let before = latest_run_token(store.recent_runs(None, 1).unwrap().first());

        store
            .start_run(
                "r1",
                "backup",
                nightjar_store::run::Trigger::Schedule,
                t,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        let mid = latest_run_token(store.recent_runs(None, 1).unwrap().first());
        assert_ne!(before, mid, "a newly started run must change the token");

        store
            .finish_run("r1", nightjar_store::run::RunStatus::Success, Some(0), t, 5)
            .unwrap();
        let after = latest_run_token(store.recent_runs(None, 1).unwrap().first());
        assert_ne!(
            mid, after,
            "a run finishing must change the token even though its id did not"
        );
    }

    #[test]
    fn token_is_rejected_on_every_route_when_it_is_wrong() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        let token = "c".repeat(MIN_TOKEN_LEN);
        let wrong = "d".repeat(MIN_TOKEN_LEN);

        for route in ROUTES {
            let request = get_with_token(route.path, &wrong);
            let response = dispatch(&paths, Some(&token), &request);
            assert_eq!(
                response.status_code().0,
                401,
                "route {} accepted the wrong token",
                route.path
            );
        }
    }

    #[test]
    fn token_comparison_is_constant_time() {
        assert!(constant_time_eq(b"matching", b"matching"));
        assert!(!constant_time_eq(b"matching", b"mismatch"));
        assert!(!constant_time_eq(b"short", b"a-lot-longer"));
        assert!(!constant_time_eq(b"", b"nonempty"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn token_never_appears_in_a_log_line_or_an_error_page() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        let real = "f".repeat(MIN_TOKEN_LEN);
        let guessed = "wrong-guess-value-not-the-real-token";

        let no_auth = dispatch(&paths, Some(&real), &get("/"));
        for header in no_auth.headers() {
            assert!(!header.value.as_str().contains(real.as_str()));
        }
        let no_auth_body = body_of(no_auth);
        assert!(!String::from_utf8_lossy(&no_auth_body).contains(real.as_str()));

        let bad_auth = dispatch(&paths, Some(&real), &get_with_token("/", guessed));
        for header in bad_auth.headers() {
            assert!(!header.value.as_str().contains(real.as_str()));
        }
        let bad_auth_body = body_of(bad_auth);
        let bad_auth_text = String::from_utf8_lossy(&bad_auth_body);
        assert!(!bad_auth_text.contains(real.as_str()));
        assert!(!bad_auth_text.contains(guessed));

        let err = check_bind("127.0.0.1:8080".parse().unwrap(), Some("weak-secret-value"))
            .unwrap_err()
            .to_string();
        assert!(!err.contains("weak-secret-value"));
    }

    #[test]
    fn no_route_answers_post_put_patch_or_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        let mutating = [Method::Post, Method::Put, Method::Patch, Method::Delete];

        for route in ROUTES {
            for method in &mutating {
                let request: Request = TestRequest::new()
                    .with_method(method.clone())
                    .with_path(route.path)
                    .into();
                let response = dispatch(&paths, None, &request);
                assert!(
                    !(200..300).contains(&response.status_code().0),
                    "{method} {} answered as if it were handled",
                    route.path
                );
            }
        }
    }

    #[test]
    fn every_route_is_a_get() {
        assert!(ROUTES.iter().all(|r| r.method == Method::Get));
    }

    #[test]
    fn path_is_not_found_when_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        let response = dispatch(&paths, None, &get("/nope"));
        assert_eq!(response.status_code().0, 404);
    }

    #[test]
    fn wrong_method_and_unknown_path_look_identical_when_no_token_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        let token = "z".repeat(MIN_TOKEN_LEN);

        let wrong_method: Request = TestRequest::new()
            .with_method(Method::Post)
            .with_path(ROUTES[0].path)
            .into();
        let unknown_path = get("/does-not-exist-at-all");

        let wrong_method_status = dispatch(&paths, Some(&token), &wrong_method)
            .status_code()
            .0;
        let unknown_path_status = dispatch(&paths, Some(&token), &unknown_path)
            .status_code()
            .0;

        assert_eq!(wrong_method_status, 401);
        assert_eq!(unknown_path_status, 401);
    }

    #[test]
    fn request_is_rejected_when_no_authorization_header_is_present() {
        let request = get("/");
        assert!(!authorized(&request, "sometoken"));
    }

    #[test]
    fn authorization_header_is_rejected_not_panicked_on_when_empty() {
        assert!(basic_auth_password("").is_none());
    }

    #[test]
    fn auth_scheme_is_rejected_when_wrong() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(":sometoken");
        assert!(basic_auth_password(&format!("Bearer {encoded}")).is_none());
    }

    #[test]
    fn basic_scheme_is_matched_case_insensitively() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(":sometoken");
        assert_eq!(
            basic_auth_password(&format!("basic {encoded}")),
            Some("sometoken".to_string())
        );
        assert_eq!(
            basic_auth_password(&format!("BASIC {encoded}")),
            Some("sometoken".to_string())
        );
    }

    #[test]
    fn base64_is_rejected_not_panicked_on_when_malformed() {
        assert!(basic_auth_password("Basic not-valid-base64!!!").is_none());
    }

    #[test]
    fn decoded_payload_is_rejected_not_panicked_on_when_it_is_not_valid_utf8() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([0xff, 0xfe, 0xfd]);
        assert!(basic_auth_password(&format!("Basic {encoded}")).is_none());
    }

    #[test]
    fn decoded_value_is_rejected_when_it_has_no_colon() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("justauser");
        assert!(basic_auth_password(&format!("Basic {encoded}")).is_none());
    }

    #[test]
    fn password_round_trips_whole_when_it_contains_a_colon() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:pa:ss");
        assert_eq!(
            basic_auth_password(&format!("Basic {encoded}")),
            Some("pa:ss".to_string())
        );
    }

    #[test]
    fn header_value_is_handled_without_panicking_when_it_is_very_large() {
        let huge_password = "x".repeat(200_000);
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("user:{huge_password}"));
        assert_eq!(
            basic_auth_password(&format!("Basic {encoded}")),
            Some(huge_password)
        );
    }

    fn get_with_host(path: &str, host: &str) -> Request {
        TestRequest::new()
            .with_path(path)
            .with_header(Header::from_bytes(&b"Host"[..], host.as_bytes()).unwrap())
            .into()
    }

    #[test]
    fn request_is_refused_when_the_host_header_names_something_other_than_loopback() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();

        for host in [
            "rebind.attacker.example",
            "rebind.attacker.example:18734",
            "nightjar.internal",
            "192.168.1.10:18734",
            "[2001:db8::1]:18734",
        ] {
            let response = dispatch(&paths, None, &get_with_host("/", host));
            assert_eq!(
                response.status_code().0,
                403,
                "a page on any origin can point {host:?} at 127.0.0.1 and then read \
                 these responses as same-origin; the Host header is the only thing \
                 that tells the two apart"
            );
        }
    }

    #[test]
    fn output_is_refused_too_when_the_host_header_is_not_loopback() {
        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("r1.out");
        std::fs::write(&out_path, b"captured secrets\n").unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        let store = Store::open(&paths.db_path).unwrap();
        store
            .start_run(
                "r1",
                "backup",
                nightjar_store::run::Trigger::Schedule,
                "2026-06-01T00:00:00Z".parse().unwrap(),
                &out_path,
                Path::new("/tmp/e"),
            )
            .unwrap();

        let response = dispatch(&paths, None, &get_with_host("/output/r1", "evil.example"));
        assert_eq!(response.status_code().0, 403);
        assert!(!String::from_utf8_lossy(&body_of(response)).contains("captured secrets"));
    }

    #[test]
    fn every_way_of_naming_this_machine_is_still_served() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();

        for host in [
            "localhost",
            "localhost:18734",
            "LocalHost:18734",
            "127.0.0.1",
            "127.0.0.1:18734",
            "127.0.0.53:18734",
            "[::1]",
            "[::1]:18734",
        ] {
            let response = dispatch(&paths, None, &get_with_host("/", host));
            assert_eq!(
                response.status_code().0,
                200,
                "{host:?} names this machine and must still be served"
            );
        }
    }

    #[test]
    fn absent_host_header_is_served_because_no_browser_lets_a_page_omit_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();

        let response = dispatch(&paths, None, &get("/"));
        assert_eq!(response.status_code().0, 200);
    }

    #[test]
    fn refusing_a_foreign_host_leaks_nothing_about_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        let token = "h".repeat(MIN_TOKEN_LEN);

        let response = dispatch(&paths, Some(&token), &get_with_host("/", "evil.example"));
        assert_eq!(response.status_code().0, 403);
        let body = String::from_utf8(body_of(response)).unwrap();
        assert!(!body.contains(token.as_str()), "got: {body}");
    }

    fn header(response: &ResponseBox, name: &'static str) -> Option<String> {
        response
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    }

    #[test]
    fn responses_carry_conservative_security_headers() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        let token = "f".repeat(MIN_TOKEN_LEN);

        let responses = [
            dispatch(&paths, None, &get("/")),
            dispatch(&paths, None, &get("/nope")),
            dispatch(&paths, Some(&token), &get("/")),
        ];
        for response in &responses {
            assert_eq!(
                header(response, "X-Content-Type-Options").as_deref(),
                Some("nosniff")
            );
            assert_eq!(
                header(response, "Referrer-Policy").as_deref(),
                Some("no-referrer")
            );
            assert_eq!(
                header(response, "Cache-Control").as_deref(),
                Some("no-store"),
                "a token-protected live view must not be left in a browser's disk cache"
            );
            let csp = header(response, "Content-Security-Policy")
                .expect("every response must carry a CSP");
            assert!(csp.contains("default-src 'none'"), "got: {csp}");
            assert!(csp.contains("frame-ancestors 'none'"), "got: {csp}");

            let server = header(response, "Server").unwrap_or_default();
            assert!(
                !server.to_lowercase().contains("tiny") && !server.to_lowercase().contains("rust"),
                "leaked the HTTP library or language: {server:?}"
            );
        }
    }

    #[test]
    fn request_does_not_panic_the_server_when_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();

        for path in ["/output/%gg", "/output/%", "/output/abc%", "/output/%%%"] {
            let response = dispatch(&paths, None, &get(path));
            assert_eq!(response.status_code().0, 404, "path {path:?}");
        }

        let huge_path = format!("/{}", "a".repeat(500_000));
        let response = dispatch(&paths, None, &get(&huge_path));
        assert_eq!(response.status_code().0, 404);

        let mut request = TestRequest::new().with_path("/");
        for i in 0..5_000 {
            request = request.with_header(
                Header::from_bytes(format!("X-Filler-{i}").as_bytes(), &b"1"[..]).unwrap(),
            );
        }
        let response = dispatch(&paths, None, &request.into());
        assert_eq!(response.status_code().0, 200);

        let store = test_store(&paths);
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let addr = server.server_addr().to_ip().unwrap();
        let bg = Arc::clone(&server);
        let handle = thread::spawn(move || serve_requests(&bg, None, &paths, &store));

        let huge_target = format!("GET /{} HTTP/1.1\r\nHost: x\r\n\r\n", "a".repeat(200_000));
        let raw_payloads: Vec<Vec<u8>> = vec![
            b"GET /output/".to_vec(),
            b"GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 999999\r\n\r\n".to_vec(),
            huge_target.into_bytes(),
            b"\x00\x01\x02 not even close to an HTTP request\r\n\r\n".to_vec(),
        ];
        for raw in &raw_payloads {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let _ = stream.write_all(raw);
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf);
        }

        let response = ureq::get(format!("http://{addr}/"))
            .config()
            .http_status_as_error(false)
            .build()
            .call()
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            200,
            "the server must still answer a well-formed request after the above"
        );

        server.unblock();
        handle.join().unwrap();
    }

    #[test]
    fn client_cannot_hold_a_thread_forever_when_slow() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();

        let store = test_store(&paths);
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let addr = server.server_addr().to_ip().unwrap();
        let bg = Arc::clone(&server);
        let handle = thread::spawn(move || serve_requests(&bg, None, &paths, &store));

        let stuck = TcpStream::connect(addr).unwrap();

        let response = ureq::get(format!("http://{addr}/"))
            .config()
            .http_status_as_error(false)
            .build()
            .call()
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            200,
            "a stalled connection must not block an independent, well-formed request"
        );

        drop(stuck);
        server.unblock();
        handle.join().unwrap();
    }

    #[test]
    fn error_pages_do_not_leak_filesystem_paths_or_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        let tmp_path_str = tmp.path().display().to_string();
        let version = env!("CARGO_PKG_VERSION");
        let token = "g".repeat(MIN_TOKEN_LEN);

        let not_found = dispatch(&paths, None, &get("/does-not-exist"));
        let unauthorized = dispatch(&paths, Some(&token), &get("/"));

        let store = Store::open(&paths.db_path).unwrap();
        store.drop_table_for_testing("runs").unwrap();
        let internal_error = dispatch(&paths, None, &get("/status.json"));
        assert_eq!(internal_error.status_code().0, 500);

        for response in [not_found, unauthorized, internal_error] {
            let headers: Vec<String> = response
                .headers()
                .iter()
                .map(|h| h.value.as_str().to_string())
                .collect();
            let body = String::from_utf8(body_of(response)).unwrap();

            assert!(
                !body.contains(&tmp_path_str) && !headers.iter().any(|h| h.contains(&tmp_path_str)),
                "leaked a filesystem path: body={body:?} headers={headers:?}"
            );
            assert!(
                !body.contains(version) && !headers.iter().any(|h| h.contains(version)),
                "leaked the crate version: body={body:?} headers={headers:?}"
            );
        }
    }

    #[test]
    fn database_busy_error_gets_a_distinguishing_message() {
        let busy = rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY);
        let busy = anyhow::Error::new(rusqlite::Error::SqliteFailure(busy, None));
        let response = server_error_for(&busy);
        assert_eq!(response.status_code().0, 500);
        let body = String::from_utf8(body_of(response)).unwrap();
        assert!(body.contains("busy"), "got: {body}");

        let generic = anyhow::anyhow!("some unrelated failure");
        let response = server_error_for(&generic);
        assert_eq!(response.status_code().0, 500);
        let body = String::from_utf8(body_of(response)).unwrap();
        assert!(!body.contains("busy"), "got: {body}");
    }

    mod output_serving {
        use jiff::Timestamp;
        use nightjar_store::run::{RunStatus, Trigger};

        use super::*;

        fn ts(s: &str) -> Timestamp {
            s.parse().unwrap()
        }

        fn store_with_run(tmp: &std::path::Path, out_path: &std::path::Path) -> (Paths, Store) {
            let paths = Paths::for_root(tmp);
            paths.ensure_dirs().unwrap();
            let store = Store::open(&paths.db_path).unwrap();
            store
                .start_run(
                    "r1",
                    "backup",
                    Trigger::Schedule,
                    ts("2026-06-01T00:00:00Z"),
                    out_path,
                    Path::new("/does/not/matter.err"),
                )
                .unwrap();
            (paths, store)
        }

        #[test]
        fn output_is_reachable_by_run_id() {
            let tmp = tempfile::tempdir().unwrap();
            let out_path = tmp.path().join("r1.out");
            std::fs::write(&out_path, b"hello from backup\n").unwrap();
            let (paths, store) = store_with_run(tmp.path(), &out_path);
            store
                .finish_run(
                    "r1",
                    RunStatus::Success,
                    Some(0),
                    ts("2026-06-01T00:00:01Z"),
                    18,
                )
                .unwrap();

            let response = dispatch(&paths, None, &get("/output/r1"));
            assert_eq!(response.status_code().0, 200);
            assert_eq!(body_of(response), b"hello from backup\n");
        }

        #[test]
        fn no_url_input_reaches_the_filesystem_as_a_path() {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::for_root(tmp.path());
            paths.ensure_dirs().unwrap();

            for id in [
                "../../etc/passwd",
                "..%2f..%2fetc%2fpasswd",
                "....//etc/passwd",
                "/etc/passwd",
            ] {
                let response = dispatch(&paths, None, &get(&format!("/output/{id}")));
                assert_eq!(
                    response.status_code().0,
                    404,
                    "id {id:?} must 404 as an unknown run, not resolve"
                );
            }
        }

        #[test]
        fn output_says_pruned_when_the_file_has_been_pruned() {
            let tmp = tempfile::tempdir().unwrap();
            let gone = tmp.path().join("gone.out");
            let (paths, store) = store_with_run(tmp.path(), &gone);
            store
                .finish_run(
                    "r1",
                    RunStatus::Success,
                    Some(0),
                    ts("2026-06-01T00:00:01Z"),
                    10,
                )
                .unwrap();

            let response = dispatch(&paths, None, &get("/output/r1"));
            assert_eq!(response.status_code().0, 200);
            let body = String::from_utf8(body_of(response)).unwrap();
            assert!(
                body.contains("pruned"),
                "a pruned run must say so, not imply it produced nothing: {body}"
            );
        }

        #[test]
        fn output_is_paged_and_does_not_load_entirely_into_memory_when_the_file_is_large() {
            let tmp = tempfile::tempdir().unwrap();
            let out_path = tmp.path().join("big.out");
            let content = vec![b'x'; 5 * 1024 * 1024];
            std::fs::write(&out_path, &content).unwrap();
            let (paths, store) = store_with_run(tmp.path(), &out_path);
            store
                .finish_run(
                    "r1",
                    RunStatus::Success,
                    Some(0),
                    ts("2026-06-01T00:00:01Z"),
                    content.len() as u64,
                )
                .unwrap();

            let response = dispatch(&paths, None, &get("/output/r1"));
            assert_eq!(response.status_code().0, 200);
            assert_eq!(response.data_length(), Some(content.len()));
            assert_eq!(body_of(response).len(), content.len());
        }

        #[test]
        fn output_is_served_with_a_content_type_that_cannot_execute() {
            let tmp = tempfile::tempdir().unwrap();
            let out_path = tmp.path().join("r1.out");
            std::fs::write(&out_path, b"<script>alert(1)</script>").unwrap();
            let (paths, store) = store_with_run(tmp.path(), &out_path);
            store
                .finish_run(
                    "r1",
                    RunStatus::Success,
                    Some(0),
                    ts("2026-06-01T00:00:01Z"),
                    10,
                )
                .unwrap();

            let response = dispatch(&paths, None, &get("/output/r1"));
            let content_type = response
                .headers()
                .iter()
                .find(|h| h.field.equiv("Content-Type"))
                .map(|h| h.value.as_str().to_string());
            assert_eq!(content_type.as_deref(), Some("text/plain; charset=utf-8"));

            let nosniff = response
                .headers()
                .iter()
                .any(|h| h.field.equiv("X-Content-Type-Options") && h.value.as_str() == "nosniff");
            assert!(nosniff, "missing X-Content-Type-Options: nosniff");

            assert_eq!(body_of(response), b"<script>alert(1)</script>");
        }

        #[test]
        fn served_output_is_redacted() {
            let tmp = tempfile::tempdir().unwrap();
            let marker = std::str::from_utf8(nightjar_config::redact::MARKER).unwrap();
            let out_path = tmp.path().join("r1.out");
            std::fs::write(&out_path, format!("connecting with password={marker}\n")).unwrap();
            let (paths, store) = store_with_run(tmp.path(), &out_path);
            store
                .finish_run(
                    "r1",
                    RunStatus::Success,
                    Some(0),
                    ts("2026-06-01T00:00:01Z"),
                    10,
                )
                .unwrap();

            let response = dispatch(&paths, None, &get("/output/r1"));
            let body = String::from_utf8(body_of(response)).unwrap();
            assert!(
                body.contains(marker),
                "the marker already on disk must reach the response unchanged: {body}"
            );
        }

        #[test]
        fn run_says_so_when_it_has_no_recorded_output_path() {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::for_root(tmp.path());
            paths.ensure_dirs().unwrap();
            let store = Store::open(&paths.db_path).unwrap();
            store
                .record_missed_run(
                    "m1",
                    "backup",
                    Trigger::Schedule,
                    ts("2026-06-01T00:00:00Z"),
                )
                .unwrap();

            let response = dispatch(&paths, None, &get("/output/m1"));
            assert_eq!(response.status_code().0, 200);
            let body = String::from_utf8(body_of(response)).unwrap();
            assert!(body.contains("no output"), "got: {body}");
        }

        #[test]
        fn stream_query_parameter_selects_stderr() {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::for_root(tmp.path());
            paths.ensure_dirs().unwrap();
            let store = Store::open(&paths.db_path).unwrap();
            let out_path = tmp.path().join("r1.out");
            let err_path = tmp.path().join("r1.err");
            std::fs::write(&out_path, b"stdout body").unwrap();
            std::fs::write(&err_path, b"stderr body").unwrap();
            store
                .start_run(
                    "r1",
                    "backup",
                    Trigger::Schedule,
                    ts("2026-06-01T00:00:00Z"),
                    &out_path,
                    &err_path,
                )
                .unwrap();
            store
                .finish_run(
                    "r1",
                    RunStatus::Failure,
                    Some(1),
                    ts("2026-06-01T00:00:01Z"),
                    11,
                )
                .unwrap();

            let response = dispatch(&paths, None, &get("/output/r1?stream=stderr"));
            assert_eq!(body_of(response), b"stderr body");
        }

        #[test]
        fn served_output_never_declares_a_length_a_growing_file_can_break() {
            let tmp = tempfile::tempdir().unwrap();
            let out_path = tmp.path().join("r1.out");
            std::fs::write(&out_path, "x".repeat(100)).unwrap();
            let (paths, store) = store_with_run(tmp.path(), &out_path);
            store
                .finish_run(
                    "r1",
                    RunStatus::Success,
                    Some(0),
                    ts("2026-06-01T00:00:01Z"),
                    100,
                )
                .unwrap();

            let response = dispatch(&paths, None, &get("/output/r1"));

            std::fs::OpenOptions::new()
                .append(true)
                .open(&out_path)
                .unwrap()
                .write_all(b"appended after the response object already exists")
                .unwrap();

            let mut wire = Vec::new();
            response
                .raw_print(&mut wire, tiny_http::HTTPVersion(1, 1), &[], false, None)
                .unwrap();

            let boundary = wire
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("a response always has a header/body boundary");
            let headers = std::str::from_utf8(&wire[..boundary]).unwrap();
            let body_len = wire.len() - (boundary + 4);
            let declared: Option<usize> = headers
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .map(|l| l.split_once(':').unwrap().1.trim().parse().unwrap());

            assert!(
                declared.is_none_or(|n| n == body_len),
                "declared {declared:?} bytes but the wire actually carries {body_len}"
            );
        }
    }
}
