use actix_web::{web, App, HttpResponse, HttpServer, Responder, middleware, Error};
use kube::{
    api::Api,
    Client,
};
use k8s_openapi::api::core::v1::Secret;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use base64::{engine::general_purpose, Engine as _};
use log::{info, error, debug, warn};
use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    ClientBuilder,
};
use std::env;
use std::time::Duration;
use tokio::time::sleep;
use actix_web::middleware::Logger;
use actix_web_prom::PrometheusMetricsBuilder;

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

#[derive(Debug, Serialize, Deserialize)]
struct CertManagerHook {
    #[serde(rename = "secretRef")]
    secret_ref: SecretRef,
    // Add other fields as needed
}

#[derive(Debug, Serialize, Deserialize)]
struct SecretRef {
    name: String,
    namespace: String,
}

struct AppState {
    kube_client: Client,
    http_client: reqwest::Client,
    linode_token: String,
    nodebalancer_id: String,
    https_config_id: String,
}

const MAX_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 500;

async fn health_check() -> impl Responder {
    HttpResponse::Ok().json(ApiResponse {
        status: "healthy".to_string(),
        message: None,
    })
}

async fn deep_health_check(state: web::Data<Arc<AppState>>) -> impl Responder {
    // Check if we can connect to Kubernetes
    match state.kube_client.apiserver_version().await {
        Ok(_) => {
            // Check if we can connect to Linode API
            let url = format!("https://api.linode.com/v4/nodebalancers/{}", state.nodebalancer_id);
            match state.http_client.get(&url)
                .header(AUTHORIZATION, format!("Bearer {}", state.linode_token))
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_success() {
                        HttpResponse::Ok().json(ApiResponse {
                            status: "healthy".to_string(),
                            message: None,
                        })
                    } else {
                        warn!("Linode API responded with status: {}", response.status());
                        HttpResponse::ServiceUnavailable().json(ApiResponse {
                            status: "degraded".to_string(),
                            message: Some(format!("Linode API responded with status: {}", response.status())),
                        })
                    }
                },
                Err(e) => {
                    error!("Failed to connect to Linode API: {}", e);
                    HttpResponse::ServiceUnavailable().json(ApiResponse {
                        status: "degraded".to_string(),
                        message: Some(format!("Failed to connect to Linode API: {}", e)),
                    })
                }
            }
        },
        Err(e) => {
            error!("Failed to connect to Kubernetes API: {}", e);
            HttpResponse::ServiceUnavailable().json(ApiResponse {
                status: "degraded".to_string(),
                message: Some(format!("Failed to connect to Kubernetes API: {}", e)),
            })
        }
    }
}

async fn validate_hook_request(req: &HookRequest) -> Result<(), String> {
    if req.namespace.is_empty() {
        return Err("namespace cannot be empty".to_string());
    }
    if req.secret_name.is_empty() {
        return Err("secret_name cannot be empty".to_string());
    }
    // Validate that namespace and secret_name don't contain invalid characters
    if !req.namespace.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return Err("namespace contains invalid characters".to_string());
    }
    if !req.secret_name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '.') {
        return Err("secret_name contains invalid characters".to_string());
    }
    Ok(())
}

async fn update_nodebalancer_cert(
    state: web::Data<Arc<AppState>>,
    webhook_data: web::Json<CertManagerHook>,
) -> Result<HttpResponse, Error> {
    // Convert cert-manager format to our internal format
    let request = HookRequest {
        namespace: webhook_data.secret_ref.namespace.clone(),
        secret_name: webhook_data.secret_ref.name.clone(),
    };
    
    info!("Processing certificate request for {}/{}", request.namespace, request.secret_name);
    
    // Validate request
    if let Err(e) = validate_hook_request(&request).await {
        error!("Validation error: {}", e);
        return Ok(HttpResponse::BadRequest().json(ApiResponse {
            status: "error".to_string(),
            message: Some(format!("Invalid request: {}", e)),
        }));
    }
    
    // Get the certificate data from Kubernetes with retries
    let cert_result = retry_operation(|| async {
        get_secret_data(&state.kube_client, &request.namespace, &request.secret_name).await
    }).await;
    
    match cert_result {
        Ok((cert, key)) => {
            // Update Linode NodeBalancer with retries
            let update_result = retry_operation(|| async {
                update_linode_config(
                    &state.http_client,
                    &state.linode_token, 
                    &state.nodebalancer_id,
                    &state.https_config_id,
                    &cert, 
                    &key
                ).await
            }).await;
            
            match update_result {
                Ok(_) => {
                    info!("Successfully updated certificate for {}/{}", request.namespace, request.secret_name);
                    Ok(HttpResponse::Ok().json(ApiResponse {
                        status: "success".to_string(),
                        message: None,
                    }))
                }
                Err(e) => {
                    error!("Failed to update NodeBalancer after retries: {}", e);
                    Ok(HttpResponse::InternalServerError().json(ApiResponse {
                        status: "error".to_string(),
                        message: Some(format!("Failed to update NodeBalancer: {}", e)),
                    }))
                }
            }
        }
        Err(e) => {
            error!("Failed to retrieve certificate data after retries: {}", e);
            Ok(HttpResponse::InternalServerError().json(ApiResponse {
                status: "error".to_string(),
                message: Some(format!("Failed to retrieve certificate data: {}", e)),
            }))
        }
    }
}

