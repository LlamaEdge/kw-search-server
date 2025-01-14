use axum::{
    extract::Multipart,
    routing::{get, post},
    Json, Router,
};
use clap::{ArgGroup, Parser};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf};
use tracing::{error, info, warn, Level};

// default port of Keyword Search Server
const DEFAULT_PORT: &str = "9069";

/// Command line arguments configuration
#[derive(Debug, Parser)]
#[command(name = "Keyword Search Server", version = env!("CARGO_PKG_VERSION"), author = env!("CARGO_PKG_AUTHORS"), about = "Keyword Search Server")]
#[command(group = ArgGroup::new("socket_address_group").multiple(false).args(&["socket_addr", "port"]))]
struct Args {
    /// Socket address of llama-proxy-server instance. For example, `0.0.0.0:9069`.
    #[arg(long, default_value = None, value_parser = clap::value_parser!(SocketAddr), group = "socket_address_group")]
    socket_addr: Option<SocketAddr>,
    /// Socket address of llama-proxy-server instance
    #[arg(long, default_value = DEFAULT_PORT, value_parser = clap::value_parser!(u16), group = "socket_address_group")]
    port: u16,
}

#[tokio::main]
async fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_max_level(Level::INFO)
        .init();

    // Parse command line arguments
    let args = Args::parse();
    info!("Server starting, command line arguments parsed");

    // Build application routes
    let app = Router::new()
        .route("/", get(hello_world))
        .route("/v1/index", post(index_document));
    info!("Route configuration completed");

    // Run the server
    let addr = format!("127.0.0.1:{}", args.port);
    info!("Binding to address: {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("Server running at http://{}", addr);

    info!("Starting to accept connections...");
    axum::serve(listener, app).await.unwrap();
}

// Handler for root path
async fn hello_world() -> &'static str {
    info!("Received health check request");
    "Hello, World!"
}

// Document processing result
#[derive(Debug, Serialize)]
struct DocumentResult {
    filename: String,
    status: String,
    error: Option<String>,
}

// Index response
#[derive(Debug, Serialize)]
struct IndexResponse {
    results: Vec<DocumentResult>,
}

// Handler for document indexing requests
async fn index_document(mut multipart: Multipart) -> Json<IndexResponse> {
    info!("Received new document indexing request");
    let mut results = Vec::new();

    while let Ok(Some(field)) = multipart.next_field().await {
        let filename = field
            .file_name()
            .map(ToString::to_string)
            .unwrap_or_else(|| "unknown".to_string());

        let content_type = field
            .content_type()
            .map(|ct| ct.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        info!("Processing file: {}, type: {}", filename, content_type);

        // Validate file type
        if !is_valid_content_type(&content_type) {
            warn!(
                "Unsupported file type: {}, file: {}",
                content_type, filename
            );
            results.push(DocumentResult {
                filename,
                status: "failed".to_string(),
                error: Some(
                    "Unsupported file type. Only .txt and .md files are allowed".to_string(),
                ),
            });
            continue;
        }

        // Read file content
        match field.bytes().await {
            Ok(bytes) => {
                info!(
                    "Successfully read file: {}, size: {} bytes",
                    filename,
                    bytes.len()
                );
                match String::from_utf8(bytes.to_vec()) {
                    Ok(content) => {
                        // Add actual document processing logic here
                        info!("Successfully parsed file content: {}", filename);
                        results.push(DocumentResult {
                            filename,
                            status: "indexed".to_string(),
                            error: None,
                        });
                    }
                    Err(e) => {
                        error!("File content encoding error: {}, error: {}", filename, e);
                        results.push(DocumentResult {
                            filename,
                            status: "failed".to_string(),
                            error: Some("Invalid UTF-8 content".to_string()),
                        });
                    }
                }
            }
            Err(e) => {
                error!("Failed to read file: {}, error: {}", filename, e);
                results.push(DocumentResult {
                    filename,
                    status: "failed".to_string(),
                    error: Some(format!("Failed to read file: {}", e)),
                });
            }
        }
    }

    info!(
        "Document indexing request completed, processed {} files",
        results.len()
    );
    Json(IndexResponse { results })
}

// Validate content type
fn is_valid_content_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "text/plain" | "text/markdown" | "application/octet-stream" // Sometimes file uploads might not have the correct content-type
    )
}
