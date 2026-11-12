//! The session token in the system keychain, one item per server.
//!
//! On macOS the token is a generic password in the login keychain (service
//! [`SERVICE`], account the server URL). Elsewhere there is no keychain yet:
//! [`set`] fails, and [`crate::persist`] keeps the token in its file as before.

use anyhow::Result;

/// The keychain item's service, as Keychain Access shows it under Name.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SERVICE: &str = "mtc";

/// The token saved for `server`, or `None` if there is none.
pub fn get(server: &str) -> Result<Option<String>> {
    imp::get(server)
}

/// Save `token` for `server`, replacing the one saved before.
pub fn set(server: &str, token: &str) -> Result<()> {
    imp::set(server, token)
}

/// Forget the token for `server`. Forgetting one that isn't saved succeeds.
pub fn delete(server: &str) -> Result<()> {
    imp::delete(server)
}

#[cfg(all(target_os = "macos", not(test)))]
mod imp {
    use anyhow::{Context as _, Result};
    use security_framework::passwords;

    use super::SERVICE;

    /// `errSecItemNotFound`
    const NOT_FOUND: i32 = -25300;

    pub fn get(server: &str) -> Result<Option<String>> {
        match passwords::get_generic_password(SERVICE, server) {
            Ok(bytes) => Ok(Some(
                String::from_utf8(bytes).context("the saved token isn’t text")?,
            )),
            Err(err) if err.code() == NOT_FOUND => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn set(server: &str, token: &str) -> Result<()> {
        Ok(passwords::set_generic_password(
            SERVICE,
            server,
            token.as_bytes(),
        )?)
    }

    pub fn delete(server: &str) -> Result<()> {
        match passwords::delete_generic_password(SERVICE, server) {
            Err(err) if err.code() != NOT_FOUND => Err(err.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(all(not(target_os = "macos"), not(test)))]
mod imp {
    use anyhow::{Result, bail};

    pub fn get(_server: &str) -> Result<Option<String>> {
        Ok(None)
    }

    pub fn set(_server: &str, _token: &str) -> Result<()> {
        bail!("no system keychain on this platform")
    }

    pub fn delete(_server: &str) -> Result<()> {
        Ok(())
    }
}

/// Tests keep the keychain in memory. A server whose name contains
/// `unavailable` behaves like a keychain that refuses every request.
#[cfg(test)]
pub(crate) mod imp {
    use std::{
        collections::HashMap,
        sync::{LazyLock, Mutex},
    };

    use anyhow::{Result, bail};

    static ITEMS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

    fn check(server: &str) -> Result<()> {
        if server.contains("unavailable") {
            bail!("the keychain refused");
        }
        Ok(())
    }

    pub fn get(server: &str) -> Result<Option<String>> {
        check(server)?;
        Ok(ITEMS.lock().unwrap().get(server).cloned())
    }

    pub fn set(server: &str, token: &str) -> Result<()> {
        check(server)?;
        ITEMS
            .lock()
            .unwrap()
            .insert(server.to_string(), token.to_string());
        Ok(())
    }

    pub fn delete(server: &str) -> Result<()> {
        check(server)?;
        ITEMS.lock().unwrap().remove(server);
        Ok(())
    }
}
