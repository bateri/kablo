use std::process::ExitCode;

fn main() -> ExitCode {
    let code = kablo::cli::run(
        std::env::args_os().skip(1),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    ExitCode::from(code)
}
