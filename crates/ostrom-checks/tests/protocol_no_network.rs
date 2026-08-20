use std::{fs, path::Path};

#[test]
fn production_install_path_has_no_network_capability() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source =
        fs::read_to_string(crate_root.join("src/protocol.rs")).expect("read protocol source");
    let forbidden_fragments = [
        ["std", "::net"].concat(),
        ["Tcp", "Stream"].concat(),
        ["Udp", "Socket"].concat(),
        ["req", "west"].concat(),
        ["hy", "per"].concat(),
        ["cu", "rl"].concat(),
        ["std", "::process"].concat(),
        ["Command", "::new"].concat(),
        ["g", "it://"].concat(),
        ["s", "sh://"].concat(),
    ];
    for fragment in forbidden_fragments {
        assert!(
            !source.contains(&fragment),
            "install module references network API {fragment}"
        );
    }
    assert!(source.contains("use ostrom_core::sha256_hex;"));
    assert!(!source.contains("use crate::"));
}
