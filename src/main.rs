use anyhow::{Result};
use clap::Parser;
use futures_util::stream::StreamExt;
use log::{debug, error, info, trace};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use users::get_user_by_name;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, MatchRule, MessageStream};

/// Relay from /run/xremap/gnome.sock to /run/user/{uid}/gnome.sock
/// where {uid} is the id of the user with an active seated session.
/// Behavior on systems with multiple concurrent seats is undefined.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Xremap socket path
    #[arg(short, long, default_value = "/run/xremap/gnome.sock")]
    socket: PathBuf,

    /// Xremap socket owner
    #[arg(short, long, default_value = "xremap")]
    owner: String,

    /// User socket pattern.
    #[arg(short, long, default_value = "/run/user/{uid}/gnome.sock")]
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

    let cfg = Config::new(args.socket, args.owner, args.user_socket);
    setup_signal_handlers(cfg.clone());
    let result = monitor_sessions(cfg.clone()).await;
    cleanup_socket(&cfg);
    result
}

async fn monitor_sessions(cfg: Config) -> Result<()> {
    debug!("Monitoring D-Bus for new user sessions...");

    if !cfg.socket.parent().map(|p| p.is_dir()).unwrap_or(false) {
        anyhow::bail!("Socket directory not found: {:?}", cfg.socket);
    }

    let connection = Connection::system().await?;
    let proxy = zbus::fdo::DBusProxy::new(&connection).await?;
    let active_sessions: Arc<Mutex<HashMap<String, JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    monitor_existing_sessions(&connection, &active_sessions, &cfg).await?;

    let session_new_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.login1.Manager")?
        .member("SessionNew")?
        .build();
    let session_removed_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.login1.Manager")?
        .member("SessionRemoved")?
        .build();

    proxy.add_match_rule(session_new_rule).await?;
    proxy.add_match_rule(session_removed_rule).await?;

    while let Some(msg) = MessageStream::from(&connection).next().await {
        let msg = msg?;
        if let Some(member) = msg.header().member() {
            match member.as_str() {
                "SessionNew" => {
                    let (session_id, session_path): (String, OwnedObjectPath) =
                        msg.body().deserialize()?;

                    if let Some(uid) = get_seated_uid(
                        &connection.clone(),
                        session_id.clone(),
                        session_path,
                    ).await? {
                        let handle = spawn_session_handler(
                            session_id.clone(),
                            uid,
                            cfg.clone(),
                        );
                        active_sessions.lock().await.insert(session_id, handle);
                    }
                }
                "SessionRemoved" => {
                    let (session_id, _session_path): (String, OwnedObjectPath) =
                        msg.body().deserialize()?;

                    if let Some(handle) = active_sessions.lock().await.remove(&session_id) {
                        trace!("Session {} removed -> cancel monitor", session_id);
                        handle.abort();
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

async fn get_seated_uid(
    connection: &Connection,
    session_id: String,
    session_path: OwnedObjectPath,
) -> Result<Option<u32>> {
    let session_proxy = SessionProxy::builder(connection)
        .path(session_path)?
        .build()
        .await?;
    let (seat_id, _seat_path) = session_proxy.seat().await?;
    if !seat_id.is_empty() {
        let (uid, _user_path) = session_proxy.user().await?;
        Ok(Some(uid))
    } else {
        debug!("Ignoring unseated session {}", session_id);
        Ok(None)
    }
}

async fn monitor_existing_sessions(
    connection: &Connection,
    active_sessions: &Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    cfg: &Config,
) -> Result<(), anyhow::Error> {
    let manager = ManagerProxy::new(connection).await?;
    let sessions = manager.list_sessions().await?;
    Ok(for (session_id, uid, _user, seat_id, _session_path) in sessions {
        if !seat_id.is_empty() {
            debug!("Existing session: {} (uid={})", session_id, uid);
            let handle = spawn_session_handler(
                session_id.clone(),
                uid,
                cfg.clone(),
            );
            active_sessions.lock().await.insert(session_id, handle);
        }
    })
}

fn spawn_session_handler(
    session_id: String,
    uid: u32,
    cfg: Config,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        match handle_session(session_id.clone(), uid, cfg).await {
            Ok(()) => debug!("Session {} monitor completed", session_id),
            Err(why) => error!("Session {} monitor failed: {}", session_id, why),
        }
    })
}

async fn handle_session(
    session_id: String,
    uid: u32,
    cfg: Config,
) -> Result<()> {
    info!("Monitoring session {} for user {}", session_id, uid);

    if cfg.socket.exists() {
        fs::remove_file(&cfg.socket)?;
    }
    let listener = UnixListener::bind(&cfg.socket)?;
    let owner = get_user_by_name(&cfg.owner).ok_or_else(|| anyhow::anyhow!("User not found: {}", cfg.owner))?;
    std::os::unix::fs::chown(&cfg.socket, Some(owner.uid()), Some(owner.primary_group_id()))?;
    fs::set_permissions(&cfg.socket, fs::Permissions::from_mode(0o660))?;
    let user_socket = cfg.user_socket_path(uid);
    debug!("Session {} serving {:?} -> {:?}", session_id, cfg.socket, user_socket);
    loop {
        let (stream, _addr) = listener.accept().await?;
        trace!("Session {} relaying to {:?}", session_id, user_socket);
        relay_message(stream, &user_socket).await?;
        trace!("Session {} relay completed", session_id);
    }
}

async fn relay_message(mut in_stream: UnixStream, user_socket: &Path) -> Result<()> {
    if !user_socket.exists() {
        trace!("Abort relay: {:?} does not exist", user_socket);
        return Ok(());
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
            let n = out_read.read(&mut buffer).await?;
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

fn setup_signal_handlers(cfg: Config) {
    tokio::spawn(async move {
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
        cleanup_socket(&cfg);
        std::process::exit(0);
    });
}

fn cleanup_socket(cfg: &Config) {
    if cfg.socket.exists() {
        if let Err(why) = fs::remove_file(&cfg.socket) {
            error!("Failed to remove {:?}: {}", cfg.socket, why);
        } else {
            debug!("Removed {:?}", cfg.socket);
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

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    fn list_sessions(&self) -> zbus::Result<Vec<(String, u32, String, String, OwnedObjectPath)>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
trait Session {
    #[zbus(property)]
    fn seat(&self) -> zbus::Result<(String, OwnedObjectPath)>;

    #[zbus(property)]
    fn user(&self) -> zbus::Result<(u32, OwnedObjectPath)>;
}
