mod error;

use axum::extract::Path;
use axum::response::IntoResponse;
use axum::{
    extract::{FromRequest, Multipart},
    routing::{get, post},
    Json, Router,
};
use clap::{ArgGroup, Parser};
use endpoints::keyword_search::{DocumentInput, DocumentResult, IndexRequest, IndexResponse};
use error::ServerError;
use http::status::StatusCode;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    net::{IpAddr, SocketAddr},
};
use tantivy::{collector::TopDocs, doc, query::QueryParser, schema::*, Index, ReloadPolicy};
use tracing::{debug, error, info, warn, Level};
use url::Url;

// default port of Keyword Search Server
const DEFAULT_PORT: &str = "9069";

const MEMORY_BUDGET_IN_BYTES: usize = 100_000_000;

const INDEX_STORAGE_DIR: &str = "index_storage";

// socket address
pub(crate) static DOWNLOAD_URL_PREFIX: OnceCell<Url> = OnceCell::new();

/// Command line arguments configuration
#[derive(Debug, Parser)]
#[command(name = "Keyword Search Server", version = env!("CARGO_PKG_VERSION"), author = env!("CARGO_PKG_AUTHORS"), about = "Keyword Search Server")]
#[command(group = ArgGroup::new("socket_address_group").multiple(false).args(&["socket_addr", "port"]))]
struct Cli {
    /// Download URL prefix, format: `http(s)://{IPv4_address}:{port}` or `http(s)://{domain}:{port}`
    #[arg(long)]
    download_url_prefix: Option<String>,
    /// Socket address of llama-proxy-server instance. For example, `0.0.0.0:9069`.
    #[arg(long, default_value = None, value_parser = clap::value_parser!(SocketAddr), group = "socket_address_group")]
    socket_addr: Option<SocketAddr>,
    /// Socket address of llama-proxy-server instance
    #[arg(long, default_value = DEFAULT_PORT, value_parser = clap::value_parser!(u16), group = "socket_address_group")]
    port: u16,
}

// Add these new structs for query handling
#[derive(Debug, Clone, Deserialize)]
struct QueryRequest {
    query: String,
    #[serde(default = "default_top_k")]
    top_k: usize,
    index: String,
}

fn default_top_k() -> usize {
    5
}

