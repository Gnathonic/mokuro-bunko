//! `mokuro-bunko` — the command-line entry point (CLI wiring lands with the server).

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    let flavor = if cfg!(feature = "ocr") { "full" } else { "lite" };
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("mokuro-bunko {} ({flavor}, {})", bunko_core::VERSION, bunko_update::TARGET);
        return;
    }
    eprintln!("mokuro-bunko {} ({flavor}): CLI not wired yet", bunko_core::VERSION);
    std::process::exit(2);
}
