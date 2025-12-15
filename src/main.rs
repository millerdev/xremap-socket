use anyhow::{Result};
use clap::Parser;
use futures_util::stream::StreamExt;
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::RwLock;
use users::get_user_by_name;
use zbus::fdo::DBusProxy;
use zbus::zvariant::{OwnedObjectPath, Value};
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
    setup_process_signal_handlers(cfg.clone());
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
    let proxy = DBusProxy::new(&connection).await?;

    let sessions: Arc<RwLock<HashMap<String, Session>>> = Arc::new(RwLock::new(HashMap::new()));
    let active_sessions: Arc<RwLock<HashMap<String, Session>>> = Arc::new(RwLock::new(HashMap::new()));

    monitor_existing_sessions(&connection, proxy.clone(), &sessions, &active_sessions, &cfg).await?;

    // Start single socket server that relays to active session
    let active_sessions_clone = active_sessions.clone();
    let cfg_clone = cfg.clone();
    tokio::spawn(async move {
        if let Err(e) = socket_server(cfg_clone, active_sessions_clone).await {
            error!("Socket server error: {}", e);
        }
    });

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

                    handle_new_session(
                        &connection,
                        proxy.clone(),
                        session_id,
                        session_path,
                        &sessions,
                        &active_sessions,
                        &cfg,
                    ).await?;
                }
                "SessionRemoved" => {
                    let (session_id, _session_path): (String, OwnedObjectPath) =
                        msg.body().deserialize()?;

                    let session = sessions.write().await.remove(&session_id);
                    let active = active_sessions.write().await.remove(&session_id);
                    let active_str = if active.is_some() { " active" } else { "" };

                    if session.is_some() {
                        trace!("Session{} {} removed", active_str, session_id);
                    } else if active.is_some() {
                        warn!("Discarded unknown active session: {}", session_id);
                    }
                }
                "PropertiesChanged" => {
                    let header = msg.header();
                    let path_ref = header.path()
                        .ok_or_else(|| anyhow::anyhow!("No path in message"))?;
                    let path_owned: OwnedObjectPath = path_ref.clone().into();
                    let body = msg.body();
                    let (_name, props, _invalidated): (String, HashMap<String, Value<'_>>, Vec<String>) =
                        body.deserialize()?;

                    if let Some(active_value) = props.get("Active") {
                        if let Ok(owned_value) = active_value.try_clone() {
                            if let Ok(is_active) = owned_value.try_into() {
                                handle_properties_changed(
                                    path_owned,
                                    is_active,
                                    &sessions,
                                    &active_sessions,
                                ).await;
                            }
                        }
                    }
                }
                sig => trace!("Ignored signal: {}", sig)
            }
        }
    }

    Ok(())
}

async fn handle_new_session(
    connection: &Connection,
    proxy: DBusProxy<'static>,
    session_id: String,
    session_path: OwnedObjectPath,
    sessions: &Arc<RwLock<HashMap<String, Session>>>,
    active_sessions: &Arc<RwLock<HashMap<String, Session>>>,
    cfg: &Config,
) -> Result<()> {
    let session_proxy = SessionProxy::builder(connection).path(&session_path)?.build().await?;
    let (seat_id, _seat_path) = session_proxy.seat().await?;

    if seat_id.is_empty() {
        debug!("Ignoring unseated session {}", session_id);
        return Ok(());
    }

    let (uid, _user_path) = session_proxy.user().await?;
    let is_active = session_proxy.active().await?;

    let session = Session {
        id: session_id.clone(),
        user_socket: cfg.user_socket_path(uid),
        is_active,
    };

    let active_str = if is_active { " active" } else { "" };
    info!("Monitoring{} session {} for user {}", active_str, session_id, uid);

    sessions.write().await.insert(session_id.clone(), session.clone());
    if is_active {
        active_sessions.write().await.insert(session_id.clone(), session);
    }

    // Setup PropertiesChanged monitoring
    let session_changed_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .path(session_path)?
        .member("PropertiesChanged")?
        .build();
    proxy.add_match_rule(session_changed_rule).await?;

    Ok(())
}

