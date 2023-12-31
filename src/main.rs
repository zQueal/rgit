#![deny(clippy::pedantic)]

use std::{
    borrow::Cow,
    fmt::{Display, Formatter},
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use askama::Template;
use axum::{
    body::Body,
    http,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Extension, Router,
};
use bat::assets::HighlightingAssets;
use clap::Parser;
use database::schema::{prefixes::TreePrefix, SCHEMA_VERSION};
use once_cell::sync::{Lazy, OnceCell};
use sha2::{digest::FixedOutput, Digest};
use sled::Db;
use syntect::html::ClassStyle;
use tokio::{
    signal::unix::{signal, SignalKind},
    sync::mpsc,
};
use tower_http::cors::CorsLayer;
use tower_layer::layer_fn;
use tracing::{error, info, instrument, warn};

use crate::{git::Git, layers::logger::LoggingMiddleware};

mod database;
mod git;
mod layers;
mod methods;
mod syntax_highlight;

const CRATE_VERSION: &str = clap::crate_version!();

static GLOBAL_CSS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/statics/css/style.css",));
static GLOBAL_CSS_HASH: Lazy<Box<str>> = Lazy::new(|| build_asset_hash(GLOBAL_CSS));

static HIGHLIGHT_CSS_HASH: OnceCell<Box<str>> = OnceCell::new();
static DARK_HIGHLIGHT_CSS_HASH: OnceCell<Box<str>> = OnceCell::new();

#[derive(Parser, Debug)]
#[clap(author, version, about)]
pub struct Args {
    /// Path to a directory in which the Sled database should be stored, will be created if it doesn't already exist
    ///
    /// The Sled database is very quick to generate, so this can be pointed to temporary storage
    #[clap(short, long, value_parser)]
    db_store: PathBuf,
    /// The socket address to bind to (eg. 0.0.0.0:3333)
    bind_address: SocketAddr,
    /// The path in which your bare Git repositories reside (will be scanned recursively)
    scan_path: PathBuf,
    /// Configures the metadata refresh interval (eg. "never" or "60s")
    #[clap(long, default_value_t = RefreshInterval::Duration(Duration::from_secs(300)))]
    refresh_interval: RefreshInterval,
}

#[derive(Debug, Clone, Copy)]
pub enum RefreshInterval {
    Never,
    Duration(Duration),
}

impl Display for RefreshInterval {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Never => write!(f, "never"),
            Self::Duration(s) => write!(f, "{}", humantime::format_duration(*s)),
        }
    }
}

impl FromStr for RefreshInterval {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "never" {
            Ok(Self::Never)
        } else if let Ok(v) = humantime::parse_duration(s) {
            Ok(Self::Duration(v))
        } else {
            Err("must be seconds, a human readable duration (eg. '10m') or 'never'")
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let args: Args = Args::parse();

    let subscriber = tracing_subscriber::fmt();
    #[cfg(debug_assertions)]
    let subscriber = subscriber.pretty();
    subscriber.init();

    let db = open_db(&args)?;

    let indexer_wakeup_task =
        run_indexer(db.clone(), args.scan_path.clone(), args.refresh_interval);

    let bat_assets = HighlightingAssets::from_binary();
    let syntax_set = bat_assets.get_syntax_set().unwrap().clone();

    let theme = bat_assets.get_theme("GitHub");
    let css = syntect::html::css_for_theme_with_class_style(theme, ClassStyle::Spaced).unwrap();
    let css = Box::leak(
        format!(r#"@media (prefers-color-scheme: light){{{css}}}"#)
            .into_boxed_str()
            .into_boxed_bytes(),
    );
    HIGHLIGHT_CSS_HASH.set(build_asset_hash(css)).unwrap();

    let dark_theme = bat_assets.get_theme("TwoDark");
    let dark_css =
        syntect::html::css_for_theme_with_class_style(dark_theme, ClassStyle::Spaced).unwrap();
    let dark_css = Box::leak(
        format!(r#"@media (prefers-color-scheme: dark){{{dark_css}}}"#)
            .into_boxed_str()
            .into_boxed_bytes(),
    );
    DARK_HIGHLIGHT_CSS_HASH
        .set(build_asset_hash(dark_css))
        .unwrap();

    let static_favicon = |content: &'static [u8]| {
        move || async move {
            let mut resp = Response::new(Body::from(content));
            resp.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("image/x-icon"),
            );
            resp
        }
    };

    let static_css = |content: &'static [u8]| {
        move || async move {
            let mut resp = Response::new(Body::from(content));
            resp.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/css"),
            );
            resp
        }
    };

    let app = Router::new()
        .route("/", get(methods::index::handle))
        .route(
            &format!("/style-{}.css", *GLOBAL_CSS_HASH),
            get(static_css(GLOBAL_CSS)),
        )
        .route(
            &format!("/highlight-{}.css", HIGHLIGHT_CSS_HASH.get().unwrap()),
            get(static_css(css)),
        )
        .route(
            &format!(
                "/highlight-dark-{}.css",
                DARK_HIGHLIGHT_CSS_HASH.get().unwrap()
            ),
            get(static_css(dark_css)),
        )
        .route(
            "/favicon.ico",
            get(static_favicon(include_bytes!("../statics/favicon.ico"))),
        )
        .fallback(methods::repo::service)
        .layer(layer_fn(LoggingMiddleware))
        .layer(Extension(Arc::new(Git::new(syntax_set))))
        .layer(Extension(db))
        .layer(Extension(Arc::new(args.scan_path)))
        .layer(CorsLayer::new());

    let server = axum::Server::bind(&args.bind_address)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>());

    tokio::select! {
        res = server => res.context("failed to run server"),
        res = indexer_wakeup_task => res.context("failed to run indexer"),
        _ = tokio::signal::ctrl_c() => {
            info!("Received ctrl-c, shutting down");
            Ok(())
        }
    }
}

