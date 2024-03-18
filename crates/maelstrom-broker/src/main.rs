use anyhow::{Context, Result};
use maelstrom_broker::config::Config;
use slog::{info, o, Drain, LevelFilter, Logger};
use slog_async::Async;
use slog_term::{FullFormat, TermDecorator};
use std::{
    net::{Ipv6Addr, SocketAddrV6},
    process,
};
use tokio::{net::TcpListener, runtime::Runtime};
use xdg::BaseDirectories;

fn main() -> Result<()> {
    let base_directories =
        BaseDirectories::with_prefix("maelstrom/broker").context("searching for config files")?;
    let env_var_prefix = "MAELSTROM_BROKER";
    let args = Config::add_command_line_options(&base_directories, env_var_prefix).get_matches();
    let config = maelstrom_config::new_config::<Config>(&base_directories, env_var_prefix, args)?;
    let decorator = TermDecorator::new().build();
    let drain = FullFormat::new(decorator).build().fuse();
    let drain = Async::new(drain).build().fuse();
    let drain = LevelFilter::new(drain, config.log_level.as_slog_level()).fuse();
    let log = Logger::root(drain, o!());
    Runtime::new()
        .context("starting tokio runtime")?
        .block_on(async {
            let sock_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, *config.port.inner(), 0, 0);
            let listener = TcpListener::bind(sock_addr)
                .await
                .context("binding listener socket")?;

            let sock_addr =
                SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, *config.http_port.inner(), 0, 0);
            let http_listener = TcpListener::bind(sock_addr)
                .await
                .context("binding http listener socket")?;

            let listener_addr = listener
                .local_addr()
                .context("retrieving listener local address")?;
            let http_listener_addr = http_listener
                .local_addr()
                .context("retrieving listener local address")?;
            info!(log, "started";
                "config" => ?config,
                "addr" => listener_addr,
                "http_addr" => http_listener_addr,
                "pid" => process::id());

            maelstrom_broker::main(
                listener,
                http_listener,
                config.cache_root,
                config.cache_bytes_used_target,
                log.clone(),
            )
            .await;
            info!(log, "exiting");
            Ok(())
        })
}
