use std::{collections::HashMap, sync::Arc};

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PreferenceError {
    #[error("invalid_preferences")]
    InvalidPreferences,
}

#[uniffi::export]
pub fn public_contract() -> String {
    park_domain::PUBLIC_CONTRACT.to_owned()
}

#[uniffi::export]
pub fn discovery_domains(order: Vec<String>, dimension: String, media: String) -> Vec<String> {
    park_domain::settings::discovery_domains(&order, &dimension, &media)
}

#[uniffi::export]
pub fn normalized_search_text(text: String) -> String {
    park_domain::normalized_search_text(&text)
}

#[derive(uniffi::Object)]
pub struct CopyCatalog(park_domain::LocalizedCopy);

#[derive(uniffi::Object)]
pub struct SearchDictionary(park_domain::TagDictionary);

#[uniffi::export]
impl SearchDictionary {
    #[uniffi::constructor]
    pub fn new(bytes: Vec<u8>, source: String, locale: String) -> Result<Arc<Self>, PreferenceError> {
        park_domain::TagDictionary::parse(&bytes, &source, &locale)
            .map(|dictionary| Arc::new(Self(dictionary))).ok_or(PreferenceError::InvalidPreferences)
    }

    pub fn suggestions(&self, query: String) -> String {
        self.0.suggestions(&query).to_string()
    }
    pub fn translation(&self, name: String, namespace: String) -> Option<String> {
        self.0.translation(&name, &namespace).map(str::to_owned)
    }
}

#[uniffi::export]
impl CopyCatalog {
    pub fn locale(&self) -> String {
        self.0.locale().to_owned()
    }
    #[uniffi::constructor]
    pub fn new(locale: String, plugin_json: Option<String>) -> Result<Arc<Self>, PreferenceError> {
        let plugin = plugin_json
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|_| PreferenceError::InvalidPreferences)?
            .unwrap_or_default();
        Ok(Arc::new(Self(park_domain::LocalizedCopy::new(
            &locale, plugin,
        ))))
    }
    pub fn text(&self, key: String, params: HashMap<String, String>) -> String {
        self.0.text(&key, &params)
    }
}

#[derive(uniffi::Object)]
pub struct ContentFilter(park_domain::BlockFilter);

#[uniffi::export]
impl ContentFilter {
    #[uniffi::constructor]
    pub fn new(settings_json: String) -> Result<Arc<Self>, PreferenceError> {
        let settings = serde_json::from_str(&settings_json)
            .map_err(|_| PreferenceError::InvalidPreferences)?;
        Ok(Arc::new(Self(park_domain::BlockFilter::from_settings(
            &settings,
        ))))
    }
    pub fn first_match(&self, title: String, tags: Vec<String>) -> Option<String> {
        self.0.first_match(&title, &tags)
    }
}
