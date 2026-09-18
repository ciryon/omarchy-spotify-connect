// spotify-connect: Spotify Connect output switcher.
//   daemon            run the local receiver and serve state on a unix socket
//   login             one-time browser OAuth; credentials land in the state dir
//   status | devices  print JSON from the daemon
//   switch <id>       transfer playback to a Connect device
use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    process::exit,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use http::{HeaderMap, Method};
use protobuf::{Message as _, MessageField};
use librespot::{
    connect::{ConnectConfig, Spirc},
    core::{
        authentication::Credentials,
        cache::Cache,
        config::{DeviceType, SessionConfig},
        dealer::protocol::Message,
        session::Session,
    },
    oauth::OAuthClientBuilder,
    playback::{
        audio_backend,
        config::{AudioFormat, PlayerConfig},
        mixer::{self, MixerConfig},
        player::Player,
    },
    protocol::{
        connect::{Capabilities, Cluster, ClusterUpdate, Device, DeviceInfo, MemberType, PutStateReason, PutStateRequest},
        devices::DeviceType as ProtoDeviceType,
    },
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::UnixListener,
};

const LOCAL_NAME: &str = "This computer";

#[derive(Default)]
struct Shared {
    authenticated: bool,
    own_id: String,
    cluster: Option<Cluster>,
    session: Option<Session>,
}

type State = Arc<Mutex<Shared>>;

fn state_dir() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".local/state"))
        .join("spotify-connect")
}

fn socket_path() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("spotify-connect.sock")
}

fn cache() -> Cache {
    let dir = state_dir();
    fs::create_dir_all(&dir).expect("create state dir");
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    Cache::new(Some(&dir), None, None, None).expect("open credentials cache")
}

// Stable across restarts so Spotify keeps seeing the same "This computer".
fn device_id() -> String {
    let path = state_dir().join("device_id");
    if let Ok(id) = fs::read_to_string(&path) {
        return id.trim().to_string();
    }
    let id = fs::read_to_string("/proc/sys/kernel/random/uuid").expect("generate uuid");
    let id = id.trim().replace('-', "");
    fs::write(&path, &id).expect("write device_id");
    id
}

fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "librespot".into())
}

fn devices_json(cluster: &Cluster, own_id: &str) -> Value {
    let mut list: Vec<Value> = cluster
        .device
        .iter()
        .filter(|(id, _)| **id != observer_id(own_id))
        // Aliased devices (Echo) come either as one base entry or expanded to
        // `<base>_<suffix>` entries; the base one is a duplicate when both exist.
        .filter(|(id, d)| {
            d.device_aliases.is_empty() || !cluster.device.keys().any(|k| k.starts_with(&format!("{id}_")))
        })
        .map(|(id, d)| {
            let local = id == own_id;
            let alias = d.device_aliases.values().next().map(|a| a.display_name.clone());
            json!({
                "id": id,
                "name": if local { LOCAL_NAME.to_string() } else { alias.unwrap_or_else(|| d.name.clone()) },
                "type": if local { "local".to_string() } else {
                    format!("{:?}", d.device_type.enum_value_or_default()).to_lowercase()
                },
                "active": *id == cluster.active_device_id,
                "volume": (d.volume * 100).div_ceil(u32::from(u16::MAX)),
            })
        })
        .collect();
    list.sort_by_key(|d| (d["type"] != "local", d["name"].as_str().unwrap_or("").to_lowercase()));
    Value::Array(list)
}

fn status_json(cluster: &Cluster, own_id: &str) -> Value {
    let active = devices_json(cluster, own_id)
        .as_array()
        .and_then(|l| l.iter().find(|d| d["active"] == true).cloned())
        .map(|d| json!({ "id": d["id"], "name": d["name"], "volume": d["volume"] }));
    json!({ "activeDevice": active })
}

