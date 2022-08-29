//! Post extractor for `https://danbooru.donmai.us`
//!
//! The danbooru extractor has the following features:
//! - Authentication
//! - Tag blacklist (defined in user profile page)
//! - Native safe mode (don't download NSFW posts)
//!
//! # Example basic usage
//!
//! ```rust
//! use imageboard_downloader::*;
//!
//! async fn fetch_posts() {
//!     let tags = ["umbreon".to_string(), "espeon".to_string()];
//!     
//!     let safe_mode = true; // Set to true to download posts from safebooru
//!
//!     let mut ext = DanbooruExtractor::new(&tags, safe_mode); // Initialize the extractor
//!
//!     ext.auth(true);
//!
//!     // Will iterate through all pages until it finds no more posts, then returns the list.
//!     let posts = ext.full_search().await.unwrap();
//!
//!     // Print all information collected
//!     println!("{:?}", posts);
//! }
//! ```
use super::error::ExtractorError;
use super::{Auth, Extractor};
use crate::imageboards::auth::{auth_prompt, ImageboardConfig};
use crate::imageboards::post::{rating::Rating, Post, PostQueue};
use crate::imageboards::ImageBoards;
use crate::{client, join_tags, print_found};
use ahash::AHashSet;
use async_trait::async_trait;
use cfg_if::cfg_if;
use colored::Colorize;
use log::debug;
use reqwest::Client;
use serde_json::Value;
use std::io::{self, Write};
use tokio::time::Instant;

#[cfg(feature = "global_blacklist")]
use super::blacklist::GlobalBlacklist;

/// Main object to download posts
#[derive(Debug)]
pub struct DanbooruExtractor {
    client: Client,
    tags: Vec<String>,
    tag_string: String,
    pub auth_state: bool,
    pub auth: ImageboardConfig,
    safe_mode: bool,
    disable_blacklist: bool,
    total_removed: u64,
}

#[async_trait]
impl Extractor for DanbooruExtractor {
    fn new(tags: &[String], safe_mode: bool, disable_blacklist: bool) -> Self {
        // Use common client for all connections with a set User-Agent
        let client = client!(ImageBoards::Danbooru.user_agent());

        // Merge all tags in the URL format
        let tag_string = join_tags!(tags);

        // Set Safe mode status
        let safe_mode = safe_mode;

        Self {
            client,
            tags: tags.to_vec(),
            tag_string,
            auth_state: false,
            auth: Default::default(),
            safe_mode,
            disable_blacklist,
            total_removed: 0,
        }
    }

    async fn search(&mut self, page: usize) -> Result<PostQueue, ExtractorError> {
        Self::validate_tags(self).await?;

        let posts = Self::get_post_list(self, page).await?;

        let qw = PostQueue {
            posts,
            tags: self.tags.to_vec(),
        };

        Ok(qw)
    }

    async fn full_search(
        &mut self,
        start_page: Option<usize>,
        limit: Option<usize>,
    ) -> Result<PostQueue, ExtractorError> {
        Self::validate_tags(self).await?;

        let mut fvec = Vec::new();

        let mut page = 1;

        loop {
            let position = if let Some(n) = start_page {
                page + n
            } else {
                page
            };

            debug!("Scanning page {}", position);

            let mut posts = Self::get_post_list(self, position).await?;
            let size = posts.len();

            if size == 0 {
                break;
            }

            if !self.disable_blacklist {
                self.blacklist_filter(&mut posts).await?;
            }

            fvec.extend(posts);

            if let Some(num) = limit {
                if fvec.len() >= num {
                    break;
                }
            }

            if page == 100 {
                break;
            }

            page += 1;

            print_found!(fvec);
        }
        println!();

        let fin = PostQueue {
            posts: fvec,
            tags: self.tags.to_vec(),
        };
        Ok(fin)
    }
}

#[async_trait]
impl Auth for DanbooruExtractor {
    async fn auth(&mut self, prompt: bool) -> Result<(), ExtractorError> {
        auth_prompt(prompt, ImageBoards::Danbooru, &self.client).await?;

        if let Some(creds) = ImageBoards::Danbooru.read_config_from_fs().await? {
            self.auth = creds;
            self.auth_state = true;
            return Ok(());
        }

        self.auth_state = false;
        Ok(())
    }
}

