//! Server state and router composition.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool_handler};

use crate::config::{Config, file_mtime};
use crate::jwt::TokenCache;

pub const INSTRUCTIONS: &str = "Apple online services. Tool families: itunes_*/apple_* (public, no credentials), \
asc_* + notary_* (App Store Connect API), storekit_* (App Store Server API), music_* (Apple Music API), \
weather_* (WeatherKit), maps_* (Apple Maps Server API), cloudkit_* (CloudKit Web Services), apns_send (push), \
siwa_* (Sign in with Apple), abm_* (Apple Business/School Manager), searchads_* (Apple Search Ads), news_* (Apple News Publisher), \
icloud_* (iCloud Calendar/Contacts via CalDAV/CardDAV and Mail via IMAP/SMTP). \
Credentials live in ~/.config/apple-mcp/config.json (see apple_config_template / apple_status). \
Every family has a generic *_request tool for endpoints without a dedicated tool. \
Responses are JSON, trimmed to a size cap; narrow with filters/limits when truncated.";

pub struct AppleServer {
    pub config: RwLock<Config>,
    pub http: reqwest::Client,
    pub tokens: TokenCache,
    /// Discovered WebDAV home URLs (caldav_home, carddav_home, carddav_book).
    pub dav: Mutex<HashMap<String, String>>,
    tool_router: ToolRouter<Self>,
}

impl AppleServer {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("apple-mcp/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .expect("reqwest client");
        Self {
            config: RwLock::new(Config::load()),
            http,
            tokens: TokenCache::default(),
            dav: Mutex::new(HashMap::new()),
            tool_router: Self::meta_router()
                + Self::public_router()
                + Self::asc_router()
                + Self::storekit_router()
                + Self::music_router()
                + Self::weather_router()
                + Self::maps_router()
                + Self::cloudkit_router()
                + Self::apns_router()
                + Self::siwa_router()
                + Self::abm_router()
                + Self::searchads_router()
                + Self::news_router()
                + Self::icloud_dav_router()
                + Self::icloud_mail_router(),
        }
    }

    /// Snapshot of the current configuration. The config file is re-read automatically when it
    /// changes on disk (mtime/path check), so external writers (the `setup` CLI, an editor) take
    /// effect without `apple_reload_config` or an MCP reconnect.
    pub fn cfg(&self) -> Config {
        let path = Config::default_path();
        let mtime = file_mtime(&path);
        {
            let guard = self.config.read().expect("config lock");
            if guard.path == path && guard.mtime == mtime {
                return guard.clone();
            }
        }
        tracing::info!("config file changed on disk ({}); reloading", path.display());
        self.reload_config()
    }

    /// Force a reload of the config file, dropping cached tokens and discovered iCloud URLs.
    pub fn reload_config(&self) -> Config {
        let fresh = Config::load();
        self.tokens.clear();
        self.dav.lock().expect("dav cache").clear();
        let mut guard = self.config.write().expect("config lock");
        *guard = fresh;
        guard.clone()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AppleServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = Implementation::from_build_env();
        info.name = "apple-mcp".into();
        info.version = env!("CARGO_PKG_VERSION").into();
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(info)
            .with_instructions(INSTRUCTIONS)
    }
}
