use std::io::Read;

fn main() {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    if let Some(reason) = cordata_guard::guard::check(&raw) {
        eprint!("{reason}");
        std::process::exit(2);
    }
}