async fn handle(line: &str, state: &State) -> Value {
    let req: Value = serde_json::from_str(line).unwrap_or_default();
    let cmd = req["cmd"].as_str().unwrap_or_default().to_string();
    let (session, own_id, active) = {
        let mut s = state.lock().unwrap();
        if !s.authenticated {
            return json!({ "error": "not_authenticated" });
        }
        if s.session.is_none() {
            // Mock mode: no Spotify behind us, so apply commands to the fake cluster.
            let own_id = s.own_id.clone();
            let Some(cluster) = s.cluster.as_mut() else {
                return json!({ "error": "unavailable" });
            };
            return match cmd.as_str() {
                "status" => status_json(cluster, &own_id),
                "devices" => devices_json(cluster, &own_id),
                "switch" => match req["id"].as_str() {
                    Some(id) if cluster.device.contains_key(id) => {
                        cluster.active_device_id = id.to_string();
                        json!({ "ok": true })
                    }
                    _ => json!({ "error": "bad_request" }),
                },
                "volume" => match req["value"].as_u64().filter(|v| *v <= 100) {
                    Some(percent) => {
                        let active = cluster.active_device_id.clone();
                        match cluster.device.get_mut(&active) {
                            Some(d) => {
                                d.volume = (percent * u64::from(u16::MAX) / 100) as u32;
                                json!({ "ok": true })
                            }
                            None => json!({ "error": "no_active_device" }),
                        }
                    }
                    None => json!({ "error": "bad_request" }),
                },
                _ => json!({ "error": "bad_request" }),
            };
        }
        let (Some(cluster), Some(session)) = (&s.cluster, &s.session) else {
            return json!({ "error": "unavailable" });
        };
        match cmd.as_str() {
            "status" => return status_json(cluster, &s.own_id),
            "devices" => return devices_json(cluster, &s.own_id),
            "switch" | "volume" => (session.clone(), s.own_id.clone(), cluster.active_device_id.clone()),
            _ => return json!({ "error": "bad_request" }),
        }
    };

    if cmd == "volume" {
        let Some(percent) = req["value"].as_u64().filter(|v| *v <= 100) else {
            return json!({ "error": "bad_request" });
        };
        if active.is_empty() {
            return json!({ "error": "no_active_device" });
        }
        let raw = percent * u64::from(u16::MAX) / 100;
        let body = json!({ "volume": raw }).to_string();
        let endpoint = format!("/connect-state/v1/connect/volume/from/{own_id}/to/{active}");
        let mut headers = HeaderMap::new();
        match "application/json".parse() {
            Ok(value) => headers.insert("content-type", value),
            Err(e) => return json!({ "error": format!("bad_header: {e}") }),
        };
        return match session
            .spclient()
            .request(&Method::PUT, &endpoint, Some(headers), Some(body.as_bytes()))
            .await
        {
            Ok(_) => {
                if let Some(d) = state.lock().unwrap().cluster.as_mut().and_then(|c| c.device.get_mut(&active)) {
                    d.volume = (raw as u32).min(u16::MAX.into());
                }
                json!({ "ok": true })
            }
            Err(e) => {
                log::error!("set volume {percent} on {active} failed: {e}");
                json!({ "error": "volume_failed" })
            }
        };
    }

    let Some(target) = req["id"].as_str() else {
        return json!({ "error": "bad_request" });
    };
    // Nothing active: from == to lets Spotify resume the last session there.
    let from = if active.is_empty() { target } else { &active };
    match session.spclient().transfer(from, target, None).await {
        Ok(_) => {
            if let Some(c) = state.lock().unwrap().cluster.as_mut() {
                c.active_device_id = target.to_string();
            }
            json!({ "ok": true })
        }
        Err(e) => {
            log::error!("transfer {from} -> {target} failed: {e}");
            json!({ "error": "switch_failed" })
        }
    }
}

async fn serve(state: State) {
    let path = socket_path();
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind socket");
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let state = state.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            if tokio::io::BufReader::new(read).read_line(&mut line).await.is_ok() {
                let reply = handle(&line, &state).await;
                let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
            }
        });
    }
}

