use jiffshot_control::{Error, ErrorCode};
use serde_json::{Map, Value};
use std::path::PathBuf;

pub(crate) type Reply = Result<Value, Error>;

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidRequest, message)
}

pub(crate) fn not_found(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::NotFound, message)
}

pub(crate) fn internal(message: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Internal, message.to_string())
}

pub(crate) struct Args(pub(crate) Map<String, Value>);

impl Args {
    pub(crate) fn new(value: Value) -> Result<Self, Error> {
        match value {
            Value::Null => Ok(Self(Map::new())),
            Value::Object(map) => Ok(Self(map)),
            _ => Err(invalid("arguments must be a JSON object")),
        }
    }

    pub(crate) fn only(&self, allowed: &[&str]) -> Result<(), Error> {
        match self.0.keys().find(|key| !allowed.contains(&key.as_str())) {
            None => Ok(()),
            Some(key) if allowed.is_empty() => Err(invalid(format!(
                "unexpected argument {key:?}: this command takes none"
            ))),
            Some(key) => Err(invalid(format!(
                "unexpected argument {key:?}; expected one of: {}",
                allowed.join(", ")
            ))),
        }
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.0.get(key).is_some_and(|value| !value.is_null())
    }

    pub(crate) fn string(&self, key: &str) -> Result<Option<String>, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(value @ (Value::Number(_) | Value::Bool(_))) => Ok(Some(value.to_string())),
            Some(_) => Err(invalid(format!("{key} must be text"))),
        }
    }

    pub(crate) fn flag(&self, key: &str) -> Result<Option<bool>, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(Value::String(value)) if matches!(value.as_str(), "true" | "1" | "yes") => {
                Ok(Some(true))
            }
            Some(Value::String(value)) if matches!(value.as_str(), "false" | "0" | "no") => {
                Ok(Some(false))
            }
            Some(Value::Number(value)) => Ok(Some(value.as_f64() != Some(0.0))),
            Some(_) => Err(invalid(format!("{key} must be true or false"))),
        }
    }

    pub(crate) fn bool(&self, key: &str) -> Result<bool, Error> {
        Ok(self.flag(key)?.unwrap_or(false))
    }

    pub(crate) fn number(&self, key: &str) -> Result<Option<f64>, Error> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(value)) => Ok(value.as_f64()),
            Some(Value::String(value)) => value
                .trim()
                .parse()
                .map(Some)
                .map_err(|_| invalid(format!("{key} must be a number, not {value:?}"))),
            Some(_) => Err(invalid(format!("{key} must be a number"))),
        }
    }

    pub(crate) fn int(&self, key: &str, min: i64, max: i64) -> Result<Option<i64>, Error> {
        match self.number(key)? {
            None => Ok(None),
            Some(value) if value.fract() == 0.0 && (min as f64..=max as f64).contains(&value) => {
                Ok(Some(value as i64))
            }
            Some(value) => Err(invalid(format!(
                "{key} must be a whole number from {min} to {max}, not {value}"
            ))),
        }
    }

    pub(crate) fn out(&self, key: &str, extensions: &[&str]) -> Result<Option<PathBuf>, Error> {
        let Some(text) = self.string(key)? else {
            return Ok(None);
        };
        let path = PathBuf::from(&text);
        if !path.is_absolute() {
            return Err(invalid(format!(
                "{key} must be an absolute path (the CLI makes paths absolute), not {text:?}"
            )));
        }
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !extensions.contains(&extension.as_str()) {
            return Err(invalid(format!(
                "{key} must end in .{}",
                extensions.join(" or .")
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| invalid(format!("cannot create {}: {error}", parent.display())))?;
        }
        Ok(Some(path))
    }
}
