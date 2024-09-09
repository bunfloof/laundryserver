use actix_cors::Cors;
use actix_files as fs;
use actix_web::{get, post, web, App, HttpRequest, HttpResponse, HttpServer, Responder, Result};
use actix_ws::{Message as WsMessage, Session};
use chrono::{Local, Utc};
use colored::*;
use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use rusqlite::{params, Connection, Result as SqliteResult};
use serde::{Deserialize, Serialize};
use serde_json::{self, json, Value};
use std::collections::HashMap;
use std::fs as std_fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::time;

#[derive(Clone, Serialize)]
struct Client {
    location: String,
    room: String,
    #[serde(skip_serializing)]
    last_heartbeat: Instant,
    #[serde(skip_serializing)]
    session: Session,
    #[serde(skip_serializing)]
    response_sender: Arc<Mutex<Option<oneshot::Sender<String>>>>,
}

type ClientList = Arc<Mutex<HashMap<(String, String), Client>>>;

#[derive(Deserialize)]
struct LaundryRequest {
    location: String,
    room: String,
    machine: String,
}

#[derive(Serialize, Deserialize)]
struct ServerMessage {
    action: String,
    payload: serde_json::Value,
}

#[derive(Deserialize)]
struct LocationQuery {
    location: String,
}

#[derive(Deserialize)]
struct RoomQuery {
    location: String,
    room: String,
}

struct AppState {
    db: Mutex<Connection>,
    db_update_in_progress: Arc<AtomicBool>,
    archiver_in_progress: Arc<AtomicBool>,
}


fn init_db(conn: &Connection) -> SqliteResult<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS machines (
            qr_code TEXT PRIMARY KEY,
            data TEXT NOT NULL,
            last_updated TEXT NOT NULL
        )",
        [],
    )?;
    Ok(())
}

fn update_db(conn: &Connection) -> rusqlite::Result<()> {
    log_with_timestamp("Starting database update process", "INFO");
    
    let room_dir = PathBuf::from("room");
    
    log_with_timestamp(&format!("Attempting to access room directory: {}", room_dir.display()), "INFO");

    if !room_dir.exists() || !room_dir.is_dir() {
        log_with_timestamp(&format!("Room directory does not exist or is not a directory: {}", room_dir.display()), "ERROR");
        return Ok(());
    }

    conn.execute("DELETE FROM machines", [])?;
    log_with_timestamp("Cleared existing data from machines table", "INFO");

    let mut total_json_files = 0;
    let mut total_processed_qr_codes = 0;

    match std_fs::read_dir(&room_dir) {
        Ok(entries) => {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    log_with_timestamp(&format!("Processing subfolder: {}", path.display()), "INFO");
                    
                    match std_fs::read_dir(&path) {
                        Ok(file_entries) => {
                            let json_files: Vec<_> = file_entries
                                .filter_map(Result::ok)
                                .filter(|file_entry| {
                                    file_entry.path().extension().map_or(false, |ext| ext == "json")
                                })
                                .collect();
                            
                            total_json_files += json_files.len();
                            log_with_timestamp(&format!("Found {} JSON files in {}", json_files.len(), path.display()), "INFO");

                            for file in json_files {
                                let file_path = file.path();
                                log_with_timestamp(&format!("Processing file: {}", file_path.display()), "INFO");
                                
                                match std_fs::read_to_string(&file_path) {
                                    Ok(content) => {
                                        match serde_json::from_str::<Vec<Value>>(&content) {
                                            Ok(json) => {
                                                for obj in json {
                                                    if let Some(qr_code) = obj["qrCodeId"].as_str() {
                                                        match conn.execute(
                                                            "INSERT INTO machines (qr_code, data, last_updated) VALUES (?, ?, ?)",
                                                            params![qr_code, obj.to_string(), Utc::now().to_string()],
                                                        ) {
                                                            Ok(_) => {
                                                                total_processed_qr_codes += 1;
                                                                log_with_timestamp(&format!("Inserted QR code: {}", qr_code), "INFO");
                                                            },
                                                            Err(e) => log_with_timestamp(&format!("Error inserting QR code {}: {}", qr_code, e), "ERROR"),
                                                        }
                                                    } else {
                                                        log_with_timestamp(&format!("Object missing QR code: {:?}", obj), "WARN");
                                                    }
                                                }
                                            },
                                            Err(e) => log_with_timestamp(&format!("Error parsing JSON from {}: {}", file_path.display(), e), "ERROR"),
                                        }
                                    },
                                    Err(e) => log_with_timestamp(&format!("Error reading file {}: {}", file_path.display(), e), "ERROR"),
                                }
                            }
                        },
                        Err(e) => log_with_timestamp(&format!("Error reading subfolder {}: {}", path.display(), e), "ERROR"),
                    }
                }
            }
        },
        Err(e) => {
            log_with_timestamp(&format!("Error reading room directory {}: {}", room_dir.display(), e), "ERROR");
            log_with_timestamp(&format!("Error kind: {:?}", e.kind()), "ERROR");
            if let Some(os_error) = e.raw_os_error() {
                log_with_timestamp(&format!("OS Error code: {}", os_error), "ERROR");
            }
        }
    }

    log_with_timestamp(&format!("Database update process completed. Processed {} JSON files and {} QR codes", total_json_files, total_processed_qr_codes), "INFO");
    Ok(())
}

