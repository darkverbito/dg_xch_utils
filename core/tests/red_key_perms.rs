//! Red demonstration for docs/security-review-2026-09.md (finding 9). Fails until the fix
//! lands; run with `cargo test -p dg_xch_core --test red_key_perms -- --ignored`. When the fix
//! lands, remove the ignore and keep the test as the regression gate.

// Finding 9: private key material must never be readable by group or world — the writer must
// create key files owner-only rather than inheriting the ambient umask.
#[cfg(unix)]
#[test]
#[ignore = "red: finding 9 in docs/security-review-2026-09.md — key files land with ambient umask permissions"]
fn private_keys_are_owner_only() {
    use dg_xch_core::ssl::make_ca_cert;
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!(
        "red_key_perms_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cert_path = dir.join("ca.crt");
    let key_path = dir.join("ca.key");
    make_ca_cert(&cert_path, &key_path).expect("ca generates");
    let mode = std::fs::metadata(&key_path)
        .expect("key exists")
        .permissions()
        .mode();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        mode & 0o077,
        0,
        "the private key is group/world accessible (mode {mode:o}); it must be owner-only"
    );
}