async fn handle_properties_changed(
    path: OwnedObjectPath,
    is_active: bool,
    sessions: &Arc<RwLock<HashMap<String, Session>>>,
    active_sessions: &Arc<RwLock<HashMap<String, Session>>>,
) {
    // Find session by path
    let session_id = {
        let sessions_guard = sessions.read().await;
        sessions_guard.iter()
            .find(|(_, s)| s.id == path.as_str().rsplit('/').next().unwrap_or(""))
            .map(|(id, _)| id.clone())
    };

    if let Some(session_id) = session_id {
        let mut sessions_guard = sessions.write().await;
        if let Some(session) = sessions_guard.get_mut(&session_id) {
            session.is_active = is_active;

            let mut active_guard = active_sessions.write().await;
            if is_active {
                info!("Activated session {}", session_id);
                active_guard.insert(session_id.clone(), session.clone());
            } else {
                info!("Deactivated session {}", session_id);
                active_guard.remove(&session_id);
            }
        }
    }
}

async fn get_active_session(
    active_sessions: &Arc<RwLock<HashMap<String, Session>>>,
) -> Option<Session> {
    let active = active_sessions.read().await;
    if active.len() > 1 {
        warn!("Unexpected: multiple active sessions: {:?}", active.keys());
        return None;
    }
    active.values().next().cloned()
}

async fn monitor_existing_sessions(
    connection: &Connection,
    proxy: DBusProxy<'static>,
    sessions: &Arc<RwLock<HashMap<String, Session>>>,
    active_sessions: &Arc<RwLock<HashMap<String, Session>>>,
    cfg: &Config,
) -> Result<()> {
    let manager = ManagerProxy::new(connection).await?;
    let session_list = manager.list_sessions().await?;

    for (session_id, uid, _user, seat_id, session_path) in session_list {
        if !seat_id.is_empty() {
            debug!("Existing session: {} (uid={}, seat={})", session_id, uid, seat_id);
            let session_proxy = SessionProxy::builder(connection).path(&session_path)?.build().await?;
            let is_active = session_proxy.active().await?;

            let session = Session {
                id: session_id.clone(),
                user_socket: cfg.user_socket_path(uid),
                is_active,
            };

            sessions.write().await.insert(session_id.clone(), session.clone());
            if is_active {
                active_sessions.write().await.insert(session_id.clone(), session);
            }

            // Setup PropertiesChanged monitoring
            let session_changed_rule = MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.freedesktop.DBus.Properties")?
                .path(session_path)?
                .member("PropertiesChanged")?
                .build();
            proxy.add_match_rule(session_changed_rule).await?;
        }
    }

    Ok(())
}

async fn socket_server(
    cfg: Config,
    active_sessions: Arc<RwLock<HashMap<String, Session>>>,
) -> Result<()> {
    if cfg.socket.exists() {
        fs::remove_file(&cfg.socket)?;
    }

    let listener = UnixListener::bind(&cfg.socket)?;
    let owner = get_user_by_name(&cfg.owner)
        .ok_or_else(|| anyhow::anyhow!("User not found: {}", cfg.owner))?;
    std::os::unix::fs::chown(&cfg.socket, Some(owner.uid()), Some(owner.primary_group_id()))?;
    fs::set_permissions(&cfg.socket, fs::Permissions::from_mode(0o600))?;
    debug!("Serving to {:?}", cfg.socket);
    loop {
        let (stream, _addr) = listener.accept().await?;
        let session = get_active_session(&active_sessions).await;
        if let Some(session) = session {
            trace!("Relaying to session {} on {:?}", session.id, session.user_socket);
            if let Err(e) = relay_message(stream, &session.user_socket).await {
                trace!("Relay error: {}", e);
            }
            trace!("Relay to {:?} completed", session.user_socket);
        } else {
            trace!("No active session");
        }
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

#[derive(Clone, Debug)]
struct Session {
    id: String,
    user_socket: PathBuf,
    is_active: bool,
}

fn setup_process_signal_handlers(cfg: Config) {
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

    #[zbus(property)]
    fn active(&self) -> zbus::Result<bool>;
}