fn log_with_timestamp(message: &str, log_type: &str) {
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let colored_message = match log_type {
        "INFO" => format!("[{}] {}", timestamp.blue(), message.green()),
        "WARN" => format!("[{}] {}", timestamp.blue(), message.yellow()),
        "ERROR" => format!("[{}] {}", timestamp.blue(), message.red()),
        _ => format!("[{}] {}", timestamp.blue(), message),
    };
    println!("{}", colored_message);
}

async fn ws_handler(
    req: HttpRequest,
    body: web::Payload,
    clients: web::Data<ClientList>,
) -> Result<HttpResponse> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, body)?;

    let clients_clone = clients.clone();
    actix_web::rt::spawn(async move {
        let mut client_info: Option<(String, String)> = None;
        let mut last_ping = Instant::now();

        while let Some(Ok(msg)) = msg_stream.next().await {
            match msg {
                WsMessage::Ping(bytes) => {
                    //log_with_timestamp("Received Ping from client", "INFO");
                    if session.pong(&bytes).await.is_err() {
                        //log_with_timestamp("Failed to send Pong to client", "ERROR");
                        break;
                    }
                }
                WsMessage::Text(text) => {
                    //log_with_timestamp(&format!("Received text message: {}", text), "INFO");
                    if let Ok(server_message) = serde_json::from_str::<ServerMessage>(&text) {
                        match server_message.action.as_str() {
                            "CONNECT" | "KEEP_ALIVE" => {
                                let location = server_message.payload["location"].as_str().unwrap_or("").to_string();
                                let room = server_message.payload["room"].as_str().unwrap_or("").to_string();
                                if !location.is_empty() && !room.is_empty() {
                                    let key = (location.clone(), room.clone());
                                    let mut clients_guard = clients_clone.lock().unwrap();
                                    
                                    let client = clients_guard.entry(key.clone()).or_insert_with(|| Client {
                                        location: location.clone(),
                                        room: room.clone(),
                                        last_heartbeat: Instant::now(),
                                        session: session.clone(),
                                        response_sender: Arc::new(Mutex::new(None)),
                                    });
                                    
                                    client.last_heartbeat = Instant::now();
                                    client.session = session.clone();
                                    
                                    client_info = Some(key.clone());
                                    
                                    // Log only CONNECT actions
                                    if server_message.action == "CONNECT" {
                                        log_with_timestamp(&format!("Client connected: ({}, {})", location, room), "INFO");
                                    }
                                    // KEEP_ALIVE logging
                                    // else {
                                    //     log_with_timestamp(&format!("Client sent keep-alive: ({}, {})", location, room), "INFO");
                                    // }
                                }
                            }
                            "START_MACHINE_RESPONSE" => {
                                if let Some(ref key) = client_info {
                                    let mut clients_guard = clients_clone.lock().unwrap();
                                    if let Some(client) = clients_guard.get_mut(key) {
                                        let mut sender = client.response_sender.lock().unwrap();
                                        if let Some(s) = sender.take() {
                                            let _ = s.send(serde_json::to_string(&server_message.payload).unwrap());
                                            log_with_timestamp(&format!("Sent START_MACHINE response to waiting request for client: {:?}", key), "INFO");
                                        } else {
                                            log_with_timestamp(&format!("Received START_MACHINE response but no waiting request for client: {:?}", key), "WARN");
                                        }
                                    }
                                }
                            }
                            "MACHINE_STATUS_RESPONSE" => {
                                if let Some(ref key) = client_info {
                                    let mut clients_guard = clients_clone.lock().unwrap();
                                    if let Some(client) = clients_guard.get_mut(key) {
                                        let mut sender = client.response_sender.lock().unwrap();
                                        if let Some(s) = sender.take() {
                                            let _ = s.send(serde_json::to_string(&server_message.payload).unwrap());
                                            log_with_timestamp(&format!("Sent MACHINE_STATUS response to waiting request for client: {:?}", key), "INFO");
                                        } else {
                                            log_with_timestamp(&format!("Received MACHINE_STATUS response but no waiting request for client: {:?}", key), "WARN");
                                        }
                                    }
                                }
                            }
                            "MACHINE_HEALTH_RESPONSE" => {
                                if let Some(ref key) = client_info {
                                    let mut clients_guard = clients_clone.lock().unwrap();
                                    if let Some(client) = clients_guard.get_mut(key) {
                                        let mut sender = client.response_sender.lock().unwrap();
                                        if let Some(s) = sender.take() {
                                            let _ = s.send(serde_json::to_string(&server_message.payload).unwrap());
                                            log_with_timestamp(&format!("Sent MACHINE_HEALTH response to waiting request for client: {:?}", key), "INFO");
                                        } else {
                                            log_with_timestamp(&format!("Received MACHINE_HEALTH response but no waiting request for client: {:?}", key), "WARN");
                                        }
                                    }
                                }
                            }
                            "MACHINES_RESPONSE" => {
                                if let Some(ref key) = client_info {
                                    let mut clients_guard = clients_clone.lock().unwrap();
                                    if let Some(client) = clients_guard.get_mut(key) {
                                        let mut sender = client.response_sender.lock().unwrap();
                                        if let Some(s) = sender.take() {
                                            let _ = s.send(serde_json::to_string(&server_message.payload).unwrap());
                                            log_with_timestamp(&format!("Sent MACHINES_RESPONSE to waiting request for client: {:?}", key), "INFO");
                                        } else {
                                            log_with_timestamp(&format!("Received MACHINES_RESPONSE but no waiting request for client: {:?}", key), "WARN");
                                        }
                                    }
                                }
                            }
                            _ => {
                                log_with_timestamp(&format!("Received unknown action: {}", server_message.action), "WARN");
                            }
                        }
                    } else {
                        log_with_timestamp(&format!("Received invalid message format: {}", text), "WARN");
                    }
                }
                WsMessage::Close(reason) => {
                    log_with_timestamp(&format!("Received Close message: {:?}", reason), "WARN");
                    break;
                }
                _ => {
                    //log_with_timestamp("Received other type of message", "INFO");
                }
            }

            if last_ping.elapsed() > Duration::from_secs(20) { // was 5 secs
                //log_with_timestamp("Sending Ping to client", "INFO");
                if session.ping(b"").await.is_err() {
                    //log_with_timestamp("Failed to send Ping to client", "ERROR");
                    break;
                }
                last_ping = Instant::now();
            }
        }

        if let Some(key) = client_info {
            clients_clone.lock().unwrap().remove(&key);
            log_with_timestamp(&format!("Client removed: {:?}", key), "WARN");
        }

        let _ = session.close(None).await;
    });

    Ok(response)
}

