mod monitor;

use anyhow::{Result};
use defer::defer;
use clap::Parser;
use log::{debug, error, trace};
use monitor::SessionMonitor;
use std::fs;
use std::io::{Error, ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::{timeout, Duration};
use users::get_user_by_name;

/// Relay from /run/xremap/gnome.sock to /run/user/{uid}/gnome.sock
/// where {uid} is the id of the user with an active seated session.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Xremap socket path
    #[arg(short, long, default_value = "/run/xremap/gnome.sock")]
    socket: PathBuf,

    /// Xremap socket owner
    #[arg(short, long, default_value = "xremap")]
    owner: String,

    /// User socket pattern. '{uid}' will be replaced with the active user id.
    #[arg(short, long, default_value = "/run/xremap/{uid}/gnome.sock")]
    user_socket: String,

    /// Enable debug logging. Repeat for more detail.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let log_level = match args.verbose {0 => "info", 1 => "debug", _ => "trace"};
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();

    let cfg = Arc::new(Config::new(args.socket, args.owner, args.user_socket));
    let monitor = Arc::new(SessionMonitor::new(cfg.clone()).await?);
    let monitor_ = monitor.clone();
    tokio::spawn(setup_process_signal_handlers(cfg.socket.clone()));
    tokio::spawn(async move { monitor_.run().await });
    socket_server(cfg, monitor).await
}

async fn socket_server(cfg: Arc<Config>, monitor: Arc<SessionMonitor>) -> Result<()> {
    if !cfg.socket.parent().map(|p| p.is_dir()).unwrap_or(false) {
        anyhow::bail!("Socket directory not found: {:?}", cfg.socket);
    }
    if cfg.socket.exists() {
        fs::remove_file(&cfg.socket)?;
    }
    let defered_socket = cfg.socket.clone();
    let listener = UnixListener::bind(&cfg.socket)?;
    let _defer = defer(|| cleanup(defered_socket));
    let owner = get_user_by_name(&cfg.owner)
        .ok_or_else(|| anyhow::anyhow!("User not found: {}", cfg.owner))?;
    std::os::unix::fs::chown(&cfg.socket, Some(owner.uid()), Some(owner.primary_group_id()))?;
    fs::set_permissions(&cfg.socket, fs::Permissions::from_mode(0o600))?;
    debug!("Serving {:?}", cfg.socket);
    loop {
        let (stream, _addr) = listener.accept().await?;
        if let Some(session) = monitor.get_active_session().await {
            trace!("Relaying to session {} on {:?}", session.id, session.user_socket);
            match relay_message(stream, &session.user_socket).await {
                Ok(()) => trace!("Relay to session {} completed", session.id),
                Err(why) => trace!("Relay to session {} error: {}", session.id, why),
            }
        } else {
            trace!("No active session");
        }
    }
}

async fn relay_message(mut in_stream: UnixStream, user_socket: &Path) -> Result<()> {
    if !user_socket.exists() {
        return Err(anyhow::format_err!("{:?} not found", user_socket));
    }
    let mut out_stream = UnixStream::connect(user_socket).await?;
    let (mut in_read, mut in_write) = in_stream.split();
    let (mut out_read, mut out_write) = out_stream.split();

    let relay_in = async {
        let mut buffer = vec![0u8; 4096];
        loop {
            let n = in_read.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            trace!("Message: {} bytes", n);
            out_write.write_all(&buffer[..n]).await?;
            out_write.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    };

    let relay_out = async {
        let mut buffer = vec![0u8; 4096];
        loop {
            let n = match timeout(Duration::from_secs(1), out_read.read(&mut buffer)).await {
                Ok(result) => result?,
                Err(_) => return Err(Error::new(ErrorKind::TimedOut, "Read timeout")),
            };
            if n == 0 {
                break;
            }
            trace!("Response: {} bytes", n);
            in_write.write_all(&buffer[..n]).await?;
            in_write.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    };

    tokio::select! {
        res = relay_in => res?,
        res = relay_out => res?,
    }
    Ok(())
}

async fn setup_process_signal_handlers(socket: PathBuf) {
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to setup SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("Failed to setup SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => {
            debug!("Received SIGTERM");
        }
        _ = sigint.recv() => {
            debug!("Received SIGINT");
        }
    }
    cleanup(socket);
    std::process::exit(0);
}

fn cleanup(socket: PathBuf) {
    if socket.exists() {
        match fs::remove_file(&socket) {
            Ok(()) => debug!("Removed {:?}", socket),
            Err(why) => error!("Failed to remove {:?}: {}", socket, why),
        }
    }
}

#[derive(Clone, Debug)]
struct Config {
    socket: PathBuf,
    owner: String,
    user_socket: String,
}

impl Config {
    fn new(socket: PathBuf, owner: String, user_socket: String) -> Config {
        Config {
            socket,
            owner,
            user_socket,
        }
    }

    fn user_socket_path(&self, uid: u32) -> PathBuf {
        PathBuf::from(self.user_socket.replace("{uid}", &uid.to_string()))
    }
}