#[derive(Debug, Clone, Serialize)]
struct QueryResponse {
    hits: Vec<SearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SearchHit {
    title: String,
    content: String,
    score: f32,
}

#[tokio::main]
async fn main() -> Result<(), ServerError> {
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
    let cli = Cli::parse();
    info!("Server starting, command line arguments parsed");

    // Build application routes
    let app = Router::new()
        .route("/v1/index", post(index_document_handler))
        .route("/v1/search", post(query_handler))
        .route(
            "/v1/files/download/{index_name}",
            get(download_index_file_handler),
        );
    info!("Route configuration completed");

    // Run the server
    let addr = match cli.socket_addr {
        Some(addr) => addr,
        None => SocketAddr::from(([0, 0, 0, 0], cli.port)),
    };
    info!("Binding to address: {}", addr);

    // set DOWNLOAD_URL_PREFIX
    match cli.download_url_prefix {
        Some(download_url_prefix) => {
            info!(target: "stdout", "download_url_prefix: {}", &download_url_prefix);

            // download url prefix
            info!(target: "stdout", "download_url_prefix: {}", &download_url_prefix);
            let download_url_prefix = Url::parse(&download_url_prefix).map_err(|e| {
                ServerError::Operation(format!(
                    "Failed to parse `download_url_prefix` CLI option: {}",
                    e
                ))
            })?;
            if let Err(e) = DOWNLOAD_URL_PREFIX.set(download_url_prefix) {
                let err_msg = format!("Failed to set DOWNLOAD_URL_PREFIX: {}", e);

                error!(target: "stdout", "{}", &err_msg);

                return Err(ServerError::Operation(err_msg));
            }
        }
        None => {
            match addr.ip() {
                IpAddr::V4(ip) => match ip.to_string().as_str() {
                    "0.0.0.0" => {
                        let ipv4_addr_str = format!("http://localhost:{}", addr.port());

                        info!(target: "stdout", "download_url_prefix: {}", ipv4_addr_str);

                        let download_url_prefix = Url::parse(&ipv4_addr_str).map_err(|e| {
                            ServerError::Operation(format!(
                                "Failed to parse `download_url_prefix` CLI option: {}",
                                e
                            ))
                        })?;
                        if let Err(e) = DOWNLOAD_URL_PREFIX.set(download_url_prefix) {
                            let err_msg = format!("Failed to set SOCKET_ADDRESS: {}", e);

                            error!(target: "stdout", "{}", &err_msg);

                            return Err(ServerError::Operation(err_msg));
                        }
                    }
                    _ => {
                        let ipv4_addr_str = format!("http://{}:{}", addr.ip(), addr.port());

                        info!(target: "stdout", "download_url_prefix: {}", ipv4_addr_str);

                        let download_url_prefix = Url::parse(&ipv4_addr_str).map_err(|e| {
                            ServerError::Operation(format!(
                                "Failed to parse `download_url_prefix` CLI option: {}",
                                e
                            ))
                        })?;
                        if let Err(e) = DOWNLOAD_URL_PREFIX.set(download_url_prefix) {
                            let err_msg = format!("Failed to set SOCKET_ADDRESS: {}", e);

                            error!(target: "stdout", "{}", &err_msg);

                            return Err(ServerError::Operation(err_msg));
                        }
                    }
                },
                IpAddr::V6(_) => {
                    let err_msg = "ipv6 is not supported";

                    // log error
                    error!(target: "stdout", "{}", err_msg);

                    // return error
                    return Err(ServerError::Operation(err_msg.into()));
                }
            }
        }
    }

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("Server running at http://{}", addr);

    info!("Starting to accept connections...");
    match axum::serve(listener, app).await {
        Ok(_) => Ok(()),
        Err(e) => Err(ServerError::Operation(e.to_string())),
    }
}

// Main handler that routes to appropriate processing function based on content type
async fn index_document_handler(
    content_type: axum::http::header::HeaderMap,
    request: axum::extract::Request,
) -> Json<IndexResponse> {
    let content_type = content_type
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    info!("Received document indexing request");

    let response = match content_type {
        t if t.starts_with("multipart/form-data") => {
            info!("Processing as multipart/form-data");
            let multipart = match Multipart::from_request(request, &()).await {
                Ok(m) => m,
                Err(e) => {
                    error!(error = %e, "Failed to parse multipart request");
                    return Json(IndexResponse {
                        results: vec![DocumentResult {
                            filename: "unknown".to_string(),
                            status: "failed".to_string(),
                            error: Some("Failed to parse multipart request".to_string()),
                        }],
                        index_name: None,
                        download_url: None,
                    });
                }
            };
            process_multipart(multipart).await
        }
        "application/json" => {
            info!("Processing as JSON request");
            let payload = match axum::Json::<IndexRequest>::from_request(request, &()).await {
                Ok(Json(payload)) => payload,
                Err(e) => {
                    error!(error = %e, "Failed to parse JSON request");
                    return Json(IndexResponse {
                        results: vec![DocumentResult {
                            filename: "unknown".to_string(),
                            status: "failed".to_string(),
                            error: Some("Failed to parse JSON request".to_string()),
                        }],
                        index_name: None,
                        download_url: None,
                    });
                }
            };
            process_json(payload).await
        }
        _ => {
            warn!(content_type = content_type, "Unsupported content type");
            Json(IndexResponse {
                results: vec![DocumentResult {
                    filename: "unknown".to_string(),
                    status: "failed".to_string(),
                    error: Some("Unsupported content type".to_string()),
                }],
                index_name: None,
                download_url: None,
            })
        }
    };

    info!(
        successful = response
            .results
            .iter()
            .filter(|r| r.status == "indexed")
            .count(),
        failed = response
            .results
            .iter()
            .filter(|r| r.status == "failed")
            .count(),
        "Request processing completed"
    );

    response
}

// Process multipart form data
async fn process_multipart(mut multipart: Multipart) -> Json<IndexResponse> {
    info!("Starting multipart form data processing");
    let mut results = Vec::new();
    let mut field_count = 0;
    let mut documents = Vec::new();

    while let Ok(Some(field)) = multipart.next_field().await {
        field_count += 1;
        let filename = field
            .file_name()
            .map(ToString::to_string)
            .unwrap_or_else(|| "unknown".to_string());

        let content_type = field
            .content_type()
            .map(|ct| ct.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        info!(
            field_number = field_count,
            filename = %filename,
            content_type = %content_type,
            "Processing field"
        );

        if !is_valid_content_type(&content_type) {
            warn!(
                field_number = field_count,
                filename = %filename,
                content_type = %content_type,
                "Unsupported file type"
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

        process_field_content(&mut results, &mut documents, field, filename).await;
    }

    info!(
        total_fields = field_count,
        successful = results.iter().filter(|r| r.status == "indexed").count(),
        failed = results.iter().filter(|r| r.status == "failed").count(),
        "Field processing completed"
    );

    // Create index directory
    info!("Starting index creation");
    let index_storage_dir = std::env::current_dir().unwrap().join(INDEX_STORAGE_DIR);
    let index_name = format!("index-{}", uuid::Uuid::new_v4());
    let index_path = index_storage_dir.as_path().join(&index_name);
    if !index_path.exists() {
        debug!(path = %index_path.display(), "Creating index directory");
        std::fs::create_dir_all(&index_path).unwrap();
    }

    // Define schema
    info!("Defining index schema");
    let mut schema_builder = Schema::builder();
    let title = schema_builder.add_text_field("title", TEXT | STORED);
    let body = schema_builder.add_text_field("body", TEXT | STORED);
    let schema = schema_builder.build();

    // Create index
    info!("Creating new index");
    let index = match Index::create_in_dir(&index_path, schema.clone()) {
        Ok(index) => index,
        Err(e) => {
            error!(error = %e, "Failed to create index");
            return Json(IndexResponse {
                results,
                index_name: None,
                download_url: None,
            });
        }
    };

    // Create index writer
    info!("Initializing index writer");
    let mut index_writer = match index.writer(MEMORY_BUDGET_IN_BYTES) {
        Ok(writer) => writer,
        Err(e) => {
            error!(error = %e, "Failed to create index writer");
            return Json(IndexResponse {
                results,
                index_name: None,
                download_url: None,
            });
        }
    };

    // Add documents to index
    info!(
        document_count = documents.len(),
        "Starting document indexing"
    );
    for (i, document) in documents.iter().enumerate() {
        let doc = doc!(
            title => document.title.clone().unwrap_or("Unknown".to_string()),
            body => document.content.clone(),
        );
        if let Err(e) = index_writer.add_document(doc) {
            error!(
                document_number = i + 1,
                error = %e,
                "Failed to add document to index"
            );
            continue;
        }
        info!(
            document_number = i + 1,
            total = documents.len(),
            "Document added to index"
        );
    }

    // Commit index
    info!("Committing index");
    if let Err(e) = index_writer.commit() {
        error!(error = %e, "Failed to commit index");
        return Json(IndexResponse {
            results,
            index_name: None,
            download_url: None,
        });
    }

    // generate download url for index file
    let url = {
        // get the socket address of request
        let download_url_prefix = DOWNLOAD_URL_PREFIX.get().unwrap();

        let host = match download_url_prefix.port() {
            Some(port) => {
                format!("{}:{}", download_url_prefix.host_str().unwrap(), port)
            }
            None => download_url_prefix.host_str().unwrap().to_string(),
        };

        format!(
            "{}://{}/v1/files/download/{}",
            download_url_prefix.scheme(),
            host,
            &index_name,
        )
    };
    info!(url = %url, "Download URL generated");

    Json(IndexResponse {
        results,
        index_name: Some(index_name),
        download_url: Some(url),
    })
}

// Helper function to process field content
async fn process_field_content(
    results: &mut Vec<DocumentResult>,
    documents: &mut Vec<DocumentInput>,
    field: axum::extract::multipart::Field<'_>,
    filename: String,
) {
    match field.bytes().await {
        Ok(bytes) => {
            info!(size_bytes = bytes.len(), "Content read successfully");
            match String::from_utf8(bytes.to_vec()) {
                Ok(content) => {
                    let document = DocumentInput {
                        content: content.clone(),
                        title: None,
                    };
                    documents.push(document);

                    match process_content(&content) {
                        Ok(_) => {
                            info!("Content processed successfully");
                            results.push(DocumentResult {
                                filename,
                                status: "indexed".to_string(),
                                error: None,
                            });
                        }
                        Err(e) => {
                            error!(
                                filename = %filename,
                                error = %e,
                                "Content processing failed"
                            );
                            results.push(DocumentResult {
                                filename,
                                status: "failed".to_string(),
                                error: Some(e.to_string()),
                            });
                        }
                    }
                }
                Err(e) => {
                    error!(
                        filename = %filename,
                        error = %e,
                        "UTF-8 decoding failed"
                    );
                    results.push(DocumentResult {
                        filename,
                        status: "failed".to_string(),
                        error: Some("Invalid UTF-8 content".to_string()),
                    });
                }
            }
        }
        Err(e) => {
            error!(
                filename = %filename,
                error = %e,
                "Failed to read field content"
            );
            results.push(DocumentResult {
                filename,
                status: "failed".to_string(),
                error: Some(format!("Failed to read file: {}", e)),
            });
        }
    }
}

// Process JSON input
async fn process_json(request: IndexRequest) -> Json<IndexResponse> {
    info!(
        document_count = request.documents.len(),
        "Starting JSON request processing"
    );
    let mut results = Vec::new();

    // Create index directory
    info!("Starting index creation");
    let index_storage_dir = std::env::current_dir().unwrap().join(INDEX_STORAGE_DIR);
    let index_name = format!("index-{}", uuid::Uuid::new_v4());
    let index_path = index_storage_dir.as_path().join(&index_name);
    if !index_path.exists() {
        debug!(path = %index_path.display(), "Creating index directory");
        std::fs::create_dir_all(&index_path).unwrap();
    }

    // Define schema
    info!("Defining index schema");
    let mut schema_builder = Schema::builder();
    let title = schema_builder.add_text_field("title", TEXT | STORED);
    let body = schema_builder.add_text_field("body", TEXT | STORED);
    let schema = schema_builder.build();

    // Create index
    info!("Creating new index");
    let index = match Index::create_in_dir(&index_path, schema.clone()) {
        Ok(index) => index,
        Err(e) => {
            error!(error = %e, "Failed to create index");
            return Json(IndexResponse {
                results,
                index_name: None,
                download_url: None,
            });
        }
    };

    // Create index writer
    info!("Initializing index writer");
    let mut index_writer = match index.writer(MEMORY_BUDGET_IN_BYTES) {
        Ok(writer) => writer,
        Err(e) => {
            error!(error = %e, "Failed to create index writer");
            return Json(IndexResponse {
                results,
                index_name: None,
                download_url: None,
            });
        }
    };

    // Process and index documents
    let total = request.documents.len();
    for (index, document) in request.documents.into_iter().enumerate() {
        let filename = document
            .title
            .clone()
            .unwrap_or_else(|| "Unknown".to_string());
        info!(
            document_number = index + 1,
            total = total,
            filename = %filename,
            content_length = document.content.len(),
            "Processing document"
        );

        // Add document to index
        let doc = doc!(
            title => document.title.clone().unwrap_or("Unknown".to_string()),
            body => document.content.clone(),
        );

        if let Err(e) = index_writer.add_document(doc) {
            error!(
                document_number = index + 1,
                filename = %filename,
                error = %e,
                "Failed to add document to index"
            );
            results.push(DocumentResult {
                filename,
                status: "failed".to_string(),
                error: Some(format!("Failed to add to index: {}", e)),
            });
            continue;
        }

        // Process content
        match process_content(&document.content) {
            Ok(_) => {
                info!("Document processed successfully");
                results.push(DocumentResult {
                    filename,
                    status: "indexed".to_string(),
                    error: None,
                });
            }
            Err(e) => {
                error!(
                    document_number = index + 1,
                    filename = %filename,
                    error = %e,
                    "Document processing failed"
                );
                results.push(DocumentResult {
                    filename,
                    status: "failed".to_string(),
                    error: Some(e.to_string()),
                });
            }
        }
    }

    // Commit index
    info!("Committing index");
    if let Err(e) = index_writer.commit() {
        error!(error = %e, "Failed to commit index");
        return Json(IndexResponse {
            results,
            index_name: None,
            download_url: None,
        });
    }

    info!(
        total_documents = results.len(),
        successful = results.iter().filter(|r| r.status == "indexed").count(),
        failed = results.iter().filter(|r| r.status == "failed").count(),
        "JSON processing completed"
    );

    // generate download url for index file
    let url = {
        // get the socket address of request
        let download_url_prefix = DOWNLOAD_URL_PREFIX.get().unwrap();

        let host = match download_url_prefix.port() {
            Some(port) => {
                format!("{}:{}", download_url_prefix.host_str().unwrap(), port)
            }
            None => download_url_prefix.host_str().unwrap().to_string(),
        };

        format!(
            "{}://{}/v1/files/download/{}",
            download_url_prefix.scheme(),
            host,
            &index_name,
        )
    };
    info!(url = %url, "Download URL generated");

    Json(IndexResponse {
        results,
        index_name: Some(index_name),
        download_url: Some(url),
    })
}

// Process document content
fn process_content(content: &str) -> Result<(), String> {
    // Add actual document processing logic here
    // For now, just validate that content is not empty
    if content.trim().is_empty() {
        return Err("Empty content is not allowed".to_string());
    }
    Ok(())
}

// Validate content type
fn is_valid_content_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "text/plain" | "text/markdown" | "application/octet-stream" // Sometimes file uploads might not have the correct content-type
    )
}

// Add the query handler function
async fn query_handler(Json(request): Json<QueryRequest>) -> Json<QueryResponse> {
    info!(
        query = %request.query,
        top_k = request.top_k,
        "Received search request"
    );

    let index_path = std::env::current_dir()
        .unwrap()
        .join(INDEX_STORAGE_DIR)
        .join(&request.index);
    if !index_path.exists() {
        let err_msg = format!("Index '{}' does not exist", request.index);

        error!("{}", &err_msg);

        return Json(QueryResponse {
            hits: Vec::new(),
            error: Some(err_msg),
        });
    }

    info!(path = %index_path.display(), "Opening index");
    let index = match Index::open_in_dir(&index_path) {
        Ok(index) => index,
        Err(e) => {
            let err_msg = format!("Failed to open index: {}", e);

            error!("{}", &err_msg);

            return Json(QueryResponse {
                hits: Vec::new(),
                error: Some(err_msg),
            });
        }
    };

    // create reader
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()
        .unwrap();

    // acquire searcher
    let searcher = reader.searcher();

    // get schema
    let schema = index.schema();

    // get fields
    let title = schema.get_field("title").unwrap();
    let body = schema.get_field("body").unwrap();

    // create query parser
    let query_parser = QueryParser::for_index(&index, vec![title, body]);

    // parse query
    let query_str = format!("body:{}", &request.query);
    let query = match query_parser.parse_query(&query_str) {
        Ok(q) => q,
        Err(e) => {
            let err_msg = format!("Failed to parse query: {}", e);

            error!("{}", &err_msg);

            return Json(QueryResponse {
                hits: Vec::new(),
                error: Some(err_msg),
            });
        }
    };

    // execute search
    info!("Executing search");
    let top_docs = match searcher.search(&query, &TopDocs::with_limit(request.top_k)) {
        Ok(docs) => docs,
        Err(e) => {
            let err_msg = format!("Search failed: {}", e);

            error!("{}", &err_msg);

            return Json(QueryResponse {
                hits: Vec::new(),
                error: Some(err_msg),
            });
        }
    };

    // collect hits
    let mut hits = Vec::new();
    for (score, doc_address) in top_docs {
        let retrieved_doc: TantivyDocument = searcher.doc(doc_address).unwrap();

        let title_value = retrieved_doc
            .get_first(title)
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();

        let body_value = retrieved_doc
            .get_first(body)
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();

        info!(
            score = score,
            title = title_value,
            body = body_value,
            "Retrieved document"
        );

        hits.push(SearchHit {
            title: title_value,
            content: body_value,
            score,
        });
    }

    info!(hits = hits.len(), "Search completed successfully");

    Json(QueryResponse { hits, error: None })
}

// download index file
async fn download_index_file_handler(
    Path(index_name): Path<String>,
) -> impl axum::response::IntoResponse {
    info!(
        index_name = %index_name,
        "Received index file download request"
    );

    let index_storage_dir = std::env::current_dir().unwrap().join(INDEX_STORAGE_DIR);
    let index_path = index_storage_dir.as_path().join(&index_name);

    // Check if index exists
    if !index_path.exists() {
        let err_msg = format!("Index '{}' not found", index_name);
        error!(
            index_name = %index_name,
            path = %index_path.display(),
            "Index directory not found"
        );
        return (StatusCode::NOT_FOUND, err_msg).into_response();
    }

    info!("Found index directory");

    // Prepare compression
    let compressed_filename = format!("{}.tar.gz", index_name);
    let compressed_index_path = index_storage_dir.as_path().join(&compressed_filename);

    // check if compressed file exists
    if !compressed_index_path.exists() {
        info!("Starting index compression");

        // Create compressed file
        let file = match File::create(&compressed_index_path) {
            Ok(file) => {
                info!(
                    path = %compressed_index_path.display(),
                    "Created compressed file"
                );
                file
            }
            Err(e) => {
                let err_msg = format!("Failed to create compressed index file: {}", e);
                error!(
                    error = %e,
                    path = %compressed_index_path.display(),
                    "Failed to create compressed file"
                );
                return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
            }
        };

        // Compress directory
        let mut builder = tar::Builder::new(file);
        if let Err(e) = builder.append_dir_all(".", &index_path) {
            let err_msg = format!("Failed to compress index directory: {}", e);
            error!(
                error = %e,
                source = %index_path.display(),
                target = %compressed_index_path.display(),
                "Failed to compress index directory"
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
        }

        if let Err(e) = builder.finish() {
            let err_msg = format!("Failed to finalize index compression: {}", e);
            error!(
                error = %e,
                path = %compressed_index_path.display(),
                "Failed to finalize compression"
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
        }
    }

    info!("Index compression completed");

    // Read compressed file
    let mut file = match File::open(&compressed_index_path) {
        Ok(file) => file,
        Err(e) => {
            let err_msg = format!("Failed to open the compressed file: {}", e);
            error!(
                error = %e,
                path = %compressed_index_path.display(),
                "Failed to open compressed file"
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
        }
    };

    // Read file content
    let mut buffer: Vec<u8> = Vec::new();
    if let Err(e) = file.read_to_end(&mut buffer) {
        let err_msg = format!("Failed to read the compressed file content: {}", e);
        error!(
            error = %e,
            path = %compressed_index_path.display(),
            "Failed to read file content"
        );
        return (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response();
    }

    // Prepare response
    let content_type = "application/gzip";
    let content_disposition = format!("attachment; filename=\"{}\"", compressed_filename);
    let content_length = buffer.len();
    let body = axum::body::Body::from(buffer);

    info!(
        index_name = %index_name,
        content_type = %content_type,
        content_length = content_length,
        filename = %compressed_filename,
        "Prepared download response"
    );

    match axum::response::Response::builder()
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "*")
        .header("Access-Control-Allow-Headers", "*")
        .header("Content-Type", content_type)
        .header("Content-Disposition", content_disposition.as_str())
        .header("Content-Length", content_length.to_string().as_str())
        .body(body)
    {
        Ok(response) => {
            info!("Returned download response");
            response
        }
        Err(e) => {
            let err_msg = format!("Failed to build response: {}", e);
            error!(
                error = %e,
                index_name = %index_name,
                "Failed to build response"
            );
            (StatusCode::INTERNAL_SERVER_ERROR, err_msg).into_response()
        }
    }
}
