use super::*;
use sha2::{Digest, Sha256};

#[derive(Object)]
pub struct OnlineGame {
    target: park_net::GameNavigation,
    profile: String,
}

#[uniffi::export]
impl OnlineGame {
    pub fn url(&self) -> String {
        self.target.initial.to_string()
    }
    pub fn movie(&self) -> String {
        self.target.movie.to_string()
    }
    pub fn player(&self) -> Option<String> {
        self.target.player.as_ref().map(ToString::to_string)
    }
    pub fn storage_id(&self) -> String {
        self.target.storage_id.clone()
    }
    pub fn profile_name(&self) -> String {
        self.profile.clone()
    }
    pub fn allows_document(&self, address: String) -> bool {
        self.target.document(&address)
    }
    pub fn allows_resource(&self, address: String) -> bool {
        self.target.resource(&address)
    }
}

#[uniffi::export]
impl SourceService {
    pub fn online_game(
        &self,
        source: String,
        work: String,
        request: Arc<Request>,
    ) -> Result<Arc<OnlineGame>> {
        let (manifest, raw) = self.catalog.invoke(
            &source,
            "onlineGame",
            &[work.clone().into()],
            plugins::budget(&request),
        )?;
        if manifest["capabilities"]["onlineGame"] != true || manifest["category"] != "onlineGame" {
            return Err(SourceFailure::Unsupported);
        }
        let target = park_net::GameNavigation::from_plugin(&downloads::checked(raw)?, &work)
            .map_err(|_| SourceFailure::Unsupported)?;
        let mut hash = Sha256::new();
        hash.update(source.as_bytes());
        hash.update([0]);
        hash.update(target.storage_id.as_bytes());
        let profile = format!("park_game_{:x}", hash.finalize());
        Ok(Arc::new(OnlineGame { target, profile }))
    }
}
