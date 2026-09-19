//! End-to-end checks that run the built binary directly - for behaviour that
//! lives at the CLI boundary (files left on disk) and has no single function
//! worth unit testing in isolation.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_linkscan"))
}

/// A minimal site with no outbound links, so a crawl of it finishes
/// immediately and deterministically - nothing here is testing the crawler
/// itself, only what happens to `--csv` once the run is done.
fn empty_site() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free loopback port");
    let addr = listener.local_addr().expect("listener has a local address");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0u8; 512];
            let _ = stream.read(&mut buffer);
            let body = "<html><body>hello</body></html>";
            let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}/")
}

#[test]
fn an_existing_csv_report_is_left_untouched_without_force() {
    let site = empty_site();
    let dir = std::env::temp_dir().join(format!("linkscan-force-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let csv_path = dir.join("out.csv");
    std::fs::write(&csv_path, "sentinel - do not overwrite me").expect("write sentinel");

    // --allow-internal because the fake site above is on loopback, which is
    // deliberately refused by default (see extract::is_blocked_address) -
    // that is a different finding, and this test should not depend on it.
    let output = bin()
        .arg(&site)
        .args(["--depth", "0", "--allow-internal", "--csv"])
        .arg(&csv_path)
        .output()
        .expect("run linkscan");

    let contents = std::fs::read_to_string(&csv_path).expect("csv file should still exist");
    assert_eq!(contents, "sentinel - do not overwrite me", "an existing --csv file must not be overwritten without --force");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--force"), "refusal should explain how to force the overwrite, got: {stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn force_overwrites_an_existing_csv_report() {
    let site = empty_site();
    let dir = std::env::temp_dir().join(format!("linkscan-force-test-yes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let csv_path = dir.join("out.csv");
    std::fs::write(&csv_path, "sentinel - do not overwrite me").expect("write sentinel");

    let status = bin()
        .arg(&site)
        .args(["--depth", "0", "--allow-internal", "--csv"])
        .arg(&csv_path)
        .arg("--force")
        .status()
        .expect("run linkscan");
    assert!(status.success());

    let contents = std::fs::read_to_string(&csv_path).expect("csv file should still exist");
    assert!(contents.starts_with("url,verdict,status,kind,found_on,detail"), "the real report should have been written over the sentinel: {contents:?}");

    std::fs::remove_dir_all(&dir).ok();
}
