use super::*;

fn payload(text: &str) -> Result<Value> {
    if text.len() > 2 * 1024 * 1024 {
        return Err(SourceFailure::Unsupported);
    }
    serde_json::from_str(text).map_err(|_| SourceFailure::Unsupported)
}

/// 桌面插件宿主持有原站会话。选择和解析始终绑定同一次展示的作品与渠道。
#[derive(Object)]
pub struct HostGameOffers {
    source: String,
    work: String,
    manifest: Value,
    rows: Vec<Value>,
    host: Arc<RouteHost>,
}

#[derive(Object)]
pub struct HostGameChoice {
    menu: Arc<HostGameOffers>,
    resource: String,
    channel: String,
    choice: Value,
}

#[uniffi::export]
pub fn open_host_game_offers(
    source: String,
    work: String,
    manifest: String,
    entries: String,
    cache_directory: String,
    proxy: String,
    system_roots: Vec<Vec<u8>>,
) -> Result<Arc<HostGameOffers>> {
    let manifest = payload(&manifest)?;
    if manifest["key"] != source
        || manifest["category"] != "game"
        || manifest["capabilities"]["gameResources"] != true
        || manifest["capabilities"]["resolveGameResource"] != true
    {
        return Err(SourceFailure::Unsupported);
    }
    let rows = offers(payload(&entries)?)?;
    let sessions = Arc::new(Sessions::new(std::path::Path::new(&cache_directory)));
    let host = RouteHost::open(cache_directory, sessions, proxy, system_roots)?;
    Ok(Arc::new(HostGameOffers {
        source,
        work,
        manifest,
        rows,
        host,
    }))
}

#[uniffi::export]
impl HostGameOffers {
    pub fn entries(&self) -> String {
        Value::Array(self.rows.clone()).to_string()
    }

    pub fn authorize(
        self: Arc<Self>,
        resource: String,
        channel: String,
        confirmed_reply: Option<String>,
        current_entries: String,
    ) -> Result<Arc<HostGameChoice>> {
        let choice =
            confirmed_choice(&self.rows, &resource, &channel, confirmed_reply.as_deref())?.clone();
        validate_choice(
            &choice,
            &offers(payload(&current_entries)?)?,
            &resource,
            &channel,
        )?;
        Ok(Arc::new(HostGameChoice {
            menu: self,
            resource,
            channel,
            choice,
        }))
    }
}

#[uniffi::export]
impl HostGameChoice {
    pub fn resolved(&self, envelope: String) -> Result<Arc<GameLinks>> {
        build_links(
            &self.menu.host,
            &self.menu.source,
            &self.menu.work,
            self.resource.clone(),
            self.channel.clone(),
            &self.choice,
            &self.menu.manifest,
            payload(&envelope)?,
        )
    }
}
