use crate::config::Config;
use crate::connection::ActiveConnections;
use crate::event::{
    Activity, Custom, Event, GroupUpdate, Notification, PreAuth, ShareCreate, StorageUpdate,
};
use crate::message::{DebounceMap, MessageType};
use crate::metrics::METRICS;
use crate::storage_mapping::StorageMapping;
pub use crate::user::UserId;
use ahash::RandomState;
use color_eyre::{eyre::WrapErr, Report, Result};
use dashmap::DashMap;
use futures::{future::select, pin_mut, SinkExt, StreamExt};
use smallvec::alloc::sync::Arc;
use sqlx::AnyPool;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::time::timeout;
use warp::filters::addr::remote;
use warp::filters::ws::Message;
use warp::ws::WebSocket;
use warp::Filter;
use warp_real_ip::get_forwarded_for;

pub mod config;
pub mod connection;
pub mod event;
pub mod message;
pub mod metrics;
pub mod nc;
pub mod storage_mapping;
pub mod user;

pub struct App {
    connections: ActiveConnections,
    nc_client: nc::Client,
    storage_mapping: StorageMapping,
    pre_auth: DashMap<String, (Instant, UserId), RandomState>,
    test_cookie: AtomicU32,
    redis_url: String,
}

impl App {
    pub async fn new(config: Config) -> Result<Self> {
        let connections = ActiveConnections::default();
        let nc_client = nc::Client::new(&config.nextcloud_url)?;
        let test_cookie = AtomicU32::new(0);

        let storage_mapping =
            StorageMapping::new(&config.database_url, config.database_prefix).await?;
        let pre_auth = DashMap::default();

        let redis_url = config.redis_url;

        Ok(App {
            connections,
            nc_client,
            test_cookie,
            pre_auth,
            storage_mapping,
            redis_url,
        })
    }

    pub async fn with_connection(connection: AnyPool, config: Config) -> Result<Self> {
        let connections = ActiveConnections::default();
        let nc_client = nc::Client::new(&config.nextcloud_url)?;
        let test_cookie = AtomicU32::new(0);

        let storage_mapping =
            StorageMapping::from_connection(connection, config.database_prefix).await?;
        let pre_auth = DashMap::default();

        let redis_url = config.redis_url;

        Ok(App {
            connections,
            nc_client,
            test_cookie,
            pre_auth,
            storage_mapping,
            redis_url,
        })
    }

    pub async fn self_test(&self) -> Result<()> {
        let _ = self
            .storage_mapping
            .get_users_for_storage_path(1, "")
            .await
            .wrap_err("Failed to test database access")?;
        Ok(())
    }

    async fn handle_event(&self, event: Event) {
        match event {
            Event::StorageUpdate(StorageUpdate { storage, path }) => {
                match self
                    .storage_mapping
                    .get_users_for_storage_path(storage, &path)
                    .await
                {
                    Ok(users) => {
                        for user in users {
                            self.connections
                                .send_to_user(&user, MessageType::File)
                                .await;
                        }
                    }
                    Err(e) => log::error!("{:#}", e),
                }
            }
            Event::GroupUpdate(GroupUpdate { user, .. }) => {
                self.connections
                    .send_to_user(&user, MessageType::File)
                    .await;
            }
            Event::ShareCreate(ShareCreate { user }) => {
                self.connections
                    .send_to_user(&user, MessageType::File)
                    .await;
            }
            Event::TestCookie(cookie) => {
                self.test_cookie.store(cookie, Ordering::SeqCst);
            }
            Event::Activity(Activity { user }) => {
                self.connections
                    .send_to_user(&user, MessageType::Activity)
                    .await;
            }
            Event::Notification(Notification { user }) => {
                self.connections
                    .send_to_user(&user, MessageType::Notification)
                    .await;
            }
            Event::PreAuth(PreAuth { user, token }) => {
                self.pre_auth.insert(token, (Instant::now(), user));
            }
            Event::Custom(Custom { user, message }) => {
                self.connections
                    .send_to_user(&user, MessageType::Custom(message))
                    .await;
            }
        }
    }
}

