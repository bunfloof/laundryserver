use actix_cors::Cors;
use actix_files as fs;
use actix_web::{get, post, web, App, HttpResponse, HttpServer, Responder, Result};
use chrono::Local;
use colored::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
struct Client {
    location: String,
    room: String,
    #[serde(skip_serializing)]
    stream: Arc<Mutex<Option<TcpStream>>>,
    #[serde(skip_serializing)]
    last_heartbeat: Instant,
}

type ClientList = Arc<Mutex<HashMap<(String, String), Client>>>;

#[derive(Deserialize)]
struct LaundryRequest {
    location: String,
    room: String,
    machine: String,
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

#[get("/about")]
async fn about_page() -> impl Responder {
    fs::NamedFile::open("./public_html/about.html").unwrap()
}

#[get("/api")]
async fn api_page() -> impl Responder {
    fs::NamedFile::open("./public_html/api.html").unwrap()
}

#[post("/")]
async fn laundry_handler(
    data: web::Json<LaundryRequest>, clients: web::Data<ClientList>,
) -> impl Responder {
    log_with_timestamp(
        &format!(
            "POST request - Location: {}, Room: {}, Machine: {}",
            data.location, data.room, data.machine
        ),
        "INFO",
    );
    let mut clients_guard = clients.lock().unwrap();
    let key = (data.location.clone(), data.room.clone());

    if let Some(client) = clients_guard.get_mut(&key) {
        let message = format!("Machine: {}", data.machine);
        let mut stream_guard = client.stream.lock().unwrap();
        if let Some(stream) = stream_guard.as_mut() {
            if let Err(e) = stream.write_all(message.as_bytes()) {
                log_with_timestamp(&format!("Failed to send message: {}", e), "ERROR");
                return HttpResponse::InternalServerError()
                    .body(format!("Failed to send message: {}", e));
            }
            client.last_heartbeat = Instant::now();
            HttpResponse::Ok().body("Message sent to the client")
        } else {
            log_with_timestamp("Client stream not available", "WARN");
            HttpResponse::InternalServerError().body("Client stream not available")
        }
    } else {
        log_with_timestamp("Client not found", "WARN");
        HttpResponse::NotFound().body("Client not found")
    }
}

#[get("/clients")]
async fn list_clients(clients: web::Data<ClientList>) -> impl Responder {
    let clients_guard = clients.lock().unwrap();
    let client_list: Vec<&Client> = clients_guard.values().collect();
    HttpResponse::Ok().json(client_list)
}

fn handle_client(mut stream: TcpStream, clients: ClientList) {
    let mut buffer = [0; 512];

    if let Ok(size) = stream.read(&mut buffer) {
        if size > 0 {
            let config: serde_json::Value =
                serde_json::from_slice(&buffer[..size]).unwrap_or_else(|_| {
                    log_with_timestamp(
                        "Failed to parse client config, ignoring this client.",
                        "ERROR",
                    );
                    return serde_json::json!({});
                });

            let location = config["location"].as_str().unwrap_or("").to_string();
            let room = config["room"].as_str().unwrap_or("").to_string();

            if location.is_empty() || room.is_empty() {
                log_with_timestamp("Invalid client configuration. Ignoring this client.", "ERROR");
                return;
            }

            log_with_timestamp(
                &format!("Client connected: Location: {}, Room: {}", location, room),
                "INFO",
            );

            let client = Client {
                location: location.clone(),
                room: room.clone(),
                stream: Arc::new(Mutex::new(Some(stream.try_clone().unwrap()))),
                last_heartbeat: Instant::now(),
            };

            let mut clients_guard = clients.lock().unwrap();
            if let Some(existing_client) = clients_guard.get_mut(&(location.clone(), room.clone()))
            {
                *existing_client.stream.lock().unwrap() = Some(stream.try_clone().unwrap());
                existing_client.last_heartbeat = Instant::now();
            } else {
                clients_guard.insert((location.clone(), room.clone()), client);
            }
            drop(clients_guard);

            let clients_clone = Arc::clone(&clients);
            thread::spawn(move || {
                handle_client_messages(stream, &location, &room, clients_clone);
            });
        }
    } else {
        log_with_timestamp("Error reading from stream, client ignored.", "ERROR");
    }
}

fn handle_client_messages(mut stream: TcpStream, location: &str, room: &str, clients: ClientList) {
    let mut buffer = [0; 512];
    loop {
        match stream.read(&mut buffer) {
            Ok(size) if size > 0 => {
                let message = String::from_utf8_lossy(&buffer[..size]);
                if message.trim() == "KEEP_ALIVE" {
                    if let Some(client) =
                        clients.lock().unwrap().get_mut(&(location.to_string(), room.to_string()))
                    {
                        client.last_heartbeat = Instant::now();
                    }
                }
            }
            Ok(_) | Err(_) => {
                log_with_timestamp(
                    &format!("Client disconnected: Location: {}, Room: {}", location, room),
                    "WARN",
                );
                let mut clients_guard = clients.lock().unwrap();
                if let Some(client) =
                    clients_guard.get_mut(&(location.to_string(), room.to_string()))
                {
                    *client.stream.lock().unwrap() = None;
                }
                break;
            }
        }
    }
}

fn remove_inactive_clients(clients: &ClientList) {
    let mut clients_guard = clients.lock().unwrap();
    clients_guard.retain(|_, client| {
        if client.last_heartbeat.elapsed() > Duration::from_secs(60) {
            log_with_timestamp(
                &format!(
                    "Removing inactive client: Location: {}, Room: {}",
                    client.location, client.room
                ),
                "WARN",
            );
            if let Some(stream) = client.stream.lock().unwrap().take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            false
        } else {
            true
        }
    });
}

async fn not_found() -> Result<impl Responder> {
    Ok(fs::NamedFile::open("./public_html/404.html")?
        .customize()
        .with_status(actix_web::http::StatusCode::NOT_FOUND))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let clients: ClientList = Arc::new(Mutex::new(HashMap::new()));

    let clients_clone = Arc::clone(&clients);
    thread::spawn(move || loop {
        match TcpListener::bind("0.0.0.0:25651") {
            Ok(listener) => {
                log_with_timestamp("TCP server listening on port 25651", "INFO");
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let clients_clone = Arc::clone(&clients_clone);
                            thread::spawn(move || handle_client(stream, clients_clone));
                        }
                        Err(e) => {
                            log_with_timestamp(
                                &format!(
                                    "Connection failed: {}. Continuing to accept new clients...",
                                    e
                                ),
                                "ERROR",
                            );
                        }
                    }
                }
            }
            Err(e) => {
                log_with_timestamp(
                    &format!("Failed to bind TCP listener: {}. Retrying in 5 seconds...", e),
                    "ERROR",
                );
                thread::sleep(Duration::from_secs(5));
            }
        }
    });

    let clients_clone = Arc::clone(&clients);
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(30));
        remove_inactive_clients(&clients_clone);
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
            .app_data(web::Data::new(Arc::clone(&clients)))
            .service(laundry_handler)
            .service(list_clients)
            .service(about_page)
            .service(api_page)
            .service(
                fs::Files::new("/", "./public_html").show_files_listing().index_file("index.html"),
            )
            .default_service(web::route().to(not_found))
    })
    .bind("0.0.0.0:25652")?
    .run()
    .await
}
