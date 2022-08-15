//! Common functions for all imageboard downloader modules.
use crate::progress_bars::download_progress_style;
use crate::ImageBoards;
use anyhow::{bail, Error};
use futures::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget};
use log::debug;
use md5::compute;
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::fs::{read, OpenOptions};
use tokio::io::AsyncWriteExt;

/// Checks if ```output_dir``` is set in cli args then returns a ```PathBuf``` pointing to where the files will be downloaded.
///
/// In case the user does not set a value with the ```-o``` flag, the function will default to the current dir where the program is running.
///
/// The path chosen will always end with the imageboard name followed by the tags used.
///
/// ```rust
/// let tags = join_tags!(["kroos_(arknights)", "weapon"]);
/// let path = Some(PathBuf::from("./"));
///
/// let out_dir = generate_out_dir(path, &tags, ImageBoards::Danbooru).unwrap();
///
/// assert_eq!(PathBuf::from("./danbooru/kroos_(arknights)+weapon"), out_dir);
/// ```
pub fn generate_out_dir(
    out_dir: Option<PathBuf>,
    tag_string: &String,
    imageboard: ImageBoards,
) -> Result<PathBuf, Error> {
    // If out_dir is not set via cli flags, use current dir
    let place = match out_dir {
        None => std::env::current_dir()?,
        Some(dir) => dir,
    };

    let out = place.join(PathBuf::from(format!(
        "{}/{}",
        imageboard.to_string(),
        tag_string
    )));
    debug!("Target dir: {}", out.display());
    Ok(out)
}

/// Most ImageBoard APIs have a common set of info from the files we want to download.
/// This struct is just a catchall model for the necessary parts of the post the program needs to properly download and save the files.
pub struct CommonPostItem {
    /// Direct URL of the original image file located inside the imageboard's server
    pub url: String,
    /// Instead of calculating the downloaded file's MD5 hash on the fly, it uses the one provided by the API and serves as the name of the downloaded file.
    pub md5: String,
    /// The original file extension provided by the imageboard.
    ///
    /// ```https://konachan.com``` or other imageboards based on MoeBooru doesn`t provide this field. So, additional work is required to get the file extension from the url
    pub ext: String,
}

impl CommonPostItem {
    /// Main routine to download posts.
    ///
    /// This method should be coupled into a function block to run inside a ```futures::stream``` in order to prevent issues with ```await```
    pub async fn get(
        &self,
        client: &Client,
        output: &Path,
        multi: Arc<MultiProgress>,
        main: Arc<ProgressBar>,
        variant: ImageBoards,
    ) -> Result<(), Error> {
        let output = output.join(format!("{}.{}", &self.md5, &self.ext));

        if Self::check_file_exists(self, &output, multi.clone(), main.clone())
            .await
            .is_ok()
        {
            Self::fetch(self, client, multi, main, &output, variant).await?
        }
        Ok(())
    }

    async fn check_file_exists(
        &self,
        output: &Path,
        multi_progress: Arc<MultiProgress>,
        main_bar: Arc<ProgressBar>,
    ) -> Result<(), Error> {
        if output.exists() {
            let file_digest = compute(read(&output).await?);
            let hash = format!("{:x}", file_digest);
            if hash != self.md5 {
                fs::remove_file(&output).await?;
                multi_progress.println(format!(
                    "File {}.{} is corrupted. Re-downloading...",
                    &self.md5, &self.ext
                ))?;
                Ok(())
            } else {
                multi_progress.println(format!(
                    "File {}.{} already exists. Skipping.",
                    &self.md5, &self.ext
                ))?;
                main_bar.inc(1);
                bail!("")
            }
        } else {
            Ok(())
        }
    }

    async fn fetch(
        &self,
        client: &Client,
        multi: Arc<MultiProgress>,
        main: Arc<ProgressBar>,
        output: &Path,
        variant: ImageBoards,
    ) -> Result<(), Error> {
        debug!("Fetching {}", &self.url);
        let res = client.get(&self.url).send().await?;

        let size = res.content_length().unwrap_or_default();
        let bar =
            ProgressBar::new(size).with_style(download_progress_style(variant.progress_template()));
        bar.set_draw_target(ProgressDrawTarget::stderr_with_hz(60));

        let pb = multi.add(bar);

        debug!("Creating destination file {:?}", &output);
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(output)
            .await?;

        // Download the file chunk by chunk.
        debug!("Retrieving chunks...");
        let mut stream = res.bytes_stream();
        while let Some(item) = stream.next().await {
            // Retrieve chunk.
            let mut chunk = match item {
                Ok(chunk) => chunk,
                Err(e) => {
                    bail!(e)
                }
            };
            pb.inc(chunk.len() as u64);

            // Write to file.
            match file.write_all_buf(&mut chunk).await {
                Ok(_res) => (),
                Err(e) => {
                    bail!(e);
                }
            };
        }
        pb.finish_and_clear();

        main.inc(1);
        Ok(())
    }
}
