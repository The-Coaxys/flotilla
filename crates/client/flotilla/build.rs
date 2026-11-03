//! Embed the musl `flotillad` binaries from FLOTILLA_EMBED_DAEMON_DIR (set by CI).
//! Without it, embed empty placeholders: such dev builds cannot install daemons.
use std::path::PathBuf;

const NAMES: [&str; 2] = [
    "flotillad-x86_64-unknown-linux-musl",
    "flotillad-aarch64-unknown-linux-musl",
];

fn main() {
    println!("cargo:rerun-if-env-changed=FLOTILLA_EMBED_DAEMON_DIR");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let dir = std::env::var_os("FLOTILLA_EMBED_DAEMON_DIR").map(PathBuf::from);
    for name in NAMES {
        let dst = out.join(name);
        match &dir {
            Some(dir) => {
                let src = dir.join(name);
                println!("cargo:rerun-if-changed={}", src.display());
                let bytes = std::fs::read(&src).unwrap_or_else(|e| {
                    panic!(
                        "FLOTILLA_EMBED_DAEMON_DIR is set but {} is unreadable: {e}",
                        src.display()
                    )
                });
                assert!(
                    bytes.starts_with(b"\x7fELF"),
                    "{} is not an ELF binary",
                    src.display()
                );
                std::fs::write(&dst, bytes).expect("write embedded daemon");
            }
            None => std::fs::write(&dst, b"").expect("write placeholder"),
        }
    }
}
