//! `rscap` — enregistreur d'ecran temps reel.
//!
//! Voir `docs/PIPELINE.md` pour l'architecture detaillee.

pub mod bench;
pub mod capture;
pub mod config;
pub mod encoder;
pub mod error;
pub mod muxer;
pub mod performance;
pub mod pipeline;
pub mod timing;
pub mod ui;

pub use config::Config;
pub use error::{RecorderError, Result};
