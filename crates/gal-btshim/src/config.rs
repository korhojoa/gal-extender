// SPDX-License-Identifier: GPL-3.0-only
//! Configuration is read during initialization, never by a native callback.
use btsvc_core::aawg;
use std::net::Ipv4Addr;

/// Deliberately does not implement Debug: contains the AP password.
pub struct Config {
    pub ip: String,
    pub port: u16,
    pub ssid: String,
    pub key: String,
    pub bssid: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    TooLarge,
    InvalidLine,
    UnknownField,
    DuplicateField,
    MissingField,
    InvalidIp,
    InvalidPort,
    InvalidSsid,
    InvalidKey,
    InvalidBssid,
    PayloadTooLarge,
}

impl Config {
    /// Parse literal network credentials without logging their values.
    ///
    /// # Errors
    /// Returns the invalid, missing, duplicate or oversized field category.
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        if input.len() > 4096 {
            return Err(ConfigError::TooLarge);
        }
        let mut fields: [Option<&str>; 5] = [None; 5];
        for line in input.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let (name, value) = line.split_once('=').ok_or(ConfigError::InvalidLine)?;
            let index = match name.trim() {
                "ip" => 0,
                "port" => 1,
                "ssid" => 2,
                "key" => 3,
                "bssid" => 4,
                _ => return Err(ConfigError::UnknownField),
            };
            if fields[index].replace(value).is_some() {
                return Err(ConfigError::DuplicateField);
            }
        }
        let [ip, port, ssid, key, bssid] = fields.map(|v| v.ok_or(ConfigError::MissingField));
        let config = Self {
            ip: ip?.trim().to_owned(),
            port: port?.trim().parse().map_err(|_| ConfigError::InvalidPort)?,
            ssid: ssid?.to_owned(),
            key: key?.to_owned(),
            bssid: bssid?.trim().to_owned(),
        };
        config.validate()?;
        Ok(config)
    }

    /// # Errors
    /// Returns the invalid field category without including credential values.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let ip: Ipv4Addr = self.ip.parse().map_err(|_| ConfigError::InvalidIp)?;
        if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
            return Err(ConfigError::InvalidIp);
        }
        if self.port == 0 {
            return Err(ConfigError::InvalidPort);
        }
        if self.ssid.is_empty() || self.ssid.len() > 32 || self.ssid.contains('\0') {
            return Err(ConfigError::InvalidSsid);
        }
        if !((8..=63).contains(&self.key.len())
            && self.key.bytes().all(|b| (32..=126).contains(&b))
            || self.key.len() == 64 && self.key.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(ConfigError::InvalidKey);
        }
        let bytes = self.bssid.as_bytes();
        if bytes.len() != 17
            || !bytes.iter().enumerate().all(|(i, b)| {
                if i % 3 == 2 {
                    *b == b':'
                } else {
                    b.is_ascii_hexdigit()
                }
            })
        {
            return Err(ConfigError::InvalidBssid);
        }
        let mut payload = [0u8; 256];
        if aawg::encode_wifi_info_response(
            &mut payload,
            &self.ssid,
            &self.key,
            &self.bssid,
            aawg::SEC_WPA2_PERSONAL,
            aawg::AP_TYPE_DYNAMIC,
        )
        .is_none()
        {
            return Err(ConfigError::PayloadTooLarge);
        }
        Ok(())
    }

    #[must_use]
    pub fn as_core(&self) -> aawg::Cfg<'_> {
        aawg::Cfg {
            ip: &self.ip,
            port: self.port.into(),
            ssid: &self.ssid,
            key: &self.key,
            bssid: &self.bssid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const VALID: &str =
        "ip=198.51.100.1\nport=5288\nssid= AA AP \nkey= abc=def \nbssid=02:00:00:00:00:01\n";

    #[test]
    fn preserves_credentials_including_equals_and_spaces() {
        let c = Config::parse(&VALID.replace('\n', "\r\n")).unwrap();
        assert_eq!(c.key, " abc=def ");
        assert_eq!(c.ssid, " AA AP ");
    }

    #[test]
    fn rejects_malformed_config_without_echoing_values() {
        assert_eq!(
            Config::parse(&(VALID.to_owned() + "key=othersecret\n")).err(),
            Some(ConfigError::DuplicateField)
        );
        assert_eq!(
            Config::parse(&VALID.replace("5288", "0")).err(),
            Some(ConfigError::InvalidPort)
        );
        assert_eq!(
            Config::parse(&VALID.replace(" abc=def ", "short")).err(),
            Some(ConfigError::InvalidKey)
        );
        assert_eq!(
            Config::parse(&VALID.replace("02:00:00:00:00:01", "wrong")).err(),
            Some(ConfigError::InvalidBssid)
        );
        assert_eq!(
            Config::parse("ip=198.51.100.1").err(),
            Some(ConfigError::MissingField)
        );
        assert_eq!(
            Config::parse(&"x".repeat(4097)).err(),
            Some(ConfigError::TooLarge)
        );
    }
}
