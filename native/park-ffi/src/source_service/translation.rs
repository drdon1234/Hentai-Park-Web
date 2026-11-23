use super::*;
use serde_json::{json, Value};

#[uniffi::export]
impl SourceService {
    pub fn translate_text(
        &self,
        text: String,
        target: String,
        request: Arc<Request>,
    ) -> Result<String> {
        if text.trim().is_empty()
            || text.encode_utf16().count() > 1000
            || !park_domain::settings::valid("translateTargets", &json!([target]))
        {
            return Err(SourceFailure::Unsupported);
        }
        let host = "translate.googleapis.com";
        let mut url = Url::parse("https://translate.googleapis.com/translate_a/single").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("client", "gtx"),
            ("sl", "auto"),
            ("tl", &target),
            ("dt", "t"),
            ("q", &text),
        ]);
        let session = self
            .routes
            .sessions
            .partition("_translation", Purpose::Translation)
            .map_err(|_| SourceFailure::Storage)?;
        let response = self
            .routes
            .runtime
            .block_on(self.routes.transport.execute(
                &session,
                &[host.to_owned()],
                park_net::Request {
                    url,
                    method: "GET".parse().unwrap(),
                    headers: Default::default(),
                    body: None,
                    connect_address: None,
                    follow_redirects: false,
                },
                512_000,
                Duration::from_secs(12),
                &request.cancelled,
            ))
            .map_err(|err| match err {
                park_net::Error::Cancelled => SourceFailure::Cancelled,
                _ => SourceFailure::Unavailable,
            })?;
        if !response.status.is_success() {
            return Err(SourceFailure::Unavailable);
        }
        let body: Value =
            serde_json::from_slice(&response.body).map_err(|_| SourceFailure::Unavailable)?;
        let sections = body[0].as_array().ok_or(SourceFailure::Unavailable)?;
        let mut translated = String::new();
        for section in sections {
            // The service can append metadata-only rows to a sentence list.
            if let Some(part) = section[0].as_str() {
                translated.push_str(part);
            }
        }
        let translated = translated.trim();
        if translated.is_empty() {
            return Err(SourceFailure::Unavailable);
        }
        Ok(translated.to_owned())
    }
}