async fn retry_operation<F, Fut, T>(operation: F) -> Result<T, Box<dyn std::error::Error>>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, Box<dyn std::error::Error>>>,
{
    let mut last_error = None;
    
    for attempt in 1..=MAX_RETRIES {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                warn!("Operation failed (attempt {}/{}): {}", attempt, MAX_RETRIES, e);
                last_error = Some(e);
                
                if attempt < MAX_RETRIES {
                    let backoff = RETRY_DELAY_MS * 2u64.pow(attempt - 1);
                    debug!("Retrying after {}ms", backoff);
                    sleep(Duration::from_millis(backoff)).await;
                }
            }
        }
    }
    
    Err(last_error.unwrap_or_else(|| 
        Box::new(std::io::Error::new(std::io::ErrorKind::Other, "Unknown error during retry"))
    ))
}

async fn get_secret_data(
    client: &Client,
    namespace: &str,
    name: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    debug!("Retrieving secret {}/{} from Kubernetes", namespace, name);
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
    client: &reqwest::Client,
    token: &str,
    nodebalancer_id: &str,
    https_config_id: &str,
    cert: &str,
    key: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", token))?);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    
    // Update the existing HTTPS config using the provided ID
    info!("Updating HTTPS config (ID: {})", https_config_id);
    
    let update_url = format!("https://api.linode.com/v4/nodebalancers/{}/configs/{}", 
                             nodebalancer_id, https_config_id);
    
    let payload = serde_json::json!({
        "protocol": "https",
        "ssl_cert": cert,
        "ssl_key": key
    });
    
    let response = client.put(&update_url)
        .headers(headers)
        .json(&payload)
        .send()
        .await?;
    
    if !response.status().is_success() {
        let error_text = response.text().await?;
        error!("Failed to update Linode config: {}", error_text);
        return Err(format!("Failed to update config: {}", error_text).into());
    }
    
    info!("Successfully updated certificate in NodeBalancer config");
    Ok(())
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Initialize logging with more verbose format
    env_logger::Builder::from_env(env_logger::Env::new().default_filter_or("info"))
        .format_timestamp_millis()
        .format_module_path(true)
        .init();
    
    // Get configuration from environment
    let linode_token = env::var("LINODE_TOKEN")
        .expect("LINODE_TOKEN must be set");
    let nodebalancer_id = env::var("NODEBALANCER_ID")
        .expect("NODEBALANCER_ID must be set");
    let https_config_id = env::var("HTTPS_CONFIG_ID")
        .expect("HTTPS_CONFIG_ID must be set");
    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let port = port.parse::<u16>().expect("PORT must be a number");
    
    // Initialize Kubernetes client
    let kube_client = Client::try_default()
        .await
        .expect("Failed to create Kubernetes client");
    
    // Initialize HTTP client with timeouts and connection pooling
    let http_client = ClientBuilder::new()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(10)
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
        .expect("Failed to build HTTP client");
    
    let state = Arc::new(AppState {
        kube_client,
        http_client,
        linode_token,
        nodebalancer_id,
        https_config_id,
    });
    
    // Set up Prometheus metrics
    let prometheus = PrometheusMetricsBuilder::new("cert_webhook")
        .endpoint("/metrics")
        .build()
        .unwrap();
    
    info!("Starting webhook server on port {}", port);
    
    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .wrap(middleware::Compress::default())
            .wrap(prometheus.clone())
            .app_data(web::Data::new(state.clone()))
            .app_data(web::JsonConfig::default()
                .limit(256 * 1024)  // 256k payload limit
                .error_handler(|err, _| {
                    error!("JSON payload error: {}", err);
                    actix_web::error::InternalError::from_response(
                        err, 
                        HttpResponse::BadRequest().json(ApiResponse {
                            status: "error".to_string(),
                            message: Some("Invalid JSON payload".to_string()),
                        })
                    ).into()
                }))
            .route("/health", web::get().to(health_check))
            .route("/health/deep", web::get().to(deep_health_check))
            .route("/metrics", web::get().to(|| async { HttpResponse::Ok().body("") }))
            .route("/update-nodebalancer-cert", web::post().to(update_nodebalancer_cert))
    })
    .keep_alive(Duration::from_secs(75))  // Keep-alive timeout
    .workers(num_cpus::get())  // Use number of CPU cores for worker threads
    .shutdown_timeout(30)  // Allow 30 seconds for graceful shutdown
    .bind(("0.0.0.0", port))?
    .run()
    .await
}