use actix_cors::Cors;
use actix_files as fs;
use actix_web::{get, post, web, App, HttpRequest, HttpResponse, HttpServer, Responder, Result};
use actix_ws::{Message as WsMessage, Session};
use chrono::Local;
use colored::*;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

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
            .service(laundry_handler)
            .service(list_clients)
            .service(about_page)
            .service(api_page)
            .service(get_machines)
            .service(machine_status_handler)
            .service(machine_health_handler)
            .route("/iDQ0AdwiAq2Qh6BeiYJP", web::get().to(ws_handler)) // will serve as password
            .service(
                fs::Files::new("/", "./public_html").show_files_listing().index_file("index.html"),
            )
            .default_service(web::route().to(not_found))
    })
    .bind("0.0.0.0:25652")?
    .run()
    .await
}
