use anyhow::{Context as _, Result};
use cargo_metest::{
    config::{Config, ConfigOptions},
    main_app_new, DefaultProgressDriver, MainAppDeps,
};
use clap::{Args, Parser, Subcommand};
use console::Term;
use figment::{
    error::Kind,
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use meticulous_client::DefaultClientDriver;
use meticulous_util::process::ExitCode;
use serde::Deserialize;
use std::{io::IsTerminal as _, path::PathBuf, process::Command, str};

/// The meticulous client. This process sends work to the broker to be executed by workers.
#[derive(Parser)]
#[command(version, bin_name = "cargo")]
struct Cli {
    #[clap(subcommand)]
    subcommand: CliSubcommand,
}

impl Cli {
    fn subcommand(self) -> CliOptions {
        match self.subcommand {
            CliSubcommand::Metest(cmd) => cmd,
        }
    }
}

#[derive(Debug, Subcommand)]
enum CliSubcommand {
    Metest(CliOptions),
}

#[derive(Debug, Args)]
#[command(version)]
struct CliOptions {
    /// Configuration file. Values set in the configuration file will be overridden by values set
    /// through environment variables and values set on the command line. If not set, the file
    /// .config/cargo-metest.toml in the workspace root will be used, if it exists.
    #[arg(short = 'c', long)]
    config_file: Option<PathBuf>,

    /// Print configuration and exit
    #[arg(short = 'P', long)]
    print_config: bool,

    /// Socket address of broker. Examples: 127.0.0.1:5000 host.example.com:2000".
    #[arg(short = 'b', long)]
    broker: Option<String>,

    /// Don't output information about the tests being run
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Only run tests from the given package
    #[arg(short = 'p', long)]
    package: Option<String>,

    /// Only run tests whose names contain the given string
    filter: Option<String>,
}

impl CliOptions {
    fn to_config_options(&self) -> ConfigOptions {
        let broker = self.broker.clone();
        let quiet = if self.quiet { Some(true) } else { None };
        ConfigOptions { broker, quiet }
    }
}

#[derive(Deserialize)]
struct CargoMetadata {
    workspace_root: PathBuf,
}

/// The main function for the client. This should be called on a task of its own. It will return
/// when a signal is received or when all work has been processed by the broker.
pub fn main() -> Result<ExitCode> {
    let cli_options = Cli::parse().subcommand();

    let cargo_metadata = Command::new("cargo")
        .args(["metadata", "--format-version=1"])
        .output()
        .context("getting cargo metadata")?;
    let cargo_metadata: CargoMetadata =
        serde_json::from_slice(&cargo_metadata.stdout).context("parsing cargo metadata")?;

    let config_file = match &cli_options.config_file {
        Some(path) => {
            if !path.exists() {
                eprintln!("warning: config file {} not found", path.display());
            }
            path.clone()
        }
        None => cargo_metadata
            .workspace_root
            .join(".config")
            .join("cargo-metest.toml"),
    };

    let config: Config = Figment::new()
        .merge(Serialized::defaults(ConfigOptions::default()))
        .merge(Toml::file(config_file))
        .merge(Env::prefixed("CARGO_METEST_"))
        .merge(Serialized::globals(cli_options.to_config_options()))
        .extract()
        .map_err(|mut e| {
            if let Kind::MissingField(field) = &e.kind {
                e.kind = Kind::Message(format!("configuration value \"{field}\" was no provided"));
                e
            } else {
                e
            }
        })
        .context("reading configuration")?;

    if cli_options.print_config {
        println!("{config:#?}");
        return Ok(ExitCode::SUCCESS);
    }

    let deps = MainAppDeps::new(
        "cargo".into(),
        cli_options.package,
        cli_options.filter,
        std::io::stderr(),
        std::io::stderr().is_terminal(),
        &cargo_metadata.workspace_root,
        config.broker,
        DefaultClientDriver::default(),
    )?;

    let stdout_tty = std::io::stdout().is_terminal();
    std::thread::scope(|scope| {
        let mut app = main_app_new(
            &deps,
            stdout_tty,
            config.quiet,
            Term::buffered_stdout(),
            DefaultProgressDriver::new(scope),
        )?;
        while app.enqueue_one()? {}
        app.finish()
    })
}

#[test]
fn test_cli() {
    use clap::CommandFactory;
    Cli::command().debug_assert()
}
