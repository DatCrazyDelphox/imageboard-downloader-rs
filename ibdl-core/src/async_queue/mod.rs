#![allow(unused_imports)]
//! Queue used specifically to download, filter and save posts found by an [`Extractor`](ibdl-extractors::websites).
//!
//! # Example usage
//!
//! Conveniently using the same example from [here](ibdl-extractors::websites)
//!
//! ```rust
//! use imageboard_downloader::*;
//! use std::path::PathBuf;
//!
//! async fn download_posts() {
//!     let tags = ["umbreon", "espeon"]; // The tags to search
//!     
//!     let safe_mode = false; // Setting this to true, will ignore searching NSFW posts
//!
//!     let disable_blacklist = false; // Will filter all items according to what's set in GBL
//!
//!     let mut unit = DanbooruExtractor::new(&tags, safe_mode, disable_blacklist); // Initialize
//!
//!     let prompt = true; // If true, will ask the user to input thei username and API key.
//!
//!     unit.auth(prompt).await.unwrap(); // Try to authenticate
//!
//!     let start_page = Some(1); // Start searching from the first page
//!
//!     let limit = Some(50); // Max number of posts to download
//!
//!     let posts = unit.full_search(start_page, limit).await.unwrap(); // and then, finally search
//!
//!     let sd = 10; // Number of simultaneous downloads.
//!
//!     let limit = Some(1000); // Max number of posts to download
//!
//!     let cbz = false; // Set to true to download everything into a .cbz file
//!
//!     let mut qw = Queue::new( // Initialize the queue
//!         ImageBoards::Danbooru,
//!         posts,
//!         sd,
//!         Some(unit.client()), // Re-use the client from the extractor
//!         limit,
//!         cbz,
//!     );
//!
//!     let output = Some(PathBuf::from("./")); // Where to save the downloaded files or .cbz file
//!
//!     let id = true; // Save file with their ID as the filename instead of MD5
//!
//!     qw.download(output, id).await.unwrap(); // Start downloading
//! }
//! ```
//mod summary;

