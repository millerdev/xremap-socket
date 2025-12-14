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
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use users::get_user_by_name;
use zbus::zvariant::OwnedObjectPath;
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
    let log_level = match args.verbose {0 => "info", 1 => "debug", _ => "trace"};
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
    let active_sessions: Arc<Mutex<HashMap<String, JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    monitor_existing_sessions(&connection, &active_sessions, &xremap_socket, &owner).await?;

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
                            xremap_socket.clone(),
                            owner.clone(),
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
    xremap_socket: &PathBuf,
    owner: &String,
) -> Result<(), anyhow::Error> {
    let manager = ManagerProxy::new(connection).await?;
    let sessions = manager.list_sessions().await?;
    Ok(for (session_id, uid, _user, seat_id, _session_path) in sessions {
        if !seat_id.is_empty() {
            debug!("Existing session: {} (uid={})", session_id, uid);
            let handle = spawn_session_handler(
                session_id.clone(),
                uid,
                xremap_socket.clone(),
                owner.clone(),
            );
            active_sessions.lock().await.insert(session_id, handle);
        }
    })
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
            Err(why) => error!("Session {} monitor failed: {}", session_id, why),
        }
    })
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
    if xremap_socket.exists() {
        fs::remove_file(&xremap_socket)?;
    }

    let listener = UnixListener::bind(&xremap_socket)?;
    let user = get_user_by_name(&owner).ok_or_else(|| anyhow::anyhow!("User not found: {}", owner))?;
    std::os::unix::fs::chown(&xremap_socket, Some(user.uid()), Some(user.primary_group_id()))?;
    fs::set_permissions(&xremap_socket, fs::Permissions::from_mode(0o660))?;

    debug!("Session {} serving {:?} -> {:?}", session_id, xremap_socket, user_socket);
    loop {
        let (stream, _addr) = listener.accept().await?;
        trace!("Session {} relaying to {:?}", session_id, user_socket);
        relay_message(stream, &user_socket).await?;
        trace!("Session {} relay completed", session_id);
    }
}

async fn relay_message(mut in_stream: UnixStream, socket_path: &Path) -> Result<()> {
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
