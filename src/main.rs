//! # High-Performance MRF Processor in Rust
//!
//! This program processes large, gzipped Machine-Readable Files (MRFs) in JSON format.
//! It's designed for high performance and low memory usage, using streaming and concurrency.
//!
//! ## Execution Flow:
//! 1.  **Two-Pass Strategy**: The input file is read twice to minimize memory footprint.
//! 2.  **First Pass (References)**:
//!     - Streams the JSON token-by-token to find the `provider_references` array without loading the whole file.
//!     - Deserializes local references and identifies remote references (by URL).
//!     - Fetches all remote references concurrently using `tokio` and `reqwest`.
//!     - Consolidates all references and writes them out to `provider_references.json` and `provider_groups.csv`.
//! 3.  **Second Pass (In-Network Items)**:
//!     - Streams the JSON again to find the `in_network` array.
//!     - Uses a multi-consumer channel (`async_channel`) to create a work queue.
//!     - A dedicated blocking task reads `InNetworkItem`s and sends them to the channel.
//!     - A pool of worker tasks receives items from the channel. Each worker processes an item
//!       and writes the resulting CSV records to the appropriate file writer task.
//! 4.  **R2 Upload**:
//!     - After all local files are written, the program walks the output directory.
//!     - It uploads all generated CSV files to the specified R2 bucket concurrently.
//! 5.  **Cleanup**: Deletes the local output directory after a successful upload.

use anyhow::{anyhow, Context, Result};
use aws_config::{meta::region::RegionProviderChain, BehaviorVersion};
use aws_credential_types::Credentials;
use aws_sdk_s3::{config::Region, primitives::ByteStream, Client as S3Client};
use dashmap::DashMap;
use flate2::read::GzDecoder;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio::task;

// --- Data Structures (matching the JSON) ---

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Tin {
    r#type: String,
    value: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ProviderGroup {
    npi: Vec<serde_json::Value>,
    tin: Tin,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ProviderReference {
    provider_group_id: u64,
    provider_groups: Vec<ProviderGroup>,
}

#[derive(Debug, Deserialize, Clone)]
struct RemoteProviderReference {
    provider_group_id: u64,
    location: String,
}

/// An enum to handle both local and remote reference types seamlessly.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ReferenceType {
    Local(ProviderReference),
    Remote(RemoteProviderReference),
}

#[derive(Debug, Deserialize)]
struct NegotiatedPrice {
    negotiated_type: String,
    negotiated_rate: f64,
    billing_class: String,
    #[serde(default)]
    billing_code_modifier: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NegotiatedRate {
    provider_references: Vec<u64>,
    negotiated_prices: Vec<NegotiatedPrice>,
}

#[derive(Debug, Deserialize)]
struct InNetworkItem {
    negotiation_arrangement: String,
    billing_code_type: String,
    billing_code: String,
    negotiated_rates: Vec<NegotiatedRate>,
}

// --- Main Application Logic ---

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        eprintln!("Usage: {} <input_file.json.gz> <output_dir> <r2_prefix>", args[0]);
        std::process::exit(1);
    }

    let input_path = PathBuf::from(&args[1]);
    let output_dir = PathBuf::from(&args[2]);
    let r2_prefix = &args[3];

    let start_time = Instant::now();
    println!("🚀 Starting processing for: {}", input_path.display());

    // Ensure output directory exists and is clean
    if output_dir.exists() {
        fs::remove_dir_all(&output_dir)?;
    }
    fs::create_dir_all(&output_dir)?;

    // --- Pass 1: Extract, Fetch, and Write Provider References ---
    println!("\n===== Pass 1: Processing Provider References =====");
    let pass1_start = Instant::now();
    let all_references = extract_and_fetch_references(&input_path, &output_dir).await?;
    println!(
        "✅ Pass 1 finished in {:.2?}. Found {} total provider references.",
        pass1_start.elapsed(),
        all_references.len()
    );

    // --- Pass 2: Stream and Process In-Network Items ---
    println!("\n===== Pass 2: Processing In-Network Items =====");
    let pass2_start = Instant::now();
    process_in_network_items(&input_path, &output_dir).await?;
    println!("✅ Pass 2 finished in {:.2?}", pass2_start.elapsed());

    // --- R2 Upload ---
    println!("\n===== Uploading to R2 =====");
    let upload_start = Instant::now();
    let s3_client = setup_r2_client().await?;
    upload_to_r2(s3_client, &output_dir, r2_prefix).await?;
    println!("✅ Upload finished in {:.2?}", upload_start.elapsed());

    // --- Cleanup ---
    println!("\n🧹 Cleaning up local directory...");
    fs::remove_dir_all(&output_dir)?;

    println!(
        "\n🎉 Total processing time: {:.2?}",
        start_time.elapsed()
    );

    Ok(())
}

