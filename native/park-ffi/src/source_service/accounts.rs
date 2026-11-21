use super::*;
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(uniffi::Record)]
pub struct SiteAccount {
    pub logged_in: bool,
    pub name: String,
    pub configured: bool,
}

#[uniffi::export]
impl SourceService {
    pub fn source_options(&self, source: String, request: Arc<Request>) -> Result<String> {
        Ok(self
            .catalog
            .source_options(&source, None, plugins::budget(&request))?
            .to_string())
    }

    pub fn set_source_option(
        &self,
        source: String,
        name: String,
        json_value: String,
        request: Arc<Request>,
    ) -> Result<String> {
        if json_value.len() > 524288 {
            return Err(SourceFailure::Unsupported);
        }
        let value = serde_json::from_str(&json_value).map_err(|_| SourceFailure::Unsupported)?;
        Ok(self
            .catalog
            .source_options(&source, Some((&name, value)), plugins::budget(&request))?
            .to_string())
    }

    pub fn site_account(&self, source: String, request: Arc<Request>) -> Result<SiteAccount> {
        let (_, raw) =
            self.catalog
                .invoke(&source, "account.status", &[], plugins::budget(&request))?;
        let status = downloads::checked(raw)?;
        let logged_in = status["loggedIn"]
            .as_bool()
            .ok_or(SourceFailure::Unsupported)?;
        Ok(SiteAccount {
            logged_in,
            name: status["name"].as_str().unwrap_or_default().into(),
            configured: status["configured"].as_bool().unwrap_or(logged_in),
        })
    }

    /// 调用方显式提交后才执行一次。凭据只交给该源声明的表单，返回值只包含用户可见状态。
    pub fn sign_in(
        &self,
        source: String,
        values: HashMap<String, String>,
        request: Arc<Request>,
    ) -> Result<SiteAccount> {
        let call = plugins::budget(&request);
        let manifest = self.catalog.manifest(&source, call.clone())?;
        let fields = manifest["account"]["fields"]
            .as_array()
            .ok_or(SourceFailure::Unsupported)?;
        if values.is_empty()
            || values.len() > 32
            || fields.iter().any(|field| {
                field["optional"].as_bool() != Some(true)
                    && field["key"]
                        .as_str()
                        .and_then(|key| values.get(key))
                        .is_none_or(|value| value.trim().is_empty())
            })
            || values.iter().any(|(key, v)| {
                v.len() > 262144 || !fields.iter().any(|f| f["key"] == key.as_str())
            })
        {
            return Err(SourceFailure::Unsupported);
        }
        let (_, raw) = self
            .catalog
            .invoke(&source, "account.login", &[json!(values)], call)?;
        let result = downloads::checked(raw)?;
        if manifest["capabilities"]["status"] == true {
            let status = self.site_account(source, request)?;
            if !status.logged_in {
                return Err(SourceFailure::LoginRequired);
            }
            return Ok(status);
        }
        let name = result
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(SiteAccount {
            logged_in: true,
            configured: true,
            name: name.into(),
        })
    }

    pub fn sign_out(&self, source: String, request: Arc<Request>) -> Result<()> {
        let (_, raw) =
            self.catalog
                .invoke(&source, "account.logout", &[], plugins::budget(&request))?;
        downloads::checked(raw)?;
        Ok(())
    }
}