#[post("/")]
async fn laundry_handler(
    data: web::Json<LaundryRequest>,
    clients: web::Data<ClientList>,
) -> impl Responder {
    log_with_timestamp(
        &format!(
            "POST request - Location: {}, Room: {}, Machine: {}",
            data.location, data.room, data.machine
        ),
        "INFO",
    );
    let key = (data.location.clone(), data.room.clone());
    let (tx, rx) = oneshot::channel();

    {
        let mut clients_guard = clients.lock().unwrap();
        if let Some(client) = clients_guard.get_mut(&key) {
            let mut sender = client.response_sender.lock().unwrap();
            *sender = Some(tx);

            let message = ServerMessage {
                action: "START_MACHINE".to_string(),
                payload: serde_json::json!({
                    "machine": data.machine,
                    "location": data.location,
                    "room": data.room,
                }),
            };
            
            if let Err(e) = client.session.text(serde_json::to_string(&message).unwrap()).await {
                log_with_timestamp(&format!("Failed to send message to client: {:?}. Error: {}", key, e), "ERROR");
                return HttpResponse::InternalServerError().body("Failed to send message to the client");
            }
            log_with_timestamp(&format!("Message sent to client: {:?}", key), "INFO");
        } else {
            log_with_timestamp("Client not found", "WARN");
            return HttpResponse::NotFound().body("Client not found");
        }
    }

    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(response)) => HttpResponse::Ok().body(response),
        Ok(Err(_)) => HttpResponse::InternalServerError().body("Failed to receive response from client"),
        Err(_) => HttpResponse::RequestTimeout().body("Request timed out"),
    }
}

