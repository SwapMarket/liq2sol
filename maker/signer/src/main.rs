//! `liq2sol-signer <config.json>`: serve signing requests on a private Unix socket until stopped.

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: liq2sol-signer <config.json>");
        std::process::exit(2);
    };
    if let Err(e) = liq2sol_signer::run(std::path::Path::new(&path)) {
        eprintln!("signer stopped: {e}");
        std::process::exit(1);
    }
}
