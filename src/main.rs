fn main() {
    let code = match txpt::cli::run(std::env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("txpt: {err}");
            64
        }
    };
    std::process::exit(code);
}