#[get("/machines")]
async fn get_machines(
    web::Query(params): web::Query<HashMap<String, String>>,
    clients: web::Data<ClientList>,
) -> impl Responder {
    let location = params.get("location");
    let room = params.get("room");

    if let (Some(location), Some(room)) = (location, room) {
        let key = (location.clone(), room.clone());
        let (tx, rx) = oneshot::channel();

        {
            let mut clients_guard = clients.lock().unwrap();
            if let Some(client) = clients_guard.get_mut(&key) {
                let mut sender = client.response_sender.lock().unwrap();
                *sender = Some(tx);

                let message = ServerMessage {
                    action: "GET_MACHINES".to_string(),
                    payload: serde_json::json!({}),
                };
                
                if let Err(e) = client.session.text(serde_json::to_string(&message).unwrap()).await {
                    log_with_timestamp(&format!("Failed to send GET_MACHINES request to client: {:?}. Error: {}", key, e), "ERROR");
                    return HttpResponse::InternalServerError().body("Failed to send request to the client");
                }
            } else {
                return HttpResponse::NotFound().body("Client not found");
            }
        }

        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(response)) => HttpResponse::Ok().body(response),
            Ok(Err(_)) => HttpResponse::InternalServerError().body("Failed to receive response from client"),
            Err(_) => HttpResponse::RequestTimeout().body("Request timed out"),
        }
    } else {
        HttpResponse::BadRequest().body("Missing location or room parameters")
    }
}

#[get("/machinestatus")]
async fn machine_status_handler(
    web::Query(params): web::Query<LaundryRequest>,
    clients: web::Data<ClientList>,
) -> impl Responder {
    log_with_timestamp(
        &format!(
            "GET request - Location: {}, Room: {}, Machine: {}",
            params.location, params.room, params.machine
        ),
        "INFO",
    );
    let key = (params.location.clone(), params.room.clone());
    let (tx, rx) = oneshot::channel();

    {
        let mut clients_guard = clients.lock().unwrap();
        if let Some(client) = clients_guard.get_mut(&key) {
            let mut sender = client.response_sender.lock().unwrap();
            *sender = Some(tx);

            let message = ServerMessage {
                action: "GET_MACHINE_STATUS".to_string(),
                payload: serde_json::json!({
                    "machine": params.machine,
                    "location": params.location,
                    "room": params.room,
                }),
            };
            
            if let Err(e) = client.session.text(serde_json::to_string(&message).unwrap()).await {
                log_with_timestamp(&format!("Failed to send GET_MACHINE_STATUS request to client: {:?}. Error: {}", key, e), "ERROR");
                return HttpResponse::InternalServerError().body("Failed to send request to the client");
            }
            log_with_timestamp(&format!("GET_MACHINE_STATUS request sent to client: {:?}", key), "INFO");
        } else {
            log_with_timestamp("Client not found", "WARN");
            return HttpResponse::NotFound().body("Client not found");
        }
    }

    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(response)) => HttpResponse::Ok().body(response),
        Ok(Err(_)) => HttpResponse::InternalServerError().body("Failed to receive response from client"),
        Err(_) => HttpResponse::RequestTimeout().body("Request timed out"),
    }
}

