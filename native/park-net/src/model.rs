use crate::{connect::connect_model, transport::ConnectionTask, Error, Transport};
use bytes::Bytes;
use http::header;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::{Position, Url};

struct Events {
    line: Vec<u8>,
    event: String,
    total: usize,
    done: bool,
}
impl Events {
    fn parse(&mut self, emit: &mut impl FnMut(String)) -> Result<(), Error> {
        if self.event.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.event);
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let json: Value = serde_json::from_str(&data).map_err(|_| Error::Network)?;
        if !json.is_object() || !json["error"].is_null() {
            return Err(Error::Network);
        }
        let choice = &json["choices"][0];
        if choice["finish_reason"]
            .as_str()
            .is_some_and(|s| !matches!(s, "stop" | "length"))
        {
            return Err(Error::Network);
        }
        if let Some(content) = choice["delta"]["content"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            self.total += content.len();
            if self.total > 8 * 1024 * 1024 {
                return Err(Error::Limit);
            }
            emit(content.to_owned());
        }
        Ok(())
    }
    fn read(&mut self, input: &[u8], emit: &mut impl FnMut(String)) -> Result<(), Error> {
        for bytes in input.split_inclusive(|&b| b == b'\n') {
            if self.line.len() + self.event.len() + bytes.len() > 1024 * 1024 {
                return Err(Error::Limit);
            }
            self.line.extend_from_slice(bytes);
            if self.line.last() != Some(&b'\n') {
                continue;
            }
            let text = std::str::from_utf8(&self.line)
                .map_err(|_| Error::Network)?
                .trim_end_matches(['\n', '\r']);
            if text.is_empty() {
                self.parse(emit)?
            } else if let Some(value) = text.strip_prefix("data:") {
                if !self.event.is_empty() {
                    self.event.push('\n')
                }
                self.event
                    .push_str(value.strip_prefix(' ').unwrap_or(value));
            }
            self.line.clear();
            if self.done {
                break;
            }
        }
        Ok(())
    }
}
impl Transport {
    /// 此客户端无 Cookie、磁盘缓存或重定向。只把用户提供的密钥发往配置的 HTTPS 端点。
    pub async fn model_reply(
        &self,
        base: &str,
        key: &str,
        body: Vec<u8>,
        cancel: &CancellationToken,
        mut emit: impl FnMut(String),
    ) -> Result<(), Error> {
        let mut url = Url::parse(base).map_err(|_| Error::Blocked)?;
        if base.len() > 2048
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.scheme(), "http" | "https")
            || (url.scheme() == "http" && !key.is_empty())
            || key.len() > 4096
        {
            return Err(Error::Blocked);
        }
        url.set_path(&format!(
            "{}/chat/completions",
            url.path().trim_end_matches('/')
        ));
        if body.len() > 16 * 1024 * 1024 {
            return Err(Error::Limit);
        }
        let exchange = async {
            let io = connect_model(&url, self.proxy.as_ref(), self.tls.clone()).await?;
            let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
                .await
                .map_err(|_| Error::Network)?;
            let _connection = ConnectionTask(tokio::spawn(async move {
                let _ = conn.await;
            }));
            let mut req = http::Request::post(&url[Position::BeforePath..Position::AfterQuery])
                .header(
                    header::HOST,
                    &url[Position::BeforeHost..Position::AfterPort],
                )
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "text/event-stream");
            if !key.is_empty() {
                req = req.header(header::AUTHORIZATION, format!("Bearer {key}"))
            }
            let req = req
                .body(Full::new(Bytes::from(body)))
                .map_err(|_| Error::Blocked)?;
            let response = tokio::time::timeout(Duration::from_secs(60), sender.send_request(req))
                .await
                .map_err(|_| Error::Timeout)?
                .map_err(|_| Error::Network)?;
            if !response.status().is_success() {
                return Err(Error::Network);
            }
            let mut body = response.into_body();
            let mut events = Events {
                line: Vec::new(),
                event: String::new(),
                total: 0,
                done: false,
            };
            while !events.done {
                let frame = tokio::time::timeout(Duration::from_secs(60), body.frame())
                    .await
                    .map_err(|_| Error::Timeout)?;
                let Some(frame) = frame else {
                    events.read(b"\n\n", &mut emit)?;
                    break;
                };
                if let Ok(chunk) = frame.map_err(|_| Error::Network)?.into_data() {
                    events.read(&chunk, &mut emit)?
                }
            }
            if !events.done || events.total == 0 {
                return Err(Error::Network);
            }
            Ok(())
        };
        tokio::select! {biased;_=cancel.cancelled()=>Err(Error::Cancelled),result=exchange=>result}
    }
}