impl DanbooruExtractor {
    async fn validate_tags(&self) -> Result<(), ExtractorError> {
        if self.tags.len() > 2 {
            return Err(ExtractorError::TooManyTags {
                current: self.tags.len(),
                max: 2,
            });
        };

        let count_endpoint = format!(
            "{}?tags={}",
            ImageBoards::Danbooru
                .post_count_url(self.safe_mode)
                .unwrap(),
            &self.tag_string
        );

        // Get an estimate of total posts and pages to search
        let request = if self.auth_state {
            debug!("[AUTH] Validating tags");
            self.client
                .get(count_endpoint)
                .basic_auth(&self.auth.username, Some(&self.auth.api_key))
        } else {
            debug!("Validating tags");
            self.client.get(count_endpoint)
        };

        let count = request.send().await?.json::<Value>().await?;

        if let Some(count) = count["counts"]["posts"].as_u64() {
            // Bail out if no posts are found
            if count == 0 {
                return Err(ExtractorError::ZeroPosts);
            }

            debug!("Found {} posts", count);
            Ok(())
        } else {
            Err(ExtractorError::InvalidServerResponse)
        }
    }

    #[inline]
    async fn blacklist_filter(&mut self, list: &mut Vec<Post>) -> Result<(), ExtractorError> {
        let original_size = list.len();
        let blacklist = &self.auth.user_data.blacklisted_tags;
        let mut removed = 0;

        let start = Instant::now();
        if !blacklist.is_empty() {
            list.retain(|c| !c.tags.iter().any(|s| blacklist.contains(s)));

            let bp = original_size - list.len();
            debug!("User blacklist removed {} posts", bp);
            removed += bp as u64;
        }

        cfg_if! {
            if #[cfg(feature = "global_blacklist")] {
                let gbl = GlobalBlacklist::get().await?;

                if let Some(tags) = gbl.blacklist {
                    if !tags.global.is_empty() {
                        let fsize = list.len();
                        debug!("Removing posts with tags [{:?}]", tags);
                        list.retain(|c| !c.tags.iter().any(|s| tags.global.contains(s)));

                        let bp = fsize - list.len();
                        debug!("Global blacklist removed {} posts", bp);
                        removed += bp as u64;
                    } else {
                        debug!("Global blacklist is empty")
                    }

                    if !tags.danbooru.is_empty() {
                        let fsize = list.len();
                        debug!("Removing posts with tags [{:?}]", tags.danbooru);
                        list.retain(|c| !c.tags.iter().any(|s| tags.danbooru.contains(s)));

                        let bp = fsize - list.len();
                        debug!("Danbooru blacklist removed {} posts", bp);
                        removed += bp as u64;
                    }
                }
            }
        }
        let end = Instant::now();
        debug!("Blacklist filtering took {:?}", end - start);
        debug!("Removed {} blacklisted posts", removed);
        self.total_removed += removed;

        Ok(())
    }

    async fn get_post_list(&self, page: usize) -> Result<Vec<Post>, ExtractorError> {
        // Check safe mode
        let url_mode = format!(
            "{}?tags={}",
            ImageBoards::Danbooru.post_url(self.safe_mode).unwrap(),
            &self.tag_string
        );

        // Fetch item list from page
        let req = if self.auth_state {
            debug!("[AUTH] Fetching posts from page {}", page);
            self.client
                .get(url_mode)
                .query(&[("page", page), ("limit", 200)])
                .basic_auth(&self.auth.username, Some(&self.auth.api_key))
        } else {
            debug!("Fetching posts from page {}", page);
            self.client
                .get(url_mode)
                .query(&[("page", page), ("limit", 200)])
        };

        let post_array = req.send().await?.json::<Value>().await?;

        let start_point = Instant::now();
        let posts: Vec<Post> = post_array
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["file_url"].as_str().is_some())
            .map(|c| {
                let mut tag_list = AHashSet::new();

                for i in c["tag_string"].as_str().unwrap().split(' ') {
                    tag_list.insert(i.to_string());
                }

                let rt = c["rating"].as_str().unwrap();
                let rating = if rt == "s" {
                    Rating::Questionable
                } else {
                    Rating::from_str(rt)
                };

                Post {
                    id: c["id"].as_u64().unwrap(),
                    md5: c["md5"].as_str().unwrap().to_string(),
                    url: c["file_url"].as_str().unwrap().to_string(),
                    extension: c["file_ext"].as_str().unwrap().to_string(),
                    tags: tag_list,
                    rating,
                }
            })
            .collect();
        let end_iter = Instant::now();

        debug!("List size: {}", posts.len());
        debug!("Post mapping took {:?}", end_iter - start_point);
        Ok(posts)
    }
}