async fn fetch_cluster(session: &Session) -> Result<Cluster, librespot::core::Error> {
    let id = observer_id(session.device_id());
    let request = PutStateRequest {
        member_type: MemberType::CONNECT_STATE.into(),
        put_state_reason: PutStateReason::NEW_DEVICE.into(),
        device: MessageField::some(Device {
            device_info: MessageField::some(DeviceInfo {
                device_id: id.clone(),
                name: "spotify-connect observer".into(),
                client_id: session.client_id(),
                capabilities: MessageField::some(Capabilities {
                    hidden: true,
                    is_observable: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut headers = HeaderMap::new();
    headers.insert("x-spotify-connection-id", session.connection_id().parse()?);
    headers.insert("content-type", "application/x-protobuf".parse()?);
    let body = request.write_to_bytes()?;
    let reply = session
        .spclient()
        .request(&Method::PUT, &format!("/connect-state/v1/devices/{id}"), Some(headers), Some(&body))
        .await?;
    Ok(Cluster::parse_from_bytes(&reply)?)
}

fn observer_id(own_id: &str) -> String {
    format!("{own_id}obs")
}

async fn receive(state: &State, creds: Credentials) -> Result<(), librespot::core::Error> {
    let own_id = state.lock().unwrap().own_id.clone();
    let session = Session::new(SessionConfig { device_id: own_id, ..Default::default() }, Some(cache()));
    let mixer = mixer::find(None).expect("soft mixer")(MixerConfig::default())?;
    let sink = audio_backend::find(Some("pulseaudio".into())).expect("pulseaudio backend");
    let player = Player::new(PlayerConfig::default(), session.clone(), mixer.get_soft_volume(), move || {
        sink(None, AudioFormat::default())
    });
    // Must be registered before Spirc connects the session.
    let mut updates = session
        .dealer()
        .listen_for("hm://connect-state/v1/cluster", Message::from_raw::<ClusterUpdate>)?;
    let config = ConnectConfig { name: format!("Omarchy {}", hostname()), device_type: DeviceType::Computer, ..Default::default() };
    let (_spirc, task) = Spirc::new(config.clone(), session.clone(), creds, player, mixer).await?;
    state.lock().unwrap().session = Some(session.clone());

    // Spirc keeps the initial cluster (its put-state reply) private, and the dealer
    // only pushes later changes. Announce a hidden observer device once to read it.
    let bootstrap = async {
        while session.connection_id().is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        match fetch_cluster(&session).await {
            Ok(cluster) => {
                log::info!("initial cluster: {} devices", cluster.device.len());
                state.lock().unwrap().cluster.get_or_insert(cluster);
            }
            Err(e) => log::warn!("initial cluster request failed: {e}"),
        }
    };

    let watch = async {
        while let Some(update) = updates.next().await {
            match update {
                Ok(mut u) => {
                    if let Some(cluster) = u.cluster.take() {
                        log::info!("active device: {:?}", cluster.active_device_id);
                        state.lock().unwrap().cluster = Some(cluster);
                    }
                }
                Err(e) => log::warn!("bad cluster update: {e}"),
            }
        }
    };
    tokio::select! { _ = task => {}, _ = async { tokio::join!(bootstrap, watch) } => {} }
    Ok(())
}

// Canned cluster for screenshots: `spotify-connect daemon --mock`, with the
// real service stopped. Talks to nothing and keeps changes in memory.
fn mock_cluster(own_id: &str) -> Cluster {
    let mut cluster = Cluster::new();
    let devices = [
        (own_id, "Omarchy", ProtoDeviceType::COMPUTER, 40),
        ("mock-kitchen", "Kitchen", ProtoDeviceType::SPEAKER, 35),
        ("mock-living-room", "Living Room", ProtoDeviceType::SPEAKER, 55),
        ("mock-bedroom", "Bedroom Speaker", ProtoDeviceType::SPEAKER, 20),
        ("mock-office", "Office", ProtoDeviceType::SPEAKER, 45),
    ];
    for (id, name, device_type, percent) in devices {
        cluster.device.insert(
            id.to_string(),
            DeviceInfo {
                name: name.into(),
                device_id: id.into(),
                device_type: device_type.into(),
                volume: percent * u32::from(u16::MAX) / 100,
                ..Default::default()
            },
        );
    }
    cluster.active_device_id = "mock-living-room".into();
    cluster
}

async fn daemon(mock: bool) {
    let state: State = Default::default();
    fs::create_dir_all(state_dir()).expect("create state dir");
    state.lock().unwrap().own_id = device_id();
    if mock {
        {
            let mut s = state.lock().unwrap();
            s.authenticated = true;
            s.cluster = Some(mock_cluster(&s.own_id.clone()));
        }
        serve(state).await;
        return;
    }
    tokio::spawn(serve(state.clone()));
    loop {
        match cache().credentials() {
            // ponytail: polls for `login` output; inotify if the delay matters
            None => state.lock().unwrap().authenticated = false,
            Some(creds) => {
                state.lock().unwrap().authenticated = true;
                if let Err(e) = receive(&state, creds).await {
                    log::error!("session ended: {e}");
                }
                let mut s = state.lock().unwrap();
                s.session = None;
                s.cluster = None;
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn login() -> Result<(), Box<dyn std::error::Error>> {
    let config = SessionConfig::default();
    let token = OAuthClientBuilder::new(&config.client_id, "http://127.0.0.1:8898/login", vec!["streaming"])
        .open_in_browser()
        .build()?
        .get_access_token_async()
        .await?;
    Session::new(config, Some(cache()))
        .connect(Credentials::with_access_token(token.access_token), true)
        .await?;
    println!("Logged in. Credentials saved to {}", state_dir().display());
    Ok(())
}

fn ctl(req: Value) -> i32 {
    let reply = UnixStream::connect(socket_path()).and_then(|mut s| {
        writeln!(s, "{req}")?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line)?;
        Ok(line)
    });
    let line = match reply {
        Ok(l) if !l.trim().is_empty() => l,
        _ => json!({ "error": "unavailable" }).to_string(),
    };
    println!("{}", line.trim());
    let v: Value = serde_json::from_str(&line).unwrap_or_default();
    match v["error"].as_str() {
        None => 0,
        Some("unavailable") => 2,
        Some("not_authenticated") => 3,
        Some(_) => 1,
    }
}

#[tokio::main]
async fn main() {
    env_logger::builder().filter_level(log::LevelFilter::Info).init();
    let args: Vec<String> = env::args().skip(1).filter(|a| a != "--json").collect();
    let code = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["daemon"] => {
            daemon(false).await;
            0
        }
        ["daemon", "--mock"] => {
            daemon(true).await;
            0
        }
        ["login"] => match login().await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("login failed: {e}");
                1
            }
        },
        [cmd @ ("status" | "devices")] => ctl(json!({ "cmd": cmd })),
        ["switch", id] => ctl(json!({ "cmd": "switch", "id": id })),
        ["volume", v] => match v.parse::<u64>() {
            Ok(v) if v <= 100 => ctl(json!({ "cmd": "volume", "value": v })),
            _ => {
                eprintln!("volume takes 0-100");
                64
            }
        },
        _ => {
            eprintln!("usage: spotify-connect daemon [--mock] | login | status | devices | switch <device-id> | volume <0-100>");
            64
        }
    };
    exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot::protocol::devices::DeviceAlias;

    #[test]
    fn maps_cluster_to_devices() {
        let mut c = Cluster::new();
        for (id, name, t) in [("me", "arch", ProtoDeviceType::COMPUTER), ("p", "Portable", ProtoDeviceType::SPEAKER)] {
            let d = DeviceInfo { name: name.into(), device_type: t.into(), ..Default::default() };
            c.device.insert(id.into(), d);
        }
        let alias = |name: &str| DeviceAlias { id: 1, display_name: name.into(), ..Default::default() };
        let echo = |name: &str| DeviceInfo { name: name.into(), ..Default::default() };
        let mut base = echo("echo1");
        base.device_aliases.insert("".into(), alias("Kitchen"));
        c.device.insert("echo1".into(), base.clone());
        c.device.insert("echo1_amzn_1".into(), echo("Kitchen"));
        let mut lone = echo("echo2");
        lone.device_aliases.insert("".into(), alias("Bedroom"));
        c.device.insert("echo2".into(), lone);
        if let Some(p) = c.device.get_mut("p") { p.volume = u16::MAX as u32 / 2; }
        c.active_device_id = "p".into();
        assert_eq!(
            devices_json(&c, "me"),
            json!([
                { "id": "me", "name": "This computer", "type": "local", "active": false, "volume": 0 },
                { "id": "echo2", "name": "Bedroom", "type": "unknown", "active": false, "volume": 0 },
                { "id": "echo1_amzn_1", "name": "Kitchen", "type": "unknown", "active": false, "volume": 0 },
                { "id": "p", "name": "Portable", "type": "speaker", "active": true, "volume": 50 },
            ])
        );
        assert_eq!(
            status_json(&c, "me"),
            json!({ "activeDevice": { "id": "p", "name": "Portable", "volume": 50 } })
        );
    }
}