#[get("/machinehealth")]
async fn machine_health_handler(
    web::Query(params): web::Query<LaundryRequest>,
    clients: web::Data<ClientList>,
) -> impl Responder {
    log_with_timestamp(
        &format!(
            "GET request - Location: {}, Room: {}, Machine: {}",
            params.location, params.room, params.machine
        ),
        "INFO",
    );
    let key = (params.location.clone(), params.room.clone());
    let (tx, rx) = oneshot::channel();

    {
        let mut clients_guard = clients.lock().unwrap();
        if let Some(client) = clients_guard.get_mut(&key) {
            let mut sender = client.response_sender.lock().unwrap();
            *sender = Some(tx);

            let message = ServerMessage {
                action: "GET_MACHINE_HEALTH".to_string(),
                payload: serde_json::json!({
                    "machine": params.machine,
                    "location": params.location,
                    "room": params.room,
                }),
            };
            
            if let Err(e) = client.session.text(serde_json::to_string(&message).unwrap()).await {
                log_with_timestamp(&format!("Failed to send GET_MACHINE_HEALTH request to client: {:?}. Error: {}", key, e), "ERROR");
                return HttpResponse::InternalServerError().body("Failed to send request to the client");
            }
            log_with_timestamp(&format!("GET_MACHINE_HEALTH request sent to client: {:?}", key), "INFO");
        } else {
            log_with_timestamp("Client not found", "WARN");
            return HttpResponse::NotFound().body("Client not found");
        }
    }

    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(response)) => HttpResponse::Ok().body(response),
        Ok(Err(_)) => HttpResponse::InternalServerError().body("Failed to receive response from client"),
        Err(_) => HttpResponse::RequestTimeout().body("Request timed out"),
    }
}

#[get("/clients")]
async fn list_clients(clients: web::Data<ClientList>) -> impl Responder {
    let clients_guard = clients.lock().unwrap();
    let client_list: Vec<&Client> = clients_guard.values().collect();
    HttpResponse::Ok().json(client_list)
}

// async fn remove_inactive_clients(clients: web::Data<ClientList>) {
//     let mut clients_guard = clients.lock().unwrap();
//     let before_count = clients_guard.len();
//     clients_guard.retain(|key, client| {
//         let elapsed = client.last_heartbeat.elapsed();
//         let is_active = elapsed <= Duration::from_secs(60); // was 15 secs
//         log_with_timestamp(
//             &format!(
//                 "Checking client: {:?}, Last heartbeat: {:?} ago, Is active: {}",
//                 key, elapsed, is_active
//             ),
//             if is_active { "INFO" } else { "WARN" }
//         );
//         is_active
//     });
//     let after_count = clients_guard.len();
//     log_with_timestamp(&format!("Clients before cleanup: {}, after cleanup: {}", before_count, after_count), "INFO");
// }

async fn remove_inactive_clients(clients: web::Data<ClientList>) {
    let mut clients_guard = clients.lock().unwrap();
    let before_count = clients_guard.len();
    let mut inactive_count = 0;

    clients_guard.retain(|key, client| {
        let elapsed = client.last_heartbeat.elapsed();
        let is_active = elapsed <= Duration::from_secs(60); // 60 seconds timeout

        if !is_active {
            inactive_count += 1;
            log_with_timestamp(
                &format!(
                    "Inactive client removed: {:?}, Last heartbeat: {:?} ago",
                    key, elapsed
                ),
                "WARN"
            );
        }

        is_active
    });

    let after_count = clients_guard.len();
    
    if inactive_count > 0 {
        log_with_timestamp(
            &format!(
                "Cleaned up {} inactive clients. Clients before: {}, after: {}",
                inactive_count, before_count, after_count
            ),
            "INFO"
        );
    }
}

#[get("/about")]
async fn about_page() -> impl Responder {
    fs::NamedFile::open("./public_html/about.html").unwrap()
}

#[get("/api")]
async fn api_page() -> impl Responder {
    fs::NamedFile::open("./public_html/api.html").unwrap()
}

async fn not_found() -> Result<impl Responder> {
    Ok(fs::NamedFile::open("./public_html/404.html")?
        .customize()
        .with_status(actix_web::http::StatusCode::NOT_FOUND))
}

#[get("/qr")]
async fn get_qr(data: web::Data<AppState>, query: web::Query<HashMap<String, String>>) -> impl Responder {
    if let Some(qr_code) = query.get("code") {
        let db = data.db.lock().unwrap();
        let result: SqliteResult<String> = db.query_row(
            "SELECT data FROM machines WHERE qr_code = ?",
            params![qr_code],
            |row| row.get(0),
        );

        match result {
            Ok(json_str) => {
                if let Ok(json) = serde_json::from_str::<Value>(&json_str) {
                    HttpResponse::Ok().json(json)
                } else {
                    HttpResponse::InternalServerError().body("Failed to parse JSON data")
                }
            },
            Err(_) => HttpResponse::NotFound().body("QR code not found"),
        }
    } else {
        HttpResponse::BadRequest().body("Missing 'code' query parameter")
    }
}