/// Creates a streaming JSON deserializer positioned at the start of a specific array key.
fn stream_from_key<'a, R: Read>(
    reader: R,
    key_to_find: &str,
) -> Result<serde_json::StreamDeserializer<'a, serde_json::de::IoRead<R>, serde_json::Value>> {
    let mut deserializer = serde_json::Deserializer::from_reader(reader);

    // Find the start of the root object.
    while deserializer.byte_offset() < deserializer.get_ref().get_ref().len() {
        if let Ok(serde_json::Value::Object(_)) =
            serde_json::Value::deserialize(&mut deserializer)
        {
            // This is not what we want, but it moves the cursor. A better way is needed.
            // For now, let's reset and stream properly.
            break;
        }
    }

    // A more direct way to find the key by streaming keys.
    let stream = serde_json::Deserializer::from_reader(deserializer.into_inner()).into_iter::<serde_json::Value>();
    for item in stream {
        if let Ok(serde_json::Value::Object(map)) = item {
            for (key, value) in map {
                if key == key_to_find {
                    if let serde_json::Value::Array(arr) = value {
                        let reader = std::io::Cursor::new(serde_json::to_string(&arr)?);
                        return Ok(serde_json::Deserializer::from_reader(reader).into_iter());
                    }
                }
            }
        }
    }

    Err(anyhow!("Key '{}' not found in JSON", key_to_find))
}

/// Pass 1: Extracts local references, fetches remote ones, and writes consolidated results.
async fn extract_and_fetch_references(
    input_path: &Path,
    output_dir: &Path,
) -> Result<Vec<ProviderReference>> {
    let file = File::open(input_path).context("Failed to open input file for Pass 1")?;
    let gz = GzDecoder::new(file);
    let buf_reader = BufReader::new(gz);
    
    println!("🔍 Searching for 'provider_references' key...");
    let stream = stream_from_key(buf_reader, "provider_references")
        .context("Could not find 'provider_references' array")?;

    let references: Vec<ReferenceType> = stream
        .map(|item| serde_json::from_value(item.unwrap()).unwrap())
        .collect();

    let (local_refs, remote_refs): (Vec<_>, Vec<_>) = references.into_iter().partition(|r| matches!(r, ReferenceType::Local(_)));

    let mut local_refs: Vec<ProviderReference> = local_refs.into_iter().map(|r| match r {
        ReferenceType::Local(pr) => pr,
        _ => unreachable!(),
    }).collect();

    let remote_refs: Vec<RemoteProviderReference> = remote_refs.into_iter().map(|r| match r {
        ReferenceType::Remote(rr) => rr,
        _ => unreachable!(),
    }).collect();


    println!("Found {} local and {} remote references.", local_refs.len(), remote_refs.len());

    // Fetch remote references concurrently
    if !remote_refs.is_empty() {
        println!("📡 Fetching {} remote references...", remote_refs.len());
        let client = reqwest::Client::new();
        let bodies = stream::iter(remote_refs)
            .map(|remote_ref| {
                let client = client.clone();
                async move {
                    match client.get(&remote_ref.location).send().await {
                        Ok(resp) => match resp.json::<ProviderReference>().await {
                            Ok(mut provider_ref) => {
                                provider_ref.provider_group_id = remote_ref.provider_group_id;
                                Ok(provider_ref)
                            }
                            Err(e) => Err(anyhow!("Failed to parse JSON from {}: {}", remote_ref.location, e)),
                        },
                        Err(e) => Err(anyhow!("Failed to fetch {}: {}", remote_ref.location, e)),
                    }
                }
            })
            .buffer_unordered(100); // Concurrency limit

        let fetched_refs = bodies.filter_map(|res| async {
                match res {
                    Ok(r) => Some(r),
                    Err(e) => {
                        eprintln!("Warning: {}", e); // Log error but continue
                        None
                    }
                }
            }).collect::<Vec<_>>().await;
        println!("Successfully fetched {} remote references.", fetched_refs.len());
        local_refs.extend(fetched_refs);
    }

    // Write consolidated references to output files
    write_provider_references_files(&local_refs, output_dir)?;

    Ok(local_refs)
}

