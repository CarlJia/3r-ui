use std::process::ExitCode;

use three_r_ui::config::AppConfig;
use three_r_ui::run;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "agent" {
        let mut server = String::new();
        let mut token = None;
        let mut data_dir = std::path::PathBuf::from("/etc/3r-agent");

        let mut i = 2;
        while i < args.len() {
            if args[i] == "--server" && i + 1 < args.len() {
                server = args[i + 1].clone();
                i += 2;
            } else if args[i] == "--token" && i + 1 < args.len() {
                token = Some(args[i + 1].clone());
                i += 2;
            } else if args[i] == "--data-dir" && i + 1 < args.len() {
                data_dir = std::path::PathBuf::from(&args[i + 1]);
                i += 2;
            } else {
                i += 1;
            }
        }

        if server.is_empty() {
            eprintln!("Error: --server <URL> is required for agent mode");
            return ExitCode::FAILURE;
        }

        let daemon = std::sync::Arc::new(three_r_ui::node::agent::AgentDaemon::new(
            server, token, data_dir,
        ));
        daemon.run().await;
        return ExitCode::SUCCESS;
    }

    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "fatal");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}
