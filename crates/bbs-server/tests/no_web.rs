//! A server built without the web feature refuses `--web` rather than serve
//! without it.
#![cfg(not(feature = "web"))]

use std::process::Command;

#[test]
fn web_ui_needs_the_web_feature() {
    let home = std::env::temp_dir().join(format!("bbs-serve-{}-no-web", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_bbs-server"))
        .args(["--listen", "127.0.0.1:0", "--web", "127.0.0.1:0"])
        .env("HOME", &home)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bbs-server --features web"));
}
