fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,easycolab_gpui=info"),
    )
    .init();
    let mut mock = false;
    let mut real = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--mock" => mock = true,
            "--real" => real = true,
            "--help" | "-h" => {
                println!(
                    "EasyColab GPUI\nUsage: easycolab-gpui [--mock | --real]\n--mock  Use demo data without a Colab account or GPU charges."
                );
                return;
            }
            _ => {
                eprintln!("Unknown argument: {argument}. Use --help.");
                std::process::exit(2);
            }
        }
    }
    if mock && real {
        eprintln!("Pass either --mock or --real, not both.");
        std::process::exit(2);
    }
    easycolab_gpui::app::run(mock);
}
