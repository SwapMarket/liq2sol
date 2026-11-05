//! `liq2sol-maker <config.json>`: run the maker's main loop until stopped.
//! `liq2sol-maker clear-rollback <config.json>`: after checking the chain by hand, clear a rollback alarm.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["clear-rollback", path] => liq2sol_maker::clear_rollback(std::path::Path::new(path)),
        [path] => liq2sol_maker::run(std::path::Path::new(path)),
        _ => {
            eprintln!("usage: liq2sol-maker [clear-rollback] <config.json>");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("maker stopped: {e}");
        std::process::exit(1);
    }
}
