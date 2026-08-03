use alvr_common::{error, info};
use app_dirs2::{AppDataType, AppInfo};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::{env, fs, path::PathBuf};

/// Overrides the config directory, so that several client instances can run on one machine with
/// separate identities. Used by the mock client for multi-device testing; unset for real clients.
const CONFIG_DIR_ENV_VAR: &str = "ALVR_CLIENT_CONFIG_DIR";

/// Overrides the hostname that identifies this client to the server. The server keys clients by
/// hostname, so each concurrent instance needs a distinct one.
const HOSTNAME_ENV_VAR: &str = "ALVR_CLIENT_HOSTNAME";

fn config_path() -> PathBuf {
    if let Ok(dir) = env::var(CONFIG_DIR_ENV_VAR) {
        let dir = PathBuf::from(dir);
        fs::create_dir_all(&dir).ok();

        return dir.join("session.json");
    }

    app_dirs2::app_root(
        AppDataType::UserConfig,
        &AppInfo {
            name: "ALVR Client",
            author: "ALVR",
        },
    )
    .unwrap()
    .join("session.json")
}

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub hostname: String,
    pub protocol_id: String,
}

impl Default for Config {
    fn default() -> Self {
        let mut rng = rand::rng();

        Self {
            hostname: format!(
                "{}{}{}{}.client.local.",
                rng.random_range(0..10),
                rng.random_range(0..10),
                rng.random_range(0..10),
                rng.random_range(0..10),
            ),
            protocol_id: alvr_common::protocol_id(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let mut config = Self::load_stored();

        // Applied after loading so it wins over whatever is on disk.
        if let Ok(hostname) = env::var(HOSTNAME_ENV_VAR) {
            info!("Overriding client hostname with {hostname}");
            config.hostname = hostname;
        }

        config
    }

    fn load_stored() -> Self {
        if let Ok(config_string) = fs::read_to_string(config_path()) {
            // Failure happens if the Config signature changed between versions.
            // todo: recover data from mismatched Config signature. low priority
            if let Ok(config) = serde_json::from_str(&config_string) {
                return config;
            } else {
                info!("Error parsing ALVR config. Using default");
            }
        } else {
            info!("Error reading ALVR config. Using default");
        }

        let config = Config::default();
        config.store();

        config
    }

    pub fn store(&self) {
        let config_string = serde_json::to_string(self).unwrap();
        if let Err(e) = fs::write(config_path(), config_string) {
            error!("Error writing ALVR config: {e}")
        }
    }
}