async fn fetch_json(client: &HttpClient, url: &str) -> std::io::Result<Value> {
    //log_with_timestamp(&format!("Waiting 1 second before fetching data from: {}", url), "INFO");
    time::sleep(Duration::from_secs(1)).await; // 1 second delay before each request
    //log_with_timestamp(&format!("Now fetching data from: {}", url), "INFO");
    client.get(url).send().await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
        .json().await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
}

fn save_json(path: &Path, data: &Value) -> std::io::Result<()> {
    //log_with_timestamp(&format!("Saving data to: {:?}", path), "INFO");
    let json = serde_json::to_string_pretty(data)?;
    std_fs::write(path, json)
}

fn rotate_files(path: &Path) -> std::io::Result<()> {
    //log_with_timestamp(&format!("Rotating files for: {:?}", path), "INFO");
    let base_name = path.file_name().unwrap().to_str().unwrap();
    let parent = path.parent().unwrap();


    let mut highest = 0;
    for i in 1..=7 {
        if parent.join(format!("{}.old.{}", base_name, i)).exists() {
            highest = i;
        }
    }

    if highest < 7 {
        if path.exists() {
            let new_path = parent.join(format!("{}.old.{}", base_name, highest + 1));
            std_fs::rename(path, &new_path)?;
            //log_with_timestamp(&format!("Moved {:?} to {:?}", path, new_path), "INFO");
        }
    } else {
        // if we at 7, shift everything down and remove oldest
        let oldest = parent.join(format!("{}.old.7", base_name));
        std_fs::remove_file(&oldest)?;
        //log_with_timestamp(&format!("Removed oldest file: {:?}", oldest), "INFO");
        for i in (1..7).rev() {
            let old_path = parent.join(format!("{}.old.{}", base_name, i));
            let new_path = parent.join(format!("{}.old.{}", base_name, i + 1));
            if old_path.exists() {
                std_fs::rename(&old_path, &new_path)?;
                //log_with_timestamp(&format!("Moved {:?} to {:?}", old_path, new_path), "INFO");
            }
        }
        if path.exists() {
            let new_path = parent.join(format!("{}.old.1", base_name));
            std_fs::rename(path, &new_path)?;
            //log_with_timestamp(&format!("Moved {:?} to {:?}", path, new_path), "INFO");
        }
    }
    Ok(())
}

#[get("/location")]
async fn get_location(query: web::Query<LocationQuery>) -> Result<HttpResponse> {
    let file_path = PathBuf::from("location").join(format!("{}.json", query.location));
    
    match std_fs::read_to_string(file_path) {
        Ok(contents) => Ok(HttpResponse::Ok()
            .content_type("application/json")
            .body(contents)),
        Err(_) => Ok(HttpResponse::NotFound().body("Location not found")),
    }
}

#[get("/room")]
async fn get_room(query: web::Query<RoomQuery>) -> Result<HttpResponse> {
    let file_path = PathBuf::from("room")
        .join(&query.location)
        .join(format!("{}.json", query.room));
    
    match std_fs::read_to_string(file_path) {
        Ok(contents) => Ok(HttpResponse::Ok()
            .content_type("application/json")
            .body(contents)),
        Err(_) => Ok(HttpResponse::NotFound().body("Room not found")),
    }
}

#[get("/update_db")]
async fn manual_update_db(data: web::Data<AppState>) -> impl Responder {
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
    if data.db_update_in_progress.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        let db_update_in_progress = data.db_update_in_progress.clone();
        actix_web::rt::spawn(async move {
            let conn = data.db.lock().unwrap();
            if let Err(e) = update_db(&conn) {
                log_with_timestamp(&format!("Manual database update failed: {}", e), "ERROR");
            } else {
                log_with_timestamp("Manual database update completed successfully", "INFO");
            }
            db_update_in_progress.store(false, Ordering::SeqCst);
        });
        HttpResponse::Ok().json(json!({
            "status": "started",
            "message": "Database update started",
            "timestamp": now
        }))
    } else {
        HttpResponse::TooManyRequests().json(json!({
            "status": "in_progress",
            "message": "Database update already in progress",
            "timestamp": now
        }))
    }
}

