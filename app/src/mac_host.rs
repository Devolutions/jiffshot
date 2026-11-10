//! Access to the Rustolonia host used by macOS integrations.

use rustolonia_sys as sys;
use std::io;

/// Loads the host whose ABI fingerprint matches the pinned Rustolonia bindings.
pub(crate) fn open_application() -> io::Result<(sys::Host, sys::ComPtr<sys::IAvnApplication>)> {
    let host = if sys::STATIC_HOST {
        sys::Host::load_static()
    } else {
        let path = rustolonia::discover_host_path().map_err(io::Error::other)?;
        sys::Host::load(path)
    }.map_err(io::Error::other)?;
    let application = host
        .activation_factory()
        .and_then(|factory| factory.create_application())
        .map_err(|error| io::Error::other(format!("could not initialize the Rustolonia application ABI: {error}")))?;
    Ok((host, application))
}

pub(crate) fn check_invalid_token_rejected(result: sys::Result<()>, capability: &str) -> io::Result<()> {
    match result {
        Err(error) if error.0 == sys::E_INVALIDARG => Ok(()),
        Err(error) => Err(io::Error::other(format!("{capability} probe returned {error} instead of rejecting an invalid token"))),
        Ok(()) => Err(io::Error::other(format!("{capability} probe unexpectedly accepted an invalid token"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_probe_requires_an_invalid_token_error() {
        assert!(check_invalid_token_rejected(Err(sys::Error(sys::E_INVALIDARG)), "test").is_ok());
        assert!(check_invalid_token_rejected(Err(sys::Error(sys::E_FAIL)), "test").is_err());
        assert!(check_invalid_token_rejected(Ok(()), "test").is_err());
    }
}