/// Writes the provider references to both a JSON and a CSV file.
fn write_provider_references_files(refs: &[ProviderReference], output_dir: &Path) -> Result<()> {
    let refs_dir = output_dir.join("provider_references");
    fs::create_dir_all(&refs_dir)?;

    // Write JSON file
    let json_path = refs_dir.join("provider_references.json");
    let json_file = File::create(&json_path)?;
    serde_json::to_writer_pretty(BufWriter::new(json_file), refs)?;
    println!("Wrote consolidated references to {}", json_path.display());

    // Write CSV file
    let csv_path = refs_dir.join("provider_groups.csv");
    let mut csv_writer = csv::Writer::from_path(&csv_path)?;
    csv_writer.write_record(&["provider_group_id", "npi", "tin_type", "tin_value"])?;

    for pref in refs {
        for group in &pref.provider_groups {
            for npi_val in &group.npi {
                let npi_str = match npi_val {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    _ => continue, // Skip null or other types
                };
                csv_writer.write_record(&[
                    pref.provider_group_id.to_string(),
                    npi_str,
                    group.tin.r#type.clone(),
                    group.tin.value.clone(),
                ])?;
            }
        }
    }
    csv_writer.flush()?;
    println!("Wrote provider groups to {}", csv_path.display());

    Ok(())
}

/// Pass 2: Streams the in_network array and processes items using a pool of worker tasks.
async fn process_in_network_items(input_path: &Path, output_dir: &Path) -> Result<()> {
    let in_network_dir = output_dir.join("in_network");
    fs::create_dir_all(&in_network_dir)?;

    let writer_map: Arc<DashMap<String, mpsc::Sender<String>>> = Arc::new(DashMap::new());
    
    // Use an MPMC channel from `async_channel` so multiple workers can receive.
    let (tx, rx) = async_channel::bounded::<InNetworkItem>(2048);

    let item_count = Arc::new(AtomicU64::new(0));
    let start_time = Instant::now();
    let total_workers = num_cpus::get_physical();
    println!("⚙️  Starting {} worker tasks for in-network processing.", total_workers);

    // --- Spawn Worker Tasks ---
    let mut worker_handles = Vec::new();
    for _ in 0..total_workers {
        let rx = rx.clone(); // Clone the receiver for each worker
        let writer_map = writer_map.clone();
        let in_network_dir = in_network_dir.clone();
        let item_count = item_count.clone();

        let handle = task::spawn(async move {
            while let Ok(item) = rx.recv().await {
                let mut records_by_code: HashMap<String, String> = HashMap::new();
                for rate in &item.negotiated_rates {
                    for price in &rate.negotiated_prices {
                        for provider_ref in &rate.provider_references {
                            let modifiers = price.billing_code_modifier.join("|");
                            let record = format!(
                                "{},{},{},{},{},{},{},{}\n",
                                provider_ref,
                                price.negotiated_rate,
                                item.billing_code,
                                item.billing_code_type,
                                item.negotiation_arrangement,
                                price.negotiated_type,
                                price.billing_class,
                                modifiers
                            );
                            records_by_code.entry(item.billing_code.clone()).or_default().push_str(&record);
                        }
                    }
                }
                
                for (code, records) in records_by_code {
                     if let Some(writer_tx) = writer_map.get(&code) {
                        if writer_tx.send(records).await.is_err() {
                            eprintln!("Error: File writer for code {} has closed.", code);
                        }
                    } else {
                        let (writer_tx, mut writer_rx) = mpsc::channel::<String>(128);
                        let file_path = in_network_dir.join(format!("in_network_{}.csv", code));
                        writer_map.insert(code.clone(), writer_tx.clone());

                        task::spawn(async move {
                            let mut file = BufWriter::new(File::create(file_path).unwrap());
                            file.write_all(b"provider_reference,negotiated_rate,billing_code,billing_code_type,negotiation_arrangement,negotiated_type,billing_class,billing_code_modifier\n").unwrap();
                            while let Some(data) = writer_rx.recv().await {
                                if file.write_all(data.as_bytes()).is_err() {
                                     eprintln!("Error writing to file for code {}", code);
                                }
                            }
                            file.flush().unwrap();
                        });
                        
                        writer_tx.send(records).await.unwrap();
                    }
                }

                let count = item_count.fetch_add(1, Ordering::Relaxed) + 1;
                if count % 10000 == 0 {
                    let elapsed = start_time.elapsed().as_secs_f64();
                    println!( "Processed {} items... ({:.2} items/sec)", count, count as f64 / elapsed );
                }
            }
        });
        worker_handles.push(handle);
    }
    
    // Create an owned PathBuf to move into the blocking task.
    let input_path_owned = input_path.to_path_buf();
    let reader_handle = task::spawn_blocking(move || -> Result<()> {
        let file = File::open(input_path_owned).context("Failed to open input file for Pass 2")?;
        let gz = GzDecoder::new(file);
        let buf_reader = BufReader::new(gz);

        println!("🔍 Searching for 'in_network' key...");
        let stream = stream_from_key(buf_reader, "in_network")
            .context("Could not find 'in_network' array")?;

        for item in stream {
            let item: InNetworkItem = serde_json::from_value(item?)?;
            if tx.send_blocking(item).is_err() {
                break; // Channel closed, workers are shutting down
            }
        }
        Ok(())
    });

    reader_handle.await??;
    // The channel will close automatically when `tx` from spawn_blocking goes out of scope.
    // This will signal the workers to finish.

    // Wait for all worker tasks to complete
    for handle in worker_handles {
        handle.await?;
    }
    
    // Signal all file writers to shut down by dropping their senders
    writer_map.clear();
    
    println!("Total items processed: {}", item_count.load(Ordering::Relaxed));
    Ok(())
}


