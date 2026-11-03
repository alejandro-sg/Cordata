//! `cordata-bus mcp` — the run bus's MCP stdio server, started by a worker's CLI (see the library docs).

fn main() {
    let code = match std::env::args().nth(1).as_deref() {
        Some("mcp") => cordata_bus::mcp::run_stdio(),
        _ => {
            eprintln!("usage: cordata-bus mcp   (started by a delegated worker's CLI; needs CORDATA_BUS_SOCKET and CORDATA_BUS_TOKEN)");
            2
        }
    };
    std::process::exit(code);
}
