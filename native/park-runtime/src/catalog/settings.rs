use super::*;
use crate::SourceState;
use serde_json::{json, Map};

impl SourceCatalog {
    /// 只返回声明的配置；保密项只报告是否已设置。与插件安装、脚本状态刷新和恢复共用文件锁。
    pub fn source_options(
        &self,
        key: &str,
        change: Option<(&str, Value)>,
        call: Invocation,
    ) -> Result<Value, Error> {
        let _guard = self.lock(change.is_some(), &call)?;
        let entry = self
            .installed()?
            .into_iter()
            .find(|row| row.key == key)
            .ok_or(Error::Config)?;
        let manifest = installed_manifest(
            &self.root,
            key,
            &entry.sha256,
            &self.polyfills,
            call.clone(),
        )?;
        let options = manifest["settings"].as_array().ok_or(Error::Config)?;
        let mut state = SourceState::open(&self.root, key)?;
        if let Some((name, value)) = change {
            let spec = options
                .iter()
                .find(|row| row["key"] == name)
                .ok_or(Error::Config)?;
            if name == "managedAccount" && manifest["account"]["managedAccount"] == true {
                return Err(Error::Unsupported);
            }
            let valid = match spec["type"].as_str().unwrap_or("text") {
                "switch" => value.is_boolean(),
                "select" => spec["values"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|row| row["value"] == value)),
                "text" | "password" => value.as_str().is_some_and(|text| text.len() <= 262144),
                _ => false,
            };
            if !valid {
                return Err(Error::Config);
            }
            state.set_option(name, value, &call)?;
        }
        let settings = &state.snapshot()["settings"];
        let mut values = Map::new();
        let mut configured = Vec::new();
        for option in options {
            let name = option["key"].as_str().ok_or(Error::Config)?;
            if let Some(value) = settings.get(name) {
                if option["type"] == "password" || option["secret"] == true {
                    if value.as_str().is_some_and(|text| !text.is_empty()) {
                        configured.push(name);
                    }
                } else {
                    values.insert(name.to_owned(), value.clone());
                }
            }
        }
        Ok(json!({"values":values,"configured":configured}))
    }
}
