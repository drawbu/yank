mod cli;

fn main() -> anyhow::Result<()> {
    let args = cli::parse();
    tracing_subscriber::fmt::init();

    tracing::debug!("args: {:?}", args);

    if args.version {
        let rev = std::option_env!("BUILD_REV").unwrap_or_else(|| {
            tracing::warn!("BUILD_REV needs to be set at compile-time");
            "unknown"
        });
        println!("{} {}", env!("CARGO_PKG_NAME"), rev);
        return Ok(());
    }

    match args.command {
        None => Err(anyhow::anyhow!("No command provided")),
        Some(c) => todo!(),
    }
}
