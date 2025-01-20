use axum::{
    extract::{FromRequest, Multipart},
    routing::{get, post},
    Json, Router,
};
use clap::{ArgGroup, Parser};
use serde::{Deserialize, Serialize};
use std::{fs::File, net::SocketAddr, path::PathBuf};
use tantivy::{
    collector::TopDocs, doc, query::QueryParser, schema::*, Index, IndexWriter, ReloadPolicy,
};
use tempfile::TempDir;
use tracing::{debug, error, info, warn, Level};

// default port of Keyword Search Server
const DEFAULT_PORT: &str = "9069";

const MEMORY_BUDGET_IN_BYTES: usize = 100_000_000;

/// Command line arguments configuration
#[derive(Debug, Parser)]
#[command(name = "Keyword Search Server", version = env!("CARGO_PKG_VERSION"), author = env!("CARGO_PKG_AUTHORS"), about = "Keyword Search Server")]
#[command(group = ArgGroup::new("socket_address_group").multiple(false).args(&["socket_addr", "port"]))]
struct Cli {
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
    let cli = Cli::parse();
    info!("Server starting, command line arguments parsed");

    // Build application routes
    let app = Router::new()
        .route("/v1/index", post(index_document_handler))
        .route("/v1/search", post(query_handler));
    info!("Route configuration completed");

    // Run the server
    let addr = match cli.socket_addr {
        Some(addr) => addr,
        None => SocketAddr::from(([0, 0, 0, 0], cli.port)),
    };
    info!("Binding to address: {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("Server running at http://{}", addr);

    info!("Starting to accept connections...");
    axum::serve(listener, app).await.unwrap();
}

// Document indexing request for JSON input
#[derive(Debug, Deserialize)]
struct IndexRequest {
    documents: Vec<DocumentInput>,
}

#[derive(Debug, Clone, Deserialize)]
struct DocumentInput {
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    index_path: Option<String>,
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
                        index_path: None,
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
                        index_path: None,
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
                index_path: None,
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
    let index_path = TempDir::new().unwrap();
    let index_name = format!("index-{}", uuid::Uuid::new_v4());
    let index_path = index_path.path().join(&index_name);
    debug!(path = %index_path.display(), "Created temporary index directory");

    if !index_path.exists() {
        debug!(path = %index_path.display(), "Creating index directory");
        std::fs::create_dir_all(&index_path).unwrap();
    }

    // Define schema
    info!("Defining index schema");
    let mut schema_builder = Schema::builder();
    let title = schema_builder.add_text_field("title", TEXT | STORED);
    let body = schema_builder.add_text_field("body", TEXT);
    let schema = schema_builder.build();

    // Create index
    info!("Creating new index");
    let index = match Index::create_in_dir(&index_path, schema.clone()) {
        Ok(index) => index,
        Err(e) => {
            error!(error = %e, "Failed to create index");
            return Json(IndexResponse {
                results,
                index_path: None,
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
                index_path: None,
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
            index_path: None,
        });
    }

    // Compress index
    info!("Starting index compression");
    let compressed_filename = format!("{}.tar.gz", index_name);
    let compressed_index_path = std::env::current_dir().unwrap().join(&compressed_filename);

    let file = match File::create(&compressed_index_path) {
        Ok(file) => file,
        Err(e) => {
            error!(
                path = %compressed_index_path.display(),
                error = %e,
                "Failed to create compressed index file"
            );
            return Json(IndexResponse {
                results,
                index_path: None,
            });
        }
    };

    let mut builder = tar::Builder::new(file);
    if let Err(e) = builder.append_dir_all(".", &index_path) {
        error!(error = %e, "Failed to compress index directory");
        return Json(IndexResponse {
            results,
            index_path: None,
        });
    }
    if let Err(e) = builder.finish() {
        error!(error = %e, "Failed to finalize index compression");
        return Json(IndexResponse {
            results,
            index_path: None,
        });
    }

    info!(
        path = %compressed_index_path.display(),
        "Index compression completed"
    );

    Json(IndexResponse {
        results,
        index_path: Some(compressed_index_path.to_string_lossy().to_string()),
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
    let index_path = TempDir::new().unwrap();
    let index_name = format!("index-{}", uuid::Uuid::new_v4());
    let index_path = index_path.path().join(&index_name);
    debug!(path = %index_path.display(), "Created temporary index directory");

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
                index_path: None,
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
                index_path: None,
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
            index_path: None,
        });
    }

    // Compress index
    info!("Starting index compression");
    let compressed_filename = format!("{}.tar.gz", index_name);
    let compressed_index_path = std::env::current_dir().unwrap().join(&compressed_filename);

    let file = match File::create(&compressed_index_path) {
        Ok(file) => file,
        Err(e) => {
            error!(
                path = %compressed_index_path.display(),
                error = %e,
                "Failed to create compressed index file"
            );
            return Json(IndexResponse {
                results,
                index_path: None,
            });
        }
    };

    let mut builder = tar::Builder::new(file);
    if let Err(e) = builder.append_dir_all(".", &index_path) {
        error!(error = %e, "Failed to compress index directory");
        return Json(IndexResponse {
            results,
            index_path: None,
        });
    }
    if let Err(e) = builder.finish() {
        error!(error = %e, "Failed to finalize index compression");
        return Json(IndexResponse {
            results,
            index_path: None,
        });
    }

    info!(
        path = %compressed_index_path.display(),
        "Index compression completed"
    );

    info!(
        total_documents = results.len(),
        successful = results.iter().filter(|r| r.status == "indexed").count(),
        failed = results.iter().filter(|r| r.status == "failed").count(),
        "JSON processing completed"
    );

    Json(IndexResponse {
        results,
        index_path: Some(compressed_index_path.to_string_lossy().to_string()),
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

    let index_path = std::env::current_dir().unwrap().join(&request.index);
    if !index_path.exists() {
        error!(path = %index_path.display(), "Index path does not exist");
        return Json(QueryResponse {
            hits: Vec::new(),
            error: Some("Index path does not exist".to_string()),
        });
    }

    info!(path = %index_path.display(), "Opening index");
    let index = match Index::open_in_dir(&index_path) {
        Ok(index) => index,
        Err(e) => {
            error!(error = %e, "Failed to open index");
            return Json(QueryResponse {
                hits: Vec::new(),
                error: Some(format!("Failed to open index: {}", e)),
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
            error!(error = %e, "Failed to parse query");
            return Json(QueryResponse {
                hits: Vec::new(),
                error: Some(format!("Failed to parse query: {}", e)),
            });
        }
    };

    // execute search
    info!("Executing search");
    let top_docs = match searcher.search(&query, &TopDocs::with_limit(request.top_k)) {
        Ok(docs) => docs,
        Err(e) => {
            error!(error = %e, "Search failed");
            return Json(QueryResponse {
                hits: Vec::new(),
                error: Some(format!("Search failed: {}", e)),
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