#[get("/run_archiver")]
async fn manual_run_archiver(
    data: web::Data<AppState>,
    query: web::Query<std::collections::HashMap<String, String>>
) -> impl Responder {
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
    
    match query.get("location") {
        Some(location) => {
            if data.archiver_in_progress.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                let archiver_in_progress = data.archiver_in_progress.clone();
                let location = location.to_string();
                let location_clone = location.clone(); // Clone for use in the JSON response
                actix_web::rt::spawn(async move {
                    if let Err(e) = run_archiver(&location).await {
                        log_with_timestamp(&format!("Manual archiver run failed for location {}: {}", location, e), "ERROR");
                    } else {
                        log_with_timestamp(&format!("Manual archiver run completed successfully for location {}", location), "INFO");
                    }
                    archiver_in_progress.store(false, Ordering::SeqCst);
                });
                HttpResponse::Ok().json(json!({
                    "status": "started",
                    "message": format!("Archiver run started for location {}", location_clone),
                    "timestamp": now
                }))
            } else {
                HttpResponse::TooManyRequests().json(json!({
                    "status": "in_progress",
                    "message": "Archiver already running",
                    "timestamp": now
                }))
            }
        },
        None => {
            HttpResponse::BadRequest().json(json!({
                "status": "error",
                "message": "Missing required 'location' parameter",
                "timestamp": now
            }))
        }
    }
}

async fn run_archiver(location_id: &str) -> std::io::Result<()> {
    let client = HttpClient::new();
    let location_dir = Path::new("location");
    let room_dir = Path::new("room").join(location_id);

    std_fs::create_dir_all(&location_dir)?;
    std_fs::create_dir_all(&room_dir)?;

    log_with_timestamp(&format!("Starting API archiver for location ID: {}", location_id), "INFO");

    let location_url = format!("https://lessive.foxomy.com/api/v1/location/{}", location_id);
    match fetch_json(&client, &location_url).await {
        Ok(location) => {
            let location_path = location_dir.join(format!("{}.json", location_id));
            rotate_files(&location_path)?;
            save_json(&location_path, &location)?;

            if let Some(rooms) = location["rooms"].as_array() {
                for room in rooms {
                    if let (Some(room_id), Some(loc_id)) = (room["roomId"].as_str(), room["locationId"].as_str()) {
                        let room_url = format!("https://lessive.foxomy.com/api/v1/location/{}/room/{}/machines", loc_id, room_id);
                        match fetch_json(&client, &room_url).await {
                            Ok(machines) => {
                                let room_path = room_dir.join(format!("{}.json", room_id));
                                rotate_files(&room_path)?;
                                save_json(&room_path, &machines)?;
                            },
                            Err(e) => {
                                log_with_timestamp(&format!("Failed to fetch data for room {}: {}", room_id, e), "WARN");
                            }
                        }
                    }
                }
            }
            log_with_timestamp(&format!("Data archived for location {} at {}", location_id, Utc::now()), "INFO");
        },
        Err(e) => {
            log_with_timestamp(&format!("Failed to fetch location data for {}: {}", location_id, e), "ERROR");
        }
    }

    Ok(())
}

async fn ws_machine_status_logic(
    params: LaundryRequest,
    clients: web::Data<ClientList>,
) -> Result<String, actix_web::Error> {
    log_with_timestamp(
        &format!(
            "WebSocket GET request - Location: {}, Room: {}, Machine: {}",
            params.location, params.room, params.machine
        ),
        "INFO",
    );
    let key = (params.location.clone(), params.room.clone());
    let (tx, rx) = oneshot::channel();

    {
        let mut clients_guard = clients.lock().unwrap();
        if let Some(client) = clients_guard.get_mut(&key) {
            let mut sender = client.response_sender.lock().unwrap();
            *sender = Some(tx);

            let message = ServerMessage {
                action: "GET_MACHINE_STATUS".to_string(),
                payload: serde_json::json!({
                    "machine": params.machine,
                    "location": params.location,
                    "room": params.room,
                }),
            };
            
            if let Err(e) = client.session.text(serde_json::to_string(&message).unwrap()).await {
                log_with_timestamp(&format!("Failed to send GET_MACHINE_STATUS request to client: {:?}. Error: {}", key, e), "ERROR");
                return Err(actix_web::error::ErrorInternalServerError("Failed to send request to the client"));
            }
            log_with_timestamp(&format!("GET_MACHINE_STATUS request sent to client: {:?}", key), "INFO");
        } else {
            log_with_timestamp("Client not found", "WARN");
            return Err(actix_web::error::ErrorNotFound("Client not found"));
        }
    }

    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => Err(actix_web::error::ErrorInternalServerError("Failed to receive response from client")),
        Err(_) => Err(actix_web::error::ErrorRequestTimeout("Request timed out")),
    }
}

