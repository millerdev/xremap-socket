use anyhow::{Result};
use defer::defer;
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
use zbus::{Connection, MatchRule, Message, MessageStream};
use zbus::fdo::DBusProxy;
use zbus::zvariant::{OwnedObjectPath, Value};

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
    let active_sessions: Arc<RwLock<HashMap<OwnedObjectPath, Session>>> =
        Arc::new(RwLock::new(HashMap::new()));
    tokio::spawn(monitor_sessions(active_sessions.clone(), cfg.clone()));
    tokio::spawn(setup_process_signal_handlers(cfg.socket.clone()));
    socket_server(cfg, active_sessions).await
}

async fn monitor_sessions(
    active_sessions: Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
    cfg: Config,
) -> Result<()> {
    let connection = Connection::system().await?;
    let proxy = DBusProxy::new(&connection).await?;
    let sessions: Arc<RwLock<HashMap<OwnedObjectPath, Session>>> = Arc::new(RwLock::new(HashMap::new()));
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

    debug!("Monitoring user sessions...");
    if let Err(why) = monitor_existing_sessions(
        &connection,
        &proxy,
        &sessions,
        &active_sessions,
        &cfg
    ).await {
        warn!("Cannot monitor existing sessions: {}", why)
    };
    while let Some(msg) = MessageStream::from(&connection).next().await {
        match msg {
            Ok(message) => {
                if let Err(handle_err) = handle_message(
                    &message,
                    &connection,
                    &proxy,
                    &sessions,
                    &active_sessions,
                    &cfg,
                ).await {
                    warn!("Could not handle {:?}: {}", message, handle_err)
                }
            },
            Err(why) => warn!("Message fail: {}", why)
        };
    }
    Ok(())
}

async fn handle_message(
    message: &Message,
    connection: &Connection,
    proxy: &DBusProxy<'static>,
    sessions: &Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
    active_sessions: &Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
    cfg: &Config,
) -> Result<()> {
    let header = message.header();
    let member = match header.member() {
        Some(m) => m,
        None => return Ok(()),  // ignore null member
    };
    match member.as_str() {
        "SessionNew" => {
            let (session_id, session_path): (String, OwnedObjectPath) =
                message.body().deserialize()?;
            handle_new_session(
                &connection,
                &proxy,
                &session_id,
                &session_path,
                &sessions,
                &active_sessions,
                &cfg,
            ).await?;
        }
        "PropertiesChanged" => {
            let header = message.header();
            let path_ref = header.path()
                .ok_or_else(|| anyhow::anyhow!("No path in message"))?;
            let session_path: OwnedObjectPath = path_ref.clone().into();
            let body = message.body();
            let (_name, changed, _invalidated): (String, HashMap<String, Value<'_>>, Vec<String>) =
                body.deserialize()?;
            handle_properties_changed(
                session_path,
                changed,
                &sessions,
                &active_sessions,
            ).await;
        }
        "SessionRemoved" => {
            let (session_id, session_path): (String, OwnedObjectPath) =
                message.body().deserialize()?;
            let session = sessions.write().await.remove(&session_path);
            let active = active_sessions.write().await.remove(&session_path);
            if session.is_some() || active.is_some() {
                remove_properties_changed_match_rule(&proxy, &session_path).await?;
                if session.is_some() {
                    info!("Session {} removed", session_id);
                } else if active.is_some() {
                    warn!("Discarded unknown active session: {}", session_id);
                }
            }
        }
        sig => warn!("Ignored message: {}", sig)
    };
    Ok(())
}

async fn handle_new_session(
    connection: &Connection,
    proxy: &DBusProxy<'static>,
    session_id: &String,
    session_path: &OwnedObjectPath,
    sessions: &Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
    active_sessions: &Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
    cfg: &Config,
) -> Result<()> {
    let session_proxy = SessionProxy::builder(connection).path(session_path)?.build().await?;
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
    };
    let active_str = if is_active { " active" } else { "" };
    info!("Monitoring{} session {} (uid={}, seat={})", active_str, session_id, uid, seat_id);
    add_properties_changed_match_rule(&proxy, &session_path).await?;
    sessions.write().await.insert(session_path.clone(), session.clone());
    if is_active {
        active_sessions.write().await.insert(session_path.clone(), session);
    }
    Ok(())
}

async fn handle_properties_changed(
    session_path: OwnedObjectPath,
    changed: HashMap<String, Value<'_>>,
    sessions: &Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
    active_sessions: &Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
) {
    if let Some(Value::Bool(is_active)) = changed.get("Active") {
        if let Some(session) = sessions.write().await.get(&session_path) {
            let mut active_guard = active_sessions.write().await;
            if *is_active {
                debug!("Activated session {}", session.id);
                active_guard.insert(session_path.clone(), session.clone());
            } else {
                debug!("Deactivated session {}", session.id);
                active_guard.remove(&session_path);
            }
        }
    }
}

async fn monitor_existing_sessions(
    connection: &Connection,
    proxy: &DBusProxy<'static>,
    sessions: &Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
    active_sessions: &Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
    cfg: &Config,
) -> Result<()> {
    let manager = ManagerProxy::new(connection).await?;
    let session_list = manager.list_sessions().await?;
    for (session_id, uid, _user, seat_id, session_path) in session_list {
        if seat_id.is_empty() {
            continue;
        }
        info!("Existing session: {} (uid={}, seat={})", session_id, uid, seat_id);
        let session_proxy = SessionProxy::builder(connection).path(&session_path)?.build().await?;
        let is_active = session_proxy.active().await?;
        let session = Session {
            id: session_id.clone(),
            user_socket: cfg.user_socket_path(uid),
        };
        if is_active {
            active_sessions.write().await.insert(session_path.clone(), session.clone());
        }
        sessions.write().await.insert(session_path.clone(), session);
        add_properties_changed_match_rule(&proxy, &session_path).await?
    }
    Ok(())
}

async fn add_properties_changed_match_rule(
    proxy: &DBusProxy<'static>,
    session_path: &OwnedObjectPath,
) -> Result<()> {
    let rule = session_changed_rule(session_path)?;
    Ok(proxy.add_match_rule(rule).await?)
}

async fn remove_properties_changed_match_rule(
    proxy: &DBusProxy<'static>,
    session_path: &OwnedObjectPath,
) -> Result<()> {
    let rule = session_changed_rule(session_path)?;
    Ok(proxy.remove_match_rule(rule).await?)
}

fn session_changed_rule(session_path: &OwnedObjectPath) -> Result<MatchRule<'_>> {
    Ok(MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .path(session_path)?
        .member("PropertiesChanged")?
        .build())
}

async fn socket_server(
    cfg: Config,
    active_sessions: Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
) -> Result<()> {
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
        if let Some(session) = get_active_session(&active_sessions).await {
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

async fn get_active_session(
    active_sessions: &Arc<RwLock<HashMap<OwnedObjectPath, Session>>>,
) -> Option<Session> {
    let active = active_sessions.read().await;
    if active.len() > 1 {
        warn!("Unexpected: multiple active sessions: {:?}", active.keys());
        return None;
    }
    active.values().next().cloned()
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
