//! Modules that work by parsing post info from a imageboard API into a list of [Posts](crate::imageboards::post).
//! # Extractors
//!
//! All modules implementing [Extractor] work by connecting to a imageboard website, searching for posts with the tags supplied and parsing all of them into a [PostQueue](crate::imageboards::post::PostQueue).
use std::fmt::Display;

use self::error::ExtractorError;
use super::post::PostQueue;
use async_trait::async_trait;
use reqwest::Client;

pub mod danbooru;

pub mod e621;

pub mod gelbooru;

pub mod moebooru;

mod error;

#[cfg(feature = "global_blacklist")]
pub mod blacklist;

/// This trait should be the only common public interface all extractors should expose aside from some other website-specific configuration.
#[async_trait]
pub trait Extractor {
    /// Sets up the extractor unit with the tags supplied.
    ///
    /// Will ignore `safe_mode` state if the imageboard doesn't have a safe variant.
    fn new<S>(tags: &[S], safe_mode: bool, disable_blacklist: bool) -> Self
    where
        S: ToString + Display;

    /// Searches the tags list on a per-page way. It's relatively the fastest way, but subject to slowdowns since it needs
    /// to iter through all pages manually in order to fetch all posts.
    async fn search(&mut self, page: usize) -> Result<PostQueue, ExtractorError>;

    /// Searches all posts from all pages with given tags, it's the most pratical one, but slower on startup since it will search all pages by itself until it finds no more posts.
    async fn full_search(
        &mut self,
        start_page: Option<usize>,
        limit: Option<usize>,
    ) -> Result<PostQueue, ExtractorError>;

    /// Consumes `self` and returns the used client for external use.
    fn client(self) -> Client;

    /// Get the total number of removed files by the internal blacklist.
    fn total_removed(&self) -> u64;
}

/// Authentication capability for imageboard websites. Implies the Extractor is able to use a user-defined blacklist
#[async_trait]
pub trait Auth {
    /// Setting to `true` will prompt the user for username and API key, while setting to `false` will silently try to authenticate.
    ///
    /// Does nothing if auth was never configured.
    async fn auth(&mut self, prompt: bool) -> Result<(), ExtractorError>;
}
