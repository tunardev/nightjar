use nightjar_core::paths::Paths;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn serve_refuses_a_non_loopback_bind_before_touching_the_store_or_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::for_root(tmp.path());
    paths.ensure_dirs().unwrap();

    let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let err = nightjar_web::serve(addr, None, &paths).unwrap_err();
    assert!(err.to_string().contains("refusing to bind"), "got: {err}");
}

#[test]
fn serve_wires_a_real_store_file_through_to_a_live_request() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::for_root(tmp.path());
    paths.ensure_dirs().unwrap();
    std::fs::write(
        paths.jobs_dir.join("backup.toml"),
        "command = \"true\"\nschedule = \"hourly\"\n",
    )
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    thread::spawn(move || {
        nightjar_web::serve(addr, None, &paths).unwrap();
    });

    let url = format!("http://{addr}/");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut response = loop {
        match ureq::get(url.as_str()).call() {
            Ok(response) => break response,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "serve never answered a request before the deadline: {e}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    };

    assert_eq!(response.status().as_u16(), 200);
    let body = response.body_mut().read_to_string().unwrap();
    assert!(body.contains("backup"), "got: {body}");
}

#[test]
fn serve_on_an_already_bound_port_fails_cleanly_instead_of_panicking() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::for_root(tmp.path());
    paths.ensure_dirs().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let bg_paths = paths.clone();
    thread::spawn(move || {
        nightjar_web::serve(addr, None, &bg_paths).unwrap();
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(addr).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the first serve() never started listening on {addr} before the deadline"
        );
        thread::sleep(Duration::from_millis(20));
    }

    let err = nightjar_web::serve(addr, None, &paths).unwrap_err();
    assert!(err.to_string().contains("binding to"), "got: {err}");
}
