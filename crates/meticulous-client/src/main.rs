use anyhow::Result;
use clap::Parser;
use meticulous_base::{
    ClientJobId, JobDetails, JobError, JobOutputResult, JobResult, JobStatus, JobSuccess,
};
use meticulous_client::Client;
use meticulous_util::process::{ExitCode, ExitCodeAccumulator};
use serde::Deserialize;
use serde_json::{self, Deserializer};
use std::{
    fs::File,
    io::{self, Read, Write as _},
    net::{SocketAddr, ToSocketAddrs as _},
    path::{Path, PathBuf},
    sync::Arc,
};

/// The meticulous client. This process sends jobs to the broker to be executed.
#[derive(Parser)]
#[command(version)]
struct CliOptions {
    /// Socket address of broker. Examples: 127.0.0.1:5000 host.example.com:2000"
    #[arg(short = 'b', long)]
    broker: String,

    /// File to read jobs from instead of stdin.
    #[arg(short = 'f', long)]
    file: Option<PathBuf>,
}

fn parse_socket_addr(value: String) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = value.to_socket_addrs()?.collect();
    // It's not clear how we could end up with an empty iterator. We'll assume that's
    // impossible until proven wrong.
    Ok(*addrs.get(0).unwrap())
}

#[derive(Deserialize, Debug)]
struct JobDescription {
    program: String,
    arguments: Option<Vec<String>>,
    environment: Option<Vec<String>>,
    layers: Option<Vec<String>>,
}

fn visitor(cjid: ClientJobId, result: JobResult, accum: Arc<ExitCodeAccumulator>) -> Result<()> {
    match result {
        Ok(JobSuccess {
            status,
            stdout,
            stderr,
        }) => {
            match stdout {
                JobOutputResult::None => {}
                JobOutputResult::Inline(bytes) => {
                    io::stdout().lock().write_all(&bytes)?;
                }
                JobOutputResult::Truncated { first, truncated } => {
                    io::stdout().lock().write_all(&first)?;
                    io::stdout().lock().flush()?;
                    eprintln!("job {cjid}: stdout truncated, {truncated} bytes lost");
                }
            }
            match stderr {
                JobOutputResult::None => {}
                JobOutputResult::Inline(bytes) => {
                    io::stderr().lock().write_all(&bytes)?;
                }
                JobOutputResult::Truncated { first, truncated } => {
                    io::stderr().lock().write_all(&first)?;
                    eprintln!("job {cjid}: stderr truncated, {truncated} bytes lost");
                }
            }
            match status {
                JobStatus::Exited(0) => {}
                JobStatus::Exited(code) => {
                    io::stdout().lock().flush()?;
                    eprintln!("job {cjid}: exited with code {code}");
                    accum.add(ExitCode::from(code));
                }
                JobStatus::Signaled(signum) => {
                    io::stdout().lock().flush()?;
                    eprintln!("job {cjid}: killed by signal {signum}");
                    accum.add(ExitCode::FAILURE);
                }
            };
        }
        Err(JobError::Execution(err)) => {
            eprintln!("job {cjid}: execution error: {err}");
            accum.add(ExitCode::FAILURE);
        }
        Err(JobError::System(err)) => {
            eprintln!("job {cjid}: system error: {err}");
            accum.add(ExitCode::FAILURE);
        }
    }
    Ok(())
}

fn main() -> Result<ExitCode> {
    let cli_options = CliOptions::parse();
    let accum = Arc::new(ExitCodeAccumulator::default());
    let mut client = Client::new(parse_socket_addr(cli_options.broker)?)?;
    let reader: Box<dyn Read> = if let Some(file) = cli_options.file {
        Box::new(File::open(file)?)
    } else {
        Box::new(io::stdin().lock())
    };
    let jobs = Deserializer::from_reader(reader).into_iter::<JobDescription>();
    for job in jobs {
        let job = job?;
        let layers = job
            .layers
            .unwrap_or(vec![])
            .iter()
            .map(|layer| client.add_artifact(Path::new(layer).to_owned()))
            .collect::<Result<Vec<_>>>()?;
        let accum_clone = accum.clone();
        client.add_job(
            JobDetails {
                program: job.program,
                arguments: job.arguments.unwrap_or(vec![]),
                environment: job.environment.unwrap_or(vec![]),
                layers,
            },
            Box::new(move |cjid, result| visitor(cjid, result, accum_clone)),
        );
    }
    client.wait_for_outstanding_jobs()?;
    Ok(accum.get())
}

#[test]
fn test_cli() {
    use clap::CommandFactory;
    CliOptions::command().debug_assert()
}
