//! Literal environment precedence, separate from Cargo's child-process [env].

use std::collections::BTreeMap;

use crate::config::execution::supported_environment_key;
use crate::limits;

use super::Failure;

/// Private bounded effective environment. Values must not enter diagnostics.
#[derive(Clone)]
pub struct Environment {
    values: BTreeMap<String, String>,
    origins: BTreeMap<String, Origin>,
}

/// Safe provenance categories with no raw value or path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    /// Protected inherited launcher declaration.
    Protected,
    /// Explicit client environment field.
    Client,
    /// Literal leading shell assignment.
    Assignment,
    /// Literal env wrapper assignment, unset or clear.
    Wrapper,
}

/// Validate one literal environment name before treating it as assignment data.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index != 0 && byte.is_ascii_digit())
        })
}

impl Environment {
    /// Start from the protected inherited environment, never ambient hook data.
    ///
    /// # Errors
    /// Unknown keys and overflowing declarations fail conservatively.
    pub fn inherited(values: &BTreeMap<String, String>) -> Result<Self, Failure> {
        let mut environment = Self {
            values: BTreeMap::new(),
            origins: BTreeMap::new(),
        };
        for (key, value) in values {
            environment.set(key, Some(value), Origin::Protected)?;
        }
        Ok(environment)
    }

    /// Apply one literal assignment or unset with its distinct origin.
    ///
    /// # Errors
    /// Unknown path/executable mechanisms, NUL and overflow deny.
    pub fn set(&mut self, key: &str, value: Option<&str>, origin: Origin) -> Result<(), Failure> {
        if !valid_name(key) || !supported_environment_key(key) {
            return Err(Failure::Environment);
        }
        if value.is_some_and(|value| value.len() > limits::PATH_BYTES || value.contains('\0')) {
            return Err(Failure::Limit);
        }
        if let Some(value) = value {
            self.values.insert(key.to_owned(), value.to_owned());
        } else {
            self.values.remove(key);
        }
        self.origins.insert(key.to_owned(), origin);
        if self.values.len() > limits::EXECUTION_ENVIRONMENT
            || self.origins.len() > limits::EXECUTION_ENVIRONMENT
        {
            return Err(Failure::Limit);
        }
        Ok(())
    }

    /// Model env -i explicitly; removed keys remain represented in provenance.
    pub fn clear(&mut self) {
        self.values.clear();
        for origin in self.origins.values_mut() {
            *origin = Origin::Wrapper;
        }
    }

    /// Read private effective data for static path resolution.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Effective private map, for domain-separated provenance hashing.
    #[must_use]
    pub fn values(&self) -> &BTreeMap<String, String> {
        &self.values
    }

    /// Safe origin of a present or explicitly removed key.
    #[must_use]
    pub fn origin(&self, key: &str) -> Option<Origin> {
        self.origins.get(key).copied()
    }
}