fn open_db(args: &Args) -> Result<Db, anyhow::Error> {
    let db = sled::Config::default()
        .use_compression(true)
        .path(&args.db_store)
        .open()
        .context("Failed to open database")?;

    let needs_schema_regen = match db.get(TreePrefix::schema_version())? {
        Some(v) if v != SCHEMA_VERSION.as_bytes() => Some(Some(v)),
        Some(_) => None,
        None => Some(None),
    };

    if let Some(version) = needs_schema_regen {
        let old_version = version
            .as_deref()
            .map_or(Cow::Borrowed("unknown"), String::from_utf8_lossy);

        warn!("Clearing outdated database ({old_version} != {SCHEMA_VERSION})");

        db.clear()?;
        db.insert(TreePrefix::schema_version(), SCHEMA_VERSION)?;
    }

    Ok(db)
}

async fn run_indexer(
    db: Db,
    scan_path: PathBuf,
    refresh_interval: RefreshInterval,
) -> Result<(), tokio::task::JoinError> {
    let (indexer_wakeup_send, mut indexer_wakeup_recv) = mpsc::channel(10);

    std::thread::spawn(move || loop {
        info!("Running periodic index");
        crate::database::indexer::run(&scan_path, &db);
        info!("Finished periodic index");

        if indexer_wakeup_recv.blocking_recv().is_none() {
            break;
        }
    });

    tokio::spawn({
        let mut sighup = signal(SignalKind::hangup()).expect("could not subscribe to sighup");
        let build_sleeper = move || async move {
            match refresh_interval {
                RefreshInterval::Never => futures::future::pending().await,
                RefreshInterval::Duration(v) => tokio::time::sleep(v).await,
            };
        };

        async move {
            loop {
                tokio::select! {
                    _ = sighup.recv() => {},
                    () = build_sleeper() => {},
                }

                if indexer_wakeup_send.send(()).await.is_err() {
                    error!("Indexing thread has died and is no longer accepting wakeup messages");
                }
            }
        }
    })
    .await
}

#[must_use]
pub fn build_asset_hash(v: &[u8]) -> Box<str> {
    let mut hasher = sha2::Sha256::default();
    hasher.update(v);
    let mut out = hex::encode(hasher.finalize_fixed());
    out.truncate(10);
    Box::from(out)
}

#[instrument(skip(t))]
pub fn into_response<T: Template>(t: &T) -> Response {
    match t.render() {
        Ok(body) => {
            let headers = [(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static(T::MIME_TYPE),
            )];

            (headers, body).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
