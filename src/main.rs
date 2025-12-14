use anyhow::{Context, Result};
use clap::Parser;
use futures_util::stream::StreamExt;
use log::{debug, info, trace};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use users::get_user_by_name;
use zbus::zvariant::OwnedObjectPath;
use zbus::names::BusName;
use zbus::{Connection, MatchRule, MessageStream};


/// Relay messages from XREMAP_SOCKET to /run/user/@UID/{basename(XREMAP_SOCKET)}.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Xremap socket path
    #[arg(short, long, default_value = "/run/xremap/gnome.sock")]
    socket: PathBuf,

    /// Xremap socket owner
    #[arg(short, long, default_value = "xremap")]
    owner: String,

    /// Enable debug logging. Repeat for more detail.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let log_level = match args.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();

    let xremap_socket = args.socket.clone();
    let result = monitor_sessions(xremap_socket.clone(), args.owner).await;

    if xremap_socket.exists() {
        fs::remove_file(&xremap_socket)?;
        debug!("Removed {:?}", xremap_socket);
    }

    result
}

async fn monitor_sessions(xremap_socket: PathBuf, owner: String) -> Result<()> {
    debug!("Monitoring D-Bus for new user sessions...");

    if !xremap_socket.parent().map(|p| p.is_dir()).unwrap_or(false) {
        anyhow::bail!("Socket directory not found: {:?}", xremap_socket);
    }

    let connection = Connection::system().await?;
    let proxy = zbus::fdo::DBusProxy::new(&connection).await?;

    // Check if login1 service is available
    proxy
        .get_name_owner(BusName::from_static_str("org.freedesktop.login1")?)
        .await
        .context("login1 service not available")?;

    let active_sessions: Arc<Mutex<HashMap<String, JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // List existing sessions
    let manager = ManagerProxy::new(&connection).await?;
    let sessions = manager.list_sessions().await?;

    for (session_id, uid, _user, seat_id, _session_path) in sessions {
        if !seat_id.is_empty() {
            debug!("Existing session: {} (uid={}, seat={})", session_id, uid, seat_id);
            let handle = spawn_session_handler(
                session_id.clone(),
                uid,
                xremap_socket.clone(),
                owner.clone(),
            );
            active_sessions.lock().await.insert(session_id, handle);
        }
    }

    // Monitor for new sessions
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

    let mut stream = MessageStream::from(&connection);

    while let Some(msg) = stream.next().await {
        let msg = msg?;
        let header = msg.header();

        if let Some(member) = header.member() {
            match member.as_str() {
                "SessionNew" => {
                    let (session_id, session_path): (String, OwnedObjectPath) =
                        msg.body().deserialize()?;

                    let conn = connection.clone();
                    let xremap_socket_clone = xremap_socket.clone();
                    let owner_clone = owner.clone();
                    let active_sessions = active_sessions.clone();

                    tokio::spawn(async move {
                        match handle_new_session(
                            &conn,
                            session_id.clone(),
                            session_path,
                            xremap_socket_clone.clone(),
                            owner_clone.clone(),
                        )
                        .await
                        {
                            Ok(Some((session_id, uid))) => {
                                let handle = spawn_session_handler(
                                    session_id.clone(),
                                    uid,
                                    xremap_socket_clone,
                                    owner_clone,
                                );
                                active_sessions.lock().await.insert(session_id, handle);
                            }
                            Ok(None) => {}
                            Err(e) => {
                                log::error!("Error handling new session {}: {}", session_id, e);
                            }
                        }
                    });
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

fn spawn_session_handler(
    session_id: String,
    uid: u32,
    xremap_socket: PathBuf,
    owner: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        match handle_session(session_id.clone(), uid, xremap_socket, owner).await {
            Ok(()) => debug!("Session {} monitor completed", session_id),
            Err(e) => log::error!("Session {} monitor failed: {}", session_id, e),
        }
    })
}

async fn handle_new_session(
    connection: &Connection,
    session_id: String,
    session_path: OwnedObjectPath,
    _xremap_socket: PathBuf,
    _owner: String,
) -> Result<Option<(String, u32)>> {
    let session_proxy = SessionProxy::builder(connection)
        .path(session_path)?
        .build()
        .await?;

    let (seat_id, _seat_path) = session_proxy.seat().await?;

    if !seat_id.is_empty() {
        let (uid, _user_path) = session_proxy.user().await?;
        Ok(Some((session_id, uid)))
    } else {
        debug!("Ignoring unseated session {}", session_id);
        Ok(None)
    }
}

async fn handle_session(
    session_id: String,
    uid: u32,
    xremap_socket: PathBuf,
    owner: String,
) -> Result<()> {
    info!("Monitoring session {} for user {}", session_id, uid);

    let user_socket = PathBuf::from(format!(
        "/run/user/{}/{}",
        uid,
        xremap_socket.file_name().unwrap().to_string_lossy()
    ));

    // Remove socket if it exists
    if xremap_socket.exists() {
        fs::remove_file(&xremap_socket)?;
    }

    let listener = UnixListener::bind(&xremap_socket)?;

    // Set ownership and permissions
    let user = get_user_by_name(&owner)
        .ok_or_else(|| anyhow::anyhow!("User not found: {}", owner))?;

    std::os::unix::fs::chown(&xremap_socket, Some(user.uid()), Some(user.primary_group_id()))?;
    fs::set_permissions(&xremap_socket, fs::Permissions::from_mode(0o660))?;

    debug!(
        "Session {} serving {:?} -> {:?}",
        session_id, xremap_socket, user_socket
    );

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let user_socket = user_socket.clone();
                let session_id = session_id.clone();
                tokio::spawn(async move {
                    trace!("Session {} relaying to {:?}", session_id, user_socket);
                    if let Err(e) = relay_message(&user_socket, stream).await {
                        log::error!("Relay error: {}", e);
                    }
                    trace!("Session {} relay completed", session_id);
                });
            }
            Err(e) => {
                log::error!("Accept error: {}", e);
                break;
            }
        }
    }

    Ok(())
}

async fn relay_message(socket_path: &Path, mut in_stream: UnixStream) -> Result<()> {
    if !socket_path.exists() {
        trace!("Abort relay: {:?} does not exist", socket_path);
        return Ok(());
    }

    let mut out_stream = UnixStream::connect(socket_path).await?;

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

// D-Bus proxy definitions
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