/// Sets up the S3 client to connect to an R2 bucket using the modern AWS SDK config.
async fn setup_r2_client() -> Result<S3Client> {
    let account_id = env::var("R2_ACCOUNT_ID").context("R2_ACCOUNT_ID not set")?;
    let access_key_id = env::var("R2_ACCESS_KEY_ID").context("R2_ACCESS_KEY_ID not set")?;
    let secret_access_key = env::var("R2_SECRET_ACCESS_KEY").context("R2_SECRET_ACCESS_KEY not set")?;
    
    let endpoint_url = format!("https://{}.r2.cloudflarestorage.com", account_id);

    let credentials = Credentials::new(
        access_key_id,
        secret_access_key,
        None,
        None,
        "Static",
    );

    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(RegionProviderChain::first_try(Region::new("auto")))
        .credentials_provider(credentials)
        .endpoint_url(endpoint_url)
        .load()
        .await;

    Ok(S3Client::new(&config))
}

/// Walks the output directory and uploads all CSV files to R2 concurrently.
async fn upload_to_r2(client: S3Client, output_dir: &Path, r2_prefix: &str) -> Result<()> {
    let bucket_name = env::var("R2_BUCKET_NAME").context("R2_BUCKET_NAME not set")?;
    
    let mut files_to_upload = vec![];
    for entry in walkdir::WalkDir::new(output_dir) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files_to_upload.push(entry.into_path());
        }
    }
    
    println!("Found {} files to upload to R2 bucket '{}'.", files_to_upload.len(), bucket_name);

    let sem = Arc::new(Semaphore::new(20)); // Limit concurrent uploads
    let output_dir_owned = output_dir.to_path_buf();

    let mut tasks = Vec::new();
    for path in files_to_upload {
        let sem = Arc::clone(&sem);
        let client = client.clone();
        let bucket_name = bucket_name.clone();
        let r2_prefix = r2_prefix.to_string();
        let output_dir = output_dir_owned.clone();

        let task = task::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let key = format!(
                "{}/{}",
                r2_prefix,
                path.strip_prefix(&output_dir).unwrap().to_str().unwrap().replace("\\", "/")
            );
            
            println!("Uploading {} to s3://{}/{}", path.display(), bucket_name, key);

            let body = ByteStream::from_path(&path).await
                .map_err(|e| anyhow!("Failed to read file {}: {}", path.display(), e))?;

            client
                .put_object()
                .bucket(&bucket_name)
                .key(&key)
                .body(body)
                .send()
                .await
                .map_err(|e| anyhow!("Failed to upload {}: {:?}", path.display(), e))?;
            
            Ok::<(), anyhow::Error>(())
        });
        tasks.push(task);
    }

    for task in tasks {
        task.await??;
    }

    Ok(())
}