use futures::stream::iter;
use futures::StreamExt;
use ibdl_common::log::{debug, trace};
use ibdl_common::post::error::PostError;
use ibdl_common::post::rating::Rating;
use ibdl_common::post::tags::TagType;
use ibdl_common::post::{NameType, Post, PostQueue};
use ibdl_common::reqwest::Client;
use ibdl_common::tokio::spawn;
use ibdl_common::tokio::sync::mpsc::{channel, Sender, UnboundedReceiver};
use ibdl_common::tokio::task::JoinHandle;
use ibdl_common::{client, tokio, ImageBoards};
use md5::compute;
use once_cell::sync::OnceCell;
use owo_colors::OwoColorize;
use std::convert::TryInto;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::fs::{create_dir_all, read, remove_file, rename, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::task::{self, spawn_blocking};
use tokio_stream::wrappers::UnboundedReceiverStream;
use zip::write::FileOptions;
use zip::CompressionMethod;
use zip::ZipWriter;

use crate::progress_bars::ProgressCounter;

use crate::error::QueueError;

static PROGRESS_COUNTERS: OnceCell<ProgressCounter> = OnceCell::new();

macro_rules! get_counters {
    () => {{
        PROGRESS_COUNTERS.get().unwrap()
    }};
}

#[derive(Debug, Copy, Clone)]
enum DownloadFormat {
    Cbz,
    CbzPool,
    Folder,
}

impl DownloadFormat {
    #[inline]
    pub fn download_cbz(&self) -> bool {
        match self {
            DownloadFormat::Cbz => true,
            DownloadFormat::CbzPool => true,
            DownloadFormat::Folder => false,
        }
    }

    #[inline]
    pub fn download_pool_cbz(&self) -> bool {
        match self {
            DownloadFormat::Cbz => false,
            DownloadFormat::CbzPool => true,
            DownloadFormat::Folder => false,
        }
    }
}

/// Struct where all the downloading will take place
pub struct Queue {
    imageboard: ImageBoards,
    sim_downloads: u8,
    client: Client,
    download_fmt: DownloadFormat,
    name_type: NameType,
    annotate: bool,
}

impl Queue {
    /// Set up the queue for download
    pub fn new(
        imageboard: ImageBoards,
        sim_downloads: u8,
        custom_client: Option<Client>,
        save_as_cbz: bool,
        pool_download: bool,
        name_type: NameType,
        annotate: bool,
    ) -> Self {
        let client = if let Some(cli) = custom_client {
            cli
        } else {
            client!(imageboard)
        };

        let download_fmt = if save_as_cbz && pool_download {
            DownloadFormat::CbzPool
        } else if save_as_cbz {
            DownloadFormat::Cbz
        } else {
            DownloadFormat::Folder
        };

        Self {
            download_fmt,
            imageboard,
            sim_downloads,
            annotate,
            client,
            name_type,
        }
    }

    pub fn setup_async_downloader(
        self,
        output_dir: PathBuf,
        post_counter: Arc<AtomicU64>,
        channel_rx: UnboundedReceiver<Post>,
    ) -> JoinHandle<Result<u64, QueueError>> {
        spawn(async move {
            debug!("Async Downloader thread initialized");

            let counters = PROGRESS_COUNTERS.get_or_init(|| {
                ProgressCounter::initialize(post_counter.load(Ordering::Relaxed), self.imageboard)
            });

            self.create_out(&output_dir).await?;

            let post_channel = UnboundedReceiverStream::new(channel_rx);
            let (progress_sender, progress_channel) = channel(self.sim_downloads as usize);

            if self.download_fmt.download_cbz() {
                counters.init_length_updater(post_counter.clone(), 500);
                counters.init_download_counter(progress_channel);

                self.cbz_path(
                    output_dir,
                    progress_sender,
                    post_channel,
                    self.download_fmt.download_pool_cbz(),
                )
                .await?;

                counters.main.finish_and_clear();

                let tot = counters.downloaded_mtx.load(Ordering::Relaxed);

                return Ok(tot);
            }

            counters.init_length_updater(post_counter.clone(), 500);
            counters.init_download_counter(progress_channel);

            self.download_channel(post_channel, progress_sender, output_dir)
                .await;

            counters.main.finish_and_clear();

            let tot = counters.downloaded_mtx.load(Ordering::Relaxed);

            Ok(tot)
        })
    }

    async fn download_channel(
        &self,
        channel: UnboundedReceiverStream<Post>,
        progress: Sender<bool>,
        output_dir: PathBuf,
    ) {
        let sender = progress.clone();

        channel
            .map(|d| {
                let nt = self.name_type;

                let cli = self.client.clone();
                let output = output_dir.clone();
                let file_path = output_dir.join(d.file_name(self.name_type));
                let variant = self.imageboard;

                task::spawn(async move {
                    if !Self::check_file_exists(&d, &file_path, nt).await? {
                        Self::fetch(cli, variant, &d, &output, nt).await?;
                    }
                    Ok::<Post, QueueError>(d)
                })
            })
            .buffer_unordered(self.sim_downloads as usize)
            .for_each(|task| async {
                if let Ok(Ok(post)) = task {
                    if self.annotate {
                        if let Err(error) =
                            Self::write_caption(&post, self.name_type, &output_dir).await
                        {
                            let ctrs = get_counters!();
                            ctrs.multi
                                .println(format!(
                                    "{} {}: {}",
                                    "Failed to write caption file for".red().bold(),
                                    post.file_name(self.name_type).red().bold(),
                                    error
                                ))
                                .unwrap();
                        };
                    }
                    let _ = sender.send(true).await;
                }
            })
            .await
    }

    async fn create_out(&self, dir: &Path) -> Result<(), QueueError> {
        if self.download_fmt.download_cbz() {
            let output_file = dir.parent().unwrap().to_path_buf();

            match create_dir_all(&output_file).await {
                Ok(_) => (),
                Err(error) => {
                    return Err(QueueError::DirCreationError {
                        message: error.to_string(),
                    })
                }
            };
            return Ok(());
        }

        debug!("Target dir: {}", dir.display());
        match create_dir_all(&dir).await {
            Ok(_) => (),
            Err(error) => {
                return Err(QueueError::DirCreationError {
                    message: error.to_string(),
                })
            }
        };

        Ok(())
    }

    async fn cbz_path(
        &self,
        path: PathBuf,
        progress_channel: Sender<bool>,
        channel: UnboundedReceiverStream<Post>,
        pool: bool,
    ) -> Result<(), QueueError> {
        debug!("Target file: {}", path.display());

        let file = File::create(&path)?;
        let zip = Arc::new(Mutex::new(ZipWriter::new(file)));

        if !pool {
            self.write_zip_structure(zip.clone())?;
        }
        let sender = progress_channel.clone();

        channel
            .map(|d| {
                let nt = self.name_type;

                let cli = self.client.clone();
                let zip = zip.clone();
                let variant = self.imageboard;
                let annotate = self.annotate;

                task::spawn(async move {
                    if pool {
                        Self::fetch_cbz_pool(cli, variant, nt, d, zip).await?;
                        return Ok::<(), QueueError>(());
                    }

                    Self::fetch_cbz(cli, variant, nt, d, annotate, zip).await?;
                    Ok::<(), QueueError>(())
                })
            })
            .buffer_unordered(self.sim_downloads.into())
            .for_each(|_| async {
                let _ = sender.send(true).await;
            })
            .await;

        let mut mtx = zip.lock().unwrap();

        mtx.finish()?;
        Ok(())
    }

    fn write_zip_structure(&self, zip: Arc<Mutex<ZipWriter<File>>>) -> Result<(), QueueError> {
        let mut z_1 = zip.lock().unwrap();
        z_1.add_directory(Rating::Safe.to_string(), FileOptions::default())?;
        z_1.add_directory(Rating::Questionable.to_string(), FileOptions::default())?;
        z_1.add_directory(Rating::Explicit.to_string(), FileOptions::default())?;
        z_1.add_directory(Rating::Unknown.to_string(), FileOptions::default())?;

        Ok(())
    }

    async fn check_file_exists(
        post: &Post,
        output: &Path,
        name_type: NameType,
    ) -> Result<bool, QueueError> {
        let counters = get_counters!();
        let id_name = post.file_name(NameType::ID);
        let md5_name = post.file_name(NameType::MD5);

        let name = post.file_name(name_type);

        let raw_path = output.parent().unwrap();

        let (actual, file_is_same) = match name_type {
            NameType::ID if output.exists() => {
                debug!("File {} found.", &name);
                (output.to_path_buf(), false)
            }
            NameType::ID => {
                debug!("File {} not found.", &name);
                debug!("Trying possibly matching file: {}", &md5_name);
                (raw_path.join(Path::new(&md5_name)), true)
            }
            NameType::MD5 if output.exists() => {
                debug!("File {} found.", &name);
                (output.to_path_buf(), false)
            }
            NameType::MD5 => {
                debug!("File {} not found.", &name);
                debug!("Trying possibly matching file: {}", &id_name);
                (raw_path.join(Path::new(&id_name)), true)
            }
        };

        if actual.exists() {
            debug!(
                "Found file {}",
                actual.file_name().unwrap().to_str().unwrap()
            );
            let file_digest = compute(read(&actual).await?);
            let hash = format!("{:x}", file_digest);
            if hash == post.md5 {
                if file_is_same {
                    match counters.multi.println(format!(
                        "{} {} {}",
                        "A file similar to".bold().green(),
                        name.bold().blue().italic(),
                        "already exists and will be renamed accordingly."
                            .bold()
                            .green()
                    )) {
                        Ok(_) => {
                            rename(&actual, output).await?;
                        }
                        Err(error) => {
                            return Err(QueueError::ProgressBarPrintFail {
                                message: error.to_string(),
                            })
                        }
                    };

                    counters.main.inc(1);
                    counters.total_mtx.fetch_add(1, Ordering::SeqCst);
                    return Ok(true);
                }
                match counters.multi.println(format!(
                    "{} {} {}",
                    "File".bold().green(),
                    name.bold().blue().italic(),
                    "already exists. Skipping.".bold().green()
                )) {
                    Ok(_) => (),
                    Err(error) => {
                        return Err(QueueError::ProgressBarPrintFail {
                            message: error.to_string(),
                        })
                    }
                };

                counters.main.inc(1);
                counters.total_mtx.fetch_add(1, Ordering::SeqCst);
                return Ok(true);
            }
            remove_file(&actual).await?;
            counters.multi.println(format!(
                "{} {} {}",
                "File".bold().red(),
                name.bold().yellow().italic(),
                "is corrupted. Re-downloading...".bold().red()
            ))?;

            Ok(false)
        } else {
            Ok(false)
        }
    }

    async fn fetch_cbz_pool(
        client: Client,
        variant: ImageBoards,
        name_type: NameType,
        post: Post,
        zip: Arc<Mutex<ZipWriter<File>>>,
    ) -> Result<(), PostError> {
        let counters = get_counters!();
        let filename = post.file_name(name_type);
        debug!("Fetching {}", &post.url);
        let res = client.get(&post.url).send().await?;

        if res.status().is_client_error() {
            counters.multi.println(format!(
                "{} {}{}",
                "Image source returned status".bold().red(),
                res.status().as_str().bold().red(),
                ". Skipping download.".bold().red()
            ))?;
            counters.main.inc(1);
            return Err(PostError::RemoteFileNotFound);
        }

        let size = res.content_length().unwrap_or_default();

        let pb = counters.add_download_bar(size, variant);

        // Download the file chunk by chunk.
        debug!("Retrieving chunks for {}", &filename);
        let mut stream = res.bytes_stream();

        let buf_size: usize = size.try_into()?;

        let mut fvec: Vec<u8> = Vec::with_capacity(buf_size);

        let options = FileOptions::default().compression_method(CompressionMethod::Stored);

        while let Some(item) = stream.next().await {
            // Retrieve chunk.
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(e) => {
                    return Err(PostError::ChunkDownloadFail {
                        message: e.to_string(),
                    })
                }
            };
            pb.inc(chunk.len().try_into()?);

            // Write to file.
            AsyncWriteExt::write_all(&mut fvec, &chunk).await?;
        }

        spawn_blocking(move || -> Result<(), PostError> {
            let mut un_mut = zip.lock().unwrap();

            debug!("Writing {} to cbz file", filename);
            match un_mut.start_file(filename, options) {
                Ok(_) => {}
                Err(error) => {
                    return Err(PostError::ZipFileWriteError {
                        message: error.to_string(),
                    })
                }
            };

            un_mut.write_all(&fvec)?;

            Ok(())
        })
        .await??;

        pb.finish_and_clear();

        Ok(())
    }

    async fn fetch_cbz(
        client: Client,
        variant: ImageBoards,
        name_type: NameType,
        post: Post,
        annotate: bool,
        zip: Arc<Mutex<ZipWriter<File>>>,
    ) -> Result<(), PostError> {
        let counters = get_counters!();
        let filename = post.file_name(name_type);
        debug!("Fetching {}", &post.url);
        let res = client.get(&post.url).send().await?;

        if res.status().is_client_error() {
            counters.multi.println(format!(
                "{} {}{}",
                "Image source returned status".bold().red(),
                res.status().as_str().bold().red(),
                ". Skipping download.".bold().red()
            ))?;
            counters.main.inc(1);
            return Err(PostError::RemoteFileNotFound);
        }

        let size = res.content_length().unwrap_or_default();

        let pb = counters.add_download_bar(size, variant);

        // Download the file chunk by chunk.
        debug!("Retrieving chunks for {}", &filename);
        let mut stream = res.bytes_stream();

        let buf_size: usize = size.try_into()?;

        let mut fvec: Vec<u8> = Vec::with_capacity(buf_size);

        let options = FileOptions::default().compression_method(CompressionMethod::Stored);
        let cap_options = FileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(5));

        while let Some(item) = stream.next().await {
            // Retrieve chunk.
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(e) => {
                    return Err(PostError::ChunkDownloadFail {
                        message: e.to_string(),
                    })
                }
            };
            pb.inc(chunk.len().try_into()?);

            // Write to file.
            AsyncWriteExt::write_all(&mut fvec, &chunk).await?;
        }

        spawn_blocking(move || -> Result<(), PostError> {
            let mut un_mut = zip.lock().unwrap();

            debug!("Writing {} to cbz file", filename);
            match un_mut.start_file(format!("{}/{}", post.rating.to_string(), filename), options) {
                Ok(_) => {}
                Err(error) => {
                    return Err(PostError::ZipFileWriteError {
                        message: error.to_string(),
                    })
                }
            };

            un_mut.write_all(&fvec)?;

            if annotate {
                debug!("Writing caption for {} to cbz file", filename);
                match un_mut.start_file(
                    format!("{}/{}.txt", post.rating.to_string(), post.name(name_type)),
                    cap_options,
                ) {
                    Ok(_) => {}
                    Err(error) => {
                        return Err(PostError::ZipFileWriteError {
                            message: error.to_string(),
                        })
                    }
                };

                let tag_list = Vec::from_iter(
                    post.tags
                        .iter()
                        .filter(|t| t.is_prompt_tag())
                        .map(|tag| tag.tag()),
                );

                let prompt = tag_list.join(", ");

                let f1 = prompt.replace('_', " ");

                un_mut.write_all(f1.as_bytes())?;
            }
            Ok(())
        })
        .await??;

        pb.finish_and_clear();

        Ok(())
    }

    async fn fetch(
        client: Client,
        variant: ImageBoards,
        post: &Post,
        output: &Path,
        name_type: NameType,
    ) -> Result<(), PostError> {
        debug!("Fetching {}", &post.url);

        let counters = get_counters!();

        let res = client.get(&post.url).send().await?;

        if res.status().is_client_error() {
            counters.multi.println(format!(
                "{} {}{}",
                "Image source returned status".bold().red(),
                res.status().as_str().bold().red(),
                ". Skipping download.".bold().red()
            ))?;
            counters.main.inc(1);
            return Err(PostError::RemoteFileNotFound);
        }

        let size = res.content_length().unwrap_or_default();

        let pb = counters.add_download_bar(size, variant);

        // Download the file chunk by chunk.
        let mut stream = res.bytes_stream();

        let buf_size: usize = size.try_into()?;

        let out = output.join(post.file_name(name_type));

        debug!("Creating {:?}", &out);
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(out)
            .await?;

        let mut bw = BufWriter::with_capacity(buf_size, file);

        while let Some(item) = stream.next().await {
            // Retrieve chunk.
            let mut chunk = match item {
                Ok(chunk) => chunk,
                Err(e) => {
                    return Err(PostError::ChunkDownloadFail {
                        message: e.to_string(),
                    })
                }
            };
            pb.inc(chunk.len().try_into()?);

            // Write to file.
            bw.write_all_buf(&mut chunk).await?;
        }
        bw.flush().await?;

        pb.finish_and_clear();

        Ok(())
    }

    async fn write_caption(
        post: &Post,
        name_type: NameType,
        output: &Path,
    ) -> Result<(), PostError> {
        let outpath = output.join(format!("{}.txt", post.name(name_type)));
        let mut prompt_file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(outpath)
            .await?;

        let tag_list = Vec::from_iter(
            post.tags
                .iter()
                .filter(|t| t.is_prompt_tag())
                .map(|tag| tag.tag()),
        );

        let prompt = tag_list.join(", ");

        let f1 = prompt.replace('_', " ");

        prompt_file.write_all(f1.as_bytes()).await?;
        debug!("Wrote caption file for {}", post.file_name(name_type));
        Ok(())
    }
}
