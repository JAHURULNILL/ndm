//! Slice 7 live witness: a Digest-protected file (RFC 7616)
//! downloads byte-identical through the public engine API —
//! MD5 and SHA-256 realms — and a wrong password fails
//! cleanly instead of looping or corrupting.
//!
//! Needs: python3 ~/workspace/ndm-rig/digest_server.py
//! (serves the rig's big.bin; user nill / s3cret-pw).
//! Run: cargo test --test digest_live -- --ignored --nocapture

use std::time::{Duration, Instant};

fn auth_with(pw: &str) -> ndm::Auth {
    let pw = Some(pw.to_string());
    ndm::Auth {
        // production also sends a prebuilt Basic header; the
        // Digest answer must replace it after the 401 dance
        authorization: Some("Basic bmlsbDp3cm9uZw==".to_string()),
        login: Some("nill".to_string()),
        password: pw,
        ..Default::default()
    }
}

fn run(url: &str, auth: ndm::Auth, tag: &str) -> Result<Vec<u8>, String> {
    let out = std::env::temp_dir().join(format!("ndm-digest-{tag}.bin"));
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(ndm::state_path_for(&out));
    let dl = ndm::start_with_auth(url, &out, 4, 1024 * 1024, auth)?;
    let t0 = Instant::now();
    loop {
        let p = dl.progress();
        if p.status == ndm::Status::Finished {
            return std::fs::read(&out).map_err(|e| e.to_string());
        }
        if p.status == ndm::Status::Error {
            return Err("download errored".to_string());
        }
        if t0.elapsed() > Duration::from_secs(60) {
            return Err("timed out".to_string());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[test]
#[ignore]
fn digest_protected_download_is_byte_identical() {
    let source = std::fs::read(
        std::path::Path::new(env!("HOME")).join("workspace/ndm-rig/www/big.bin"),
    )
    .expect("rig big.bin");
    for alg in ["MD5", "SHA-256"] {
        let url = format!("http://127.0.0.1:8794/big.bin?alg={alg}");
        let got = run(&url, auth_with("s3cret-pw"), alg).expect(alg);
        assert!(got == source, "{alg}: bytes differ");
        println!("{alg}: {} bytes true", got.len());
    }
    // wrong password: a clean failure, never a finished file
    let bad = run(
        "http://127.0.0.1:8794/big.bin?alg=MD5",
        auth_with("wrong-pw"),
        "badpw",
    );
    assert!(bad.is_err(), "wrong password must not download");
    println!("wrong password refused cleanly");
}