pub async fn serve(app: Arc<App>, port: u16) {
    let app = warp::any().map(move || app.clone());

    let cors = warp::cors().allow_any_origin();

    // GET /ws -> websocket upgrade
    let socket = warp::path!("ws")
        // The `ws()` filter will prepare Websocket handshake...
        .and(warp::ws())
        .and(app.clone())
        .and(remote())
        .and(get_forwarded_for())
        .map(
            |ws: warp::ws::Ws, app, remote: Option<SocketAddr>, mut forwarded_for: Vec<IpAddr>| {
                if let Some(remote) = remote {
                    forwarded_for.push(remote.ip());
                }
                log::debug!("new websocket connection from {:?}", forwarded_for.first());
                ws.on_upgrade(move |socket| user_connected(socket, app, forwarded_for))
            },
        )
        .with(cors);

    let cookie_test = warp::path!("test" / "cookie")
        .and(app.clone())
        .map(|app: Arc<App>| {
            let cookie = app.test_cookie.load(Ordering::SeqCst);
            log::debug!("current test cookie is {}", cookie);
            cookie.to_string()
        });

    let reverse_cookie_test = warp::path!("test" / "reverse_cookie")
        .and(app.clone())
        .and_then(|app: Arc<App>| async move {
            let cookie = app.nc_client.get_test_cookie().await.unwrap_or(0);
            log::debug!("got remote test cookie {}", cookie);
            Result::<_, Infallible>::Ok(cookie.to_string())
        });

    let mapping_test = warp::path!("test" / "mapping" / u32)
        .and(app.clone())
        .and_then(|storage_id: u32, app: Arc<App>| async move {
            let access = app
                .storage_mapping
                .get_users_for_storage_path(storage_id, "")
                .await
                .map(|access| {
                    let count = access.count();
                    log::debug!("storage mapping count for {} = {}", storage_id, count);
                    count
                })
                .map_err(|err| {
                    log::error!(
                        "error while getting mapping count for {}: {:#}",
                        storage_id,
                        err
                    );
                    err
                })
                .unwrap_or(0);
            Result::<_, Infallible>::Ok(access.to_string())
        });

    let remote_test = warp::path!("test" / "remote" / IpAddr)
        .and(app.clone())
        .and_then(|remote: IpAddr, app: Arc<App>| async move {
            let result = app
                .nc_client
                .test_set_remote(remote)
                .await
                .map(|remote| remote.to_string())
                .unwrap_or_else(|e| e.to_string());
            log::debug!("got remote {} when trying to set remote {}", result, remote);
            Result::<_, Infallible>::Ok(result)
        });

    let routes = socket
        .or(cookie_test)
        .or(reverse_cookie_test)
        .or(mapping_test)
        .or(remote_test);

    warp::serve(routes).run(([0, 0, 0, 0], port)).await;
}

async fn user_connected(mut ws: WebSocket, app: Arc<App>, forwarded_for: Vec<IpAddr>) {
    let user_id = match socket_auth(&mut ws, forwarded_for, &app).await {
        Ok(user_id) => user_id,
        Err(e) => {
            log::warn!("{}", e);
            ws.send(Message::text(format!("err: {}", e))).await.ok();
            return;
        }
    };

    log::debug!("new websocket authenticated as {}", user_id);
    ws.send(Message::text("authenticated")).await.ok();

    METRICS.add_connection();

    let (mut user_ws_tx, mut user_ws_rx) = ws.split();

    let mut rx = app.connections.add(user_id.clone()).await;

    let transmit = async move {
        let mut debounce = DebounceMap::default();
        loop {
            // we dont care about dropped messages
            if let Ok(msg) = rx.recv().await {
                if debounce.should_send(&msg) {
                    METRICS.add_message();
                    user_ws_tx.send(Message::text(msg.to_string())).await.ok();
                }
            }
        }
    };

    let receive = async move {
        // handle messages until the client closes the connection
        while let Some(result) = user_ws_rx.next().await {
            let _msg = match result {
                Ok(msg) => msg,
                Err(e) => {
                    log::warn!("websocket error: {}", e);
                    break;
                }
            };
        }
    };

    pin_mut!(transmit);
    pin_mut!(receive);

    select(transmit, receive).await;

    METRICS.remove_connection();
}

async fn read_socket_auth_message(rx: &mut WebSocket) -> Result<Message> {
    match timeout(Duration::from_secs(1), rx.next()).await {
        Ok(Some(Ok(msg))) => Ok(msg),
        Ok(Some(Err(e))) => Err(Report::from(e).wrap_err("Socket error during authentication")),
        Ok(None) => Err(Report::msg("Client disconnected during authentication")),
        Err(_) => Err(Report::msg("Authentication timeout")),
    }
}

async fn socket_auth(rx: &mut WebSocket, forwarded_for: Vec<IpAddr>, app: &App) -> Result<UserId> {
    let username_msg = read_socket_auth_message(rx).await?;
    let username = username_msg
        .to_str()
        .map_err(|_| Report::msg("Invalid authentication message"))?;
    let password_msg = read_socket_auth_message(rx).await?;
    let password = password_msg
        .to_str()
        .map_err(|_| Report::msg("Invalid authentication message"))?;

    // cleanup all pre_auth tokens older than 15s
    let cutoff = Instant::now() - Duration::from_secs(15);
    app.pre_auth.retain(|_, (time, _)| *time > cutoff);

    if let Some((_, (_, user))) = app.pre_auth.remove(password) {
        log::info!(
            "Authenticated socket for {} using pre authenticated token",
            user
        );
        return Ok(user);
    }

    if !username.is_empty() {
        app.nc_client
            .verify_credentials(username, password, forwarded_for)
            .await
    } else {
        Err(Report::msg("Invalid credentials"))
    }
}

pub async fn listen(app: Arc<App>) -> Result<()> {
    let client = redis::Client::open(app.redis_url.clone())?;
    let mut event_stream = event::subscribe(client).await?;

    let handle = move |event: Event| {
        // todo: any way to do this without cloning the arc every event (scoped?)
        let app = app.clone();
        async move {
            app.handle_event(event).await;
        }
    };

    while let Some(event) = event_stream.next().await {
        match event {
            Ok(event) => {
                log::debug!(
                    target: "notify_push::receive",
                    "Received {}",
                    event
                );
                tokio::spawn(handle(event));
            }
            Err(e) => log::warn!("{:#}", e),
        }
    }
    Ok(())
}