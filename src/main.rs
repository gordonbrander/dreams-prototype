fn main() {
    // Real stdio handles, unlocked: `serve` hands stdin/stdout to tokio.
    let code = dreams::cli::run(
        std::env::args_os(),
        &mut std::io::stdin(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::exit(code);
}
