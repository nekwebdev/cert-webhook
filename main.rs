use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use kube::{
    api::{Api, Secret},
    Client,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use base64::{engine::general_purpose, Engine as _};
use log::{info, error};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use std::env;

#[derive(Debug, Serialize, Deserialize)]
struct HookRequest {
    namespace: String,
    secret_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiResponse {
    status: String,
    message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LinodeConfigsResponse {
    data: Vec<LinodeConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LinodeConfig {
    id: u32,
    port: u16,
}

struct AppState {
    kube_client: Client,
    linode_token: String,
    nodebalancer_id: String,
}

async fn health_check() -> impl Responder {
    HttpResponse::Ok().json(ApiResponse {
        status: "healthy".to_string(),
        message: None,
    })
}

async fn update_linode_cert(
    state: web::Data<Arc<AppState>>,
    webhook_data: web::Json<HookRequest>,
) -> impl Responder {
    let namespace = &webhook_data.namespace;
    let secret_name = &webhook_data.secret_name;
    
    info!("Processing certificate request for {}/{}", namespace, secret_name);
    
    // Get the certificate data from Kubernetes
    match get_secret_data(&state.kube_client, namespace, secret_name).await {
        Ok((cert, key)) => {
            // Update Linode NodeBalancer
            match update_linode_config(&state.linode_token, &state.nodebalancer_id, &cert, &key).await {
                Ok(_) => {
                    HttpResponse::Ok().json(ApiResponse {
                        status: "success".to_string(),
                        message: None,
                    })
                }
                Err(e) => {
                    error!("Failed to update NodeBalancer: {}", e);
                    HttpResponse::InternalServerError().json(ApiResponse {
                        status: "error".to_string(),
                        message: Some(format!("Failed to update NodeBalancer: {}", e)),
                    })
                }
            }
        }
        Err(e) => {
            error!("Failed to retrieve certificate data: {}", e);
            HttpResponse::InternalServerError().json(ApiResponse {
                status: "error".to_string(),
                message: Some(format!("Failed to retrieve certificate data: {}", e)),
            })
        }
    }
}

async fn get_secret_data(
    client: &Client,
    namespace: &str,
    name: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let secret = secrets.get(name).await?;
    
    let cert_data = secret.data.as_ref()
        .and_then(|data| data.get("tls.crt"))
        .ok_or("tls.crt not found in secret")?;
    
    let key_data = secret.data.as_ref()
        .and_then(|data| data.get("tls.key"))
        .ok_or("tls.key not found in secret")?;
    
    let cert = String::from_utf8(general_purpose::STANDARD.decode(&cert_data.0)?)?;
    let key = String::from_utf8(general_purpose::STANDARD.decode(&key_data.0)?)?;
    
    Ok((cert, key))
}

async fn update_linode_config(
    token: &str,
    nodebalancer_id: &str,
    cert: &str,
    key: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", token))?);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    
    // Get existing configs
    let url = format!("https://api.linode.com/v4/nodebalancers/{}/configs", nodebalancer_id);
    let response = client.get(&url)
        .headers(headers.clone())
        .send()
        .await?
        .error_for_status()?;
    
    let configs: LinodeConfigsResponse = response.json().await?;
    
    // Find HTTPS config (port 443)
    let https_config = configs.data.iter().find(|c| c.port == 443);
    
    if let Some(config) = https_config {
        // Update existing config
        info!("Updating existing HTTPS config (ID: {})", config.id);
        
        let update_url = format!("{}/{}", url, config.id);
        let payload = serde_json::json!({
            "ssl_cert": cert,
            "ssl_key": key
        });
        
        client.put(&update_url)
            .headers(headers)
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
        
        info!("Successfully updated certificate in NodeBalancer config");
    } else {
        // Create new HTTPS config
        info!("No HTTPS config found, creating new one");
        
        let payload = serde_json::json!({
            "port": 443,
            "protocol": "https",
            "algorithm": "roundrobin",
            "ssl_cert": cert,
            "ssl_key": key,
            "stickiness": "none",
            "check": "http_body",
            "check_path": "/",
            "check_body": "",
            "check_interval": 30,
            "check_timeout": 5,
            "check_attempts": 3,
            "cipher_suite": "recommended"
        });
        
        client.post(&url)
            .headers(headers)
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
        
        info!("Successfully created new HTTPS NodeBalancer config");
    }
    
    Ok(())
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
    
    // Get configuration from environment
    let linode_token = env::var("LINODE_TOKEN")
        .expect("LINODE_TOKEN must be set");
    let nodebalancer_id = env::var("NODEBALANCER_ID")
        .expect("NODEBALANCER_ID must be set");
    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let port = port.parse::<u16>().expect("PORT must be a number");
    
    // Initialize Kubernetes client
    let kube_client = Client::try_default()
        .await
        .expect("Failed to create Kubernetes client");
    
    let state = Arc::new(AppState {
        kube_client,
        linode_token,
        nodebalancer_id,
    });
    
    info!("Starting webhook server on port {}", port);
    
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .route("/health", web::get().to(health_check))
            .route("/update-linode-cert", web::post().to(update_linode_cert))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}