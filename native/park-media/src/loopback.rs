use crate::Error;
use axum::Router;
use std::net::Ipv4Addr;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub(crate) struct Listener {
    socket: TcpListener,
    pub authority: String,
    pub prefix: String,
}
impl Listener {
    pub async fn bind() -> Result<Self, Error> {
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| Error::Unavailable)?;
        let authority = socket
            .local_addr()
            .map_err(|_| Error::Unavailable)?
            .to_string();
        let mut secret = [0u8; 24];
        getrandom::fill(&mut secret).map_err(|_| Error::Unavailable)?;
        let prefix = format!(
            "/{}/",
            secret
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        Ok(Self {
            socket,
            authority,
            prefix,
        })
    }
    pub fn serve(self, entry: &str, app: Router, stop: CancellationToken) -> PlaybackRoute {
        let url = format!("http://{}{}{entry}", self.authority, self.prefix);
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(self.socket, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await;
        });
        PlaybackRoute { url, stop, task }
    }
}
pub struct PlaybackRoute {
    url: String,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl PlaybackRoute {
    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn close(&self) {
        self.stop.cancel();
        self.task.abort();
    }
}
impl Drop for PlaybackRoute {
    fn drop(&mut self) {
        self.close();
    }
}
