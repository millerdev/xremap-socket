use anyhow::Result;
use futures_util::stream::StreamExt;
use log::{debug, info, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use zbus::{Connection, MatchRule, Message, MessageStream};
use zbus::fdo::DBusProxy;
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::{Args};

pub struct SessionMonitor {
    cfg: Arc<Args>,
    sessions: Arc<Mutex<HashMap<OwnedObjectPath, Session>>>,
    active_sessions: Arc<Mutex<HashMap<OwnedObjectPath, Session>>>,
    connection: Connection,
    proxy: DBusProxy<'static>,
}

impl SessionMonitor {
    pub async fn new(cfg: Arc<Args>) -> Result<Self> {
        let connection = Connection::system().await?;
        let proxy = DBusProxy::new(&connection).await?;
        Ok(SessionMonitor {
            cfg,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
            connection,
            proxy,
        })
    }

    pub async fn get_active_session(&self) -> Option<Session> {
        let active = self.active_sessions.lock().await;
        if active.len() > 1 {
            warn!("Unexpected: multiple active sessions: {:?}", active.keys());
            return None;
        }
        active.values().next().cloned()
    }

    pub async fn run(&self) -> Result<()> {
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

        self.proxy.add_match_rule(session_new_rule).await?;
        self.proxy.add_match_rule(session_removed_rule).await?;

        debug!("Monitoring user sessions...");
        if let Err(why) = self.monitor_existing_sessions().await {
            warn!("Cannot monitor existing sessions: {}", why)
        };
        while let Some(msg) = MessageStream::from(&self.connection).next().await {
            match msg {
                Ok(message) => {
                    if let Err(handle_err) = self.handle_message(&message).await {
                        warn!("Could not handle {:?}: {}", message, handle_err)
                    }
                },
                Err(why) => warn!("Message fail: {}", why)
            };
        }
        Ok(())
    }

    async fn handle_message(&self, message: &Message) -> Result<()> {
        let header = message.header();
        let member = match header.member() {
            Some(m) => m,
            None => return Ok(()),  // ignore null member
        };
        match member.as_str() {
            "SessionNew" => {
                let (session_id, session_path): (String, OwnedObjectPath) =
                    message.body().deserialize()?;
                self.handle_new_session(&session_id, &session_path).await?;
            }
            "PropertiesChanged" => {
                let header = message.header();
                let path_ref = header.path()
                    .ok_or_else(|| anyhow::anyhow!("No path in message"))?;
                let session_path: OwnedObjectPath = path_ref.clone().into();
                let body = message.body();
                let (_name, changed, _invalidated): (String, HashMap<String, Value<'_>>, Vec<String>) =
                    body.deserialize()?;
                self.handle_properties_changed(session_path, changed).await;
            }
            "SessionRemoved" => {
                let (session_id, session_path): (String, OwnedObjectPath) =
                    message.body().deserialize()?;
                let session = self.sessions.lock().await.remove(&session_path);
                let active = self.active_sessions.lock().await.remove(&session_path);
                if session.is_some() || active.is_some() {
                    self.remove_properties_changed_match_rule(&session_path).await?;
                    if session.is_some() {
                        info!("Removed session {}", session_id);
                    } else if active.is_some() {
                        warn!("Discarded unknown active session {}", session_id);
                    }
                }
            }
            sig => warn!("Ignored message: {}", sig)
        };
        Ok(())
    }

    async fn handle_new_session(
        &self,
        session_id: &String,
        session_path: &OwnedObjectPath,
    ) -> Result<()> {
        let session_proxy = SessionProxy::builder(&self.connection).path(session_path)?.build().await?;
        let (seat_id, _seat_path) = session_proxy.seat().await?;
        if seat_id.is_empty() {
            debug!("Ignoring unseated session {}", session_id);
            return Ok(());
        }
        let (uid, _user_path) = session_proxy.user().await?;
        let is_active = session_proxy.active().await?;
        let session = Session {
            id: session_id.clone(),
            user_socket: user_socket_path(self.cfg.user_socket.clone(), uid),
        };
        let active_str = if is_active { " active" } else { "" };
        info!("Monitoring{} session {} (uid={}, seat={})", active_str, session_id, uid, seat_id);
        self.add_properties_changed_match_rule(&session_path).await?;
        self.sessions.lock().await.insert(session_path.clone(), session.clone());
        if is_active {
            self.active_sessions.lock().await.insert(session_path.clone(), session);
        }
        Ok(())
    }

    async fn handle_properties_changed(
        &self,
        session_path: OwnedObjectPath,
        changed: HashMap<String, Value<'_>>,
    ) {
        if let Some(Value::Bool(is_active)) = changed.get("Active") {
            if let Some(session) = self.sessions.lock().await.get(&session_path) {
                let mut active_guard = self.active_sessions.lock().await;
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

    async fn monitor_existing_sessions(&self) -> Result<()> {
        let connection = &self.connection;
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
                user_socket: user_socket_path(self.cfg.user_socket.clone(), uid),
            };
            if is_active {
                self.active_sessions.lock().await.insert(session_path.clone(), session.clone());
            }
            self.sessions.lock().await.insert(session_path.clone(), session);
            self.add_properties_changed_match_rule(&session_path).await?
        }
        Ok(())
    }

    async fn add_properties_changed_match_rule(
        &self,
        session_path: &OwnedObjectPath,
    ) -> Result<()> {
        let rule = session_changed_rule(session_path)?;
        Ok(self.proxy.add_match_rule(rule).await?)
    }

    async fn remove_properties_changed_match_rule(
        &self,
        session_path: &OwnedObjectPath,
    ) -> Result<()> {
        let rule = session_changed_rule(session_path)?;
        Ok(self.proxy.remove_match_rule(rule).await?)
    }
}

fn session_changed_rule(session_path: &OwnedObjectPath) -> Result<MatchRule<'_>> {
    Ok(MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .path(session_path)?
        .member("PropertiesChanged")?
        .build())
}

fn user_socket_path(user_socket: String, uid: u32) -> PathBuf {
    PathBuf::from(user_socket.replace("{uid}", &uid.to_string()))
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub user_socket: PathBuf,
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
