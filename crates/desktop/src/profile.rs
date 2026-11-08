//! Profiles (.glidedesk), connection history, and known host fingerprints. The history and
//! the fingerprints change in memory; `save` writes them.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Profile {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub rdp: Option<bool>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fullscreen: Option<bool>,
    pub mute: Option<bool>,
    pub software_decode: Option<bool>,
    pub gpu: Option<String>,
    pub fingerprint: Option<String>,
    pub codec: Option<crate::CodecChoice>,
    pub clipboard: Option<bool>,
}

impl Profile {
    pub fn load_file(path: &std::path::Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading profile {}", path.display()))?;
        let p: Profile =
            toml::from_str(&text).with_context(|| format!("parsing profile {}", path.display()))?;
        Ok(p)
    }

    pub fn save_file(&self, path: &std::path::Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing profile")?;
        fs::write(path, text).with_context(|| format!("writing profile {}", path.display()))?;
        Ok(())
    }
}

pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| ".".into())
            .join("glidedesk")
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| ".".into())
            .join("glidedesk")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct History {
    pub hosts: Vec<String>,
    pub last_user: Option<String>,
}

impl History {
    pub fn load() -> Self {
        let file = config_dir().join("history.toml");
        fs::read_to_string(file)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn add(&mut self, host: &str, user: Option<&str>) {
        if host.is_empty() {
            return;
        }
        self.hosts.retain(|h| h != host);
        self.hosts.insert(0, host.to_owned());
        if self.hosts.len() > 10 {
            self.hosts.truncate(10);
        }
        if let Some(u) = user {
            self.last_user = Some(u.to_owned());
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        fs::create_dir_all(&dir)?;
        let text = toml::to_string_pretty(self)?;
        fs::write(dir.join("history.toml"), text)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KnownHosts {
    #[serde(default)]
    pub hosts: std::collections::HashMap<String, String>,
}

impl KnownHosts {
    pub fn load() -> Self {
        let file = config_dir().join("known_hosts.toml");
        fs::read_to_string(file)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn get(&self, host: &str, port: u16) -> Option<[u8; 32]> {
        let key = format!("{host}:{port}");
        let hex_str = self.hosts.get(&key)?;
        let bytes = hex::decode(hex_str).ok()?;
        if bytes.len() == 32 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            Some(arr)
        } else {
            None
        }
    }

    pub fn set(&mut self, host: &str, port: u16, fp: &[u8; 32]) {
        let key = format!("{host}:{port}");
        self.hosts.insert(key, hex::encode(fp));
    }

    pub fn forget(&mut self, host: &str, port: u16) {
        let key = format!("{host}:{port}");
        self.hosts.remove(&key);
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        fs::create_dir_all(&dir)?;
        let text = toml::to_string_pretty(self)?;
        fs::write(dir.join("known_hosts.toml"), text)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_roundtrip() {
        let p = Profile {
            host: "test.local".into(),
            user: Some("pablo".into()),
            port: Some(3391),
            rdp: Some(false),
            width: Some(1920),
            height: Some(1080),
            fullscreen: Some(true),
            mute: Some(false),
            software_decode: Some(false),
            gpu: Some("nvidia".into()),
            codec: Some(crate::CodecChoice::Hevc),
            clipboard: Some(false),
            fingerprint: Some(
                "eeeecd4bc30d461bd13e236cb674a734b3f2198ccf60ccfac11fe740c4ee9573".into(),
            ),
        };

        let temp_dir = std::env::temp_dir().join(format!("glidedesk-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let file = temp_dir.join("test.glidedesk");

        p.save_file(&file).expect("save profile");
        let loaded = Profile::load_file(&file).expect("load profile");

        assert_eq!(loaded.host, "test.local");
        assert_eq!(loaded.user.as_deref(), Some("pablo"));
        assert_eq!(loaded.port, Some(3391));
        assert_eq!(loaded.width, Some(1920));
        assert_eq!(loaded.height, Some(1080));
        assert_eq!(loaded.gpu.as_deref(), Some("nvidia"));
        assert_eq!(loaded.codec, Some(crate::CodecChoice::Hevc));
        assert_eq!(loaded.clipboard, Some(false));
        assert_eq!(
            loaded.fingerprint.as_deref(),
            Some("eeeecd4bc30d461bd13e236cb674a734b3f2198ccf60ccfac11fe740c4ee9573")
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_known_hosts_hex() {
        let mut kh = KnownHosts::default();
        let fp: [u8; 32] = [42u8; 32];
        let key = "192.168.1.136:3391";
        kh.hosts.insert(key.into(), hex::encode(fp));

        let retrieved = kh.get("192.168.1.136", 3391);
        assert_eq!(retrieved, Some(fp));

        kh.forget("192.168.1.136", 3391);
        assert!(kh.get("192.168.1.136", 3391).is_none());
    }

    #[test]
    fn test_history_truncate() {
        let mut h = History::default();
        for i in 1..=15 {
            h.add(&format!("host{}", i), Some("user"));
        }
        assert_eq!(h.hosts.len(), 10);
        assert_eq!(h.hosts[0], "host15");
    }
}