async fn ws_machine_status_handler(
    req: HttpRequest,
    body: web::Payload,
    clients: web::Data<ClientList>,
) -> Result<HttpResponse> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, body)?;

    let clients_clone = clients.clone();
    actix_web::rt::spawn(async move {
        let mut last_ping = Instant::now();

        while let Some(Ok(msg)) = msg_stream.next().await {
            match msg {
                WsMessage::Ping(bytes) => {
                    if session.pong(&bytes).await.is_err() {
                        break;
                    }
                }
                WsMessage::Text(text) => {
                    if let Ok(request) = serde_json::from_str::<LaundryRequest>(&text) {
                        match ws_machine_status_logic(request, clients_clone.clone()).await {
                            Ok(response) => {
                                if session.text(response).await.is_err() {
                                    log_with_timestamp("Failed to send response to client", "ERROR");
                                    break;
                                }
                            }
                            Err(e) => {
                                log_with_timestamp(&format!("Error in ws_machine_status_logic: {}", e), "ERROR");
                                if session.text(e.to_string()).await.is_err() {
                                    break;
                                }
                            }
                        }
                    } else {
                        log_with_timestamp(&format!("Received invalid message format: {}", text), "WARN");
                    }
                }
                WsMessage::Close(reason) => {
                    log_with_timestamp(&format!("Received Close message: {:?}", reason), "WARN");
                    break;
                }
                _ => {}
            }

            if last_ping.elapsed() > Duration::from_secs(20) {
                if session.ping(b"").await.is_err() {
                    break;
                }
                last_ping = Instant::now();
            }
        }

        let _ = session.close(None).await;
    });

    Ok(response)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let clients: ClientList = Arc::new(Mutex::new(HashMap::new()));
    let clients_data = web::Data::new(clients.clone());

    let clients_clone = clients_data.clone();
    actix_web::rt::spawn(async move {
        loop {
            actix_web::rt::time::sleep(Duration::from_secs(5)).await;
            remove_inactive_clients(clients_clone.clone()).await;
        }
    });

    let conn = Connection::open("machines.db").expect("Failed to open database");
    init_db(&conn).expect("Failed to initialize database");

    let app_state = web::Data::new(AppState {
        db: Mutex::new(conn),
        db_update_in_progress: Arc::new(AtomicBool::new(false)),
        archiver_in_progress: Arc::new(AtomicBool::new(false)),
    });

    // actix_web::rt::spawn(async {
    //     if let Err(e) = run_archiver().await {
    //         log_with_timestamp(&format!("Archiver error: {}", e), "ERROR");
    //     }
    // });

    log_with_timestamp("HTTP server starting on port 25652", "INFO");
    HttpServer::new(move || {
        App::new()
            .wrap(
                Cors::default()
                    .allow_any_origin()
                    .allow_any_method()
                    .allow_any_header()
                    .max_age(3600),
            )
            .app_data(clients_data.clone())
            .app_data(app_state.clone())
            .service(manual_update_db)
            .service(manual_run_archiver)
            .service(get_location)
            .service(get_room)
            .service(laundry_handler)
            .service(list_clients)
            .service(about_page)
            .service(api_page)
            .service(get_machines)
            .service(machine_status_handler)
            .service(machine_health_handler)
            .service(get_qr)
            .route("/iDQ0AdwiAq2Qh6BeiYJP", web::get().to(ws_handler))
            .route("/machinestatusws", web::get().to(ws_machine_status_handler))
            .service(
                fs::Files::new("/", "./public_html").show_files_listing().index_file("index.html"),
            )
            .default_service(web::route().to(not_found))
    })
    .bind("0.0.0.0:25652")?
    .run()
    .await
}
