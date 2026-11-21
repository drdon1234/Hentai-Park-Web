use crate::{Capabilities, Error, Invocation, SourceState};
use base64::prelude::{Engine, BASE64_STANDARD};
use md5::{Digest, Md5};
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use park_net::{Purpose, Request, Session, Sessions, Transport};
use serde_json::{json, Value};
use std::{cell::RefCell, collections::BTreeMap, path::Path, sync::Arc, time::Duration};
use url::Url;

impl From<park_net::Error> for Error {
    fn from(value: park_net::Error) -> Self {
        match value {
            park_net::Error::Blocked => Self::Blocked,
            park_net::Error::Cancelled => Self::Cancelled,
            park_net::Error::Timeout => Self::Timeout,
            park_net::Error::Storage => Self::Storage,
            park_net::Error::Tls | park_net::Error::Network | park_net::Error::Limit => {
                Self::Network
            }
        }
    }
}

/// 由安装登记中的源身份及授权创建，每个 QuickJS 工作线程持有自己的宿主。
pub struct NativeHost {
    owner: String,
    domains: Vec<String>,
    chat_domains: Value,
    state: RefCell<SourceState>,
    session: Session,
    transport: Arc<Transport>,
    io: tokio::runtime::Runtime,
}
impl NativeHost {
    pub fn open(
        root: &Path,
        owner: &str,
        domains: Vec<String>,
        sessions: &Sessions,
        transport: Arc<Transport>,
    ) -> Result<Self, Error> {
        Self::registered(root, owner, domains, Value::Null, sessions, transport, None)
    }
    pub(crate) fn registered(
        root: &Path,
        owner: &str,
        domains: Vec<String>,
        chat_domains: Value,
        sessions: &Sessions,
        transport: Arc<Transport>,
        generation: Option<&str>,
    ) -> Result<Self, Error> {
        let state = SourceState::open(root, owner)?;
        let session = sessions.partition(owner, Purpose::Source)?;
        let checkpoint = park_storage::recovery::checkpoint(root).map_err(|_| Error::Storage)?;
        let stamp = match generation {
            Some(id) => Some(json!([checkpoint, id]).to_string()),
            None => checkpoint,
        };
        session.restored(stamp)?;
        let io = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::Network)?;
        Ok(Self {
            owner: owner.to_owned(),
            domains,
            chat_domains,
            state: RefCell::new(state),
            session,
            transport,
            io,
        })
    }
    pub fn snapshot(&self) -> Value {
        self.state.borrow().snapshot().clone()
    }

    fn request(&self, args: Value, call: &Invocation) -> Result<Value, Error> {
        if args["mode"].as_str() == Some("browser") {
            return Err(Error::Unsupported);
        }
        let mut headers = HeaderMap::new();
        if let Some(map) = args["headers"].as_object() {
            for (name, value) in map {
                headers.insert(
                    HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Config)?,
                    HeaderValue::from_str(value.as_str().ok_or(Error::Config)?)
                        .map_err(|_| Error::Config)?,
                );
            }
        }
        let body = if let Some(encoded) = args["bodyBase64"].as_str() {
            if !args["body"].is_null() || encoded.len() > 2_796_204 {
                return Err(Error::Config);
            }
            let bytes = BASE64_STANDARD.decode(encoded).map_err(|_| Error::Config)?;
            if bytes.len() > 2_097_152 {
                return Err(Error::Config);
            }
            Some(bytes.into())
        } else {
            args["body"].as_str().map(|s| s.as_bytes().to_vec().into())
        };
        let req = Request {
            url: Url::parse(args["url"].as_str().ok_or(Error::Config)?)
                .map_err(|_| Error::Config)?,
            method: Method::from_bytes(
                args["method"]
                    .as_str()
                    .unwrap_or("GET")
                    .to_ascii_uppercase()
                    .as_bytes(),
            )
            .map_err(|_| Error::Config)?,
            connect_address: args["connectAddress"]
                .as_str()
                .map(str::parse)
                .transpose()
                .map_err(|_| Error::Config)?,
            follow_redirects: args["followRedirects"].as_bool() != Some(false),
            headers,
            body,
        };
        let timeout =
            Duration::from_millis(args["timeout"].as_u64().unwrap_or(30_000)).min(call.remaining());
        let response = self.io.block_on(self.transport.execute_read(
            &self.session,
            &self.domains,
            req,
            16 * 1024 * 1024,
            timeout,
            &call.cancelled,
            call.read_scope.as_deref(),
        ))?;
        let body = if args["responseType"].as_str() == Some("base64") {
            BASE64_STANDARD.encode(response.body)
        } else {
            let charset = response
                .headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| {
                    v.split(';')
                        .skip(1)
                        .filter_map(|p| p.trim().split_once('='))
                        .find(|(k, _)| k.trim().eq_ignore_ascii_case("charset"))
                })
                .and_then(|(_, v)| {
                    encoding_rs::Encoding::for_label(v.trim().trim_matches('"').as_bytes())
                })
                .unwrap_or(encoding_rs::UTF_8);
            charset.decode(&response.body).0.into_owned()
        };
        let headers: BTreeMap<_, _> = response
            .headers
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str(), s)))
            .collect();
        Ok(
            json!({"status":response.status.as_u16(),"url":response.url.as_str(),"body":body,"headers":headers}),
        )
    }
}

impl Capabilities for NativeHost {
    fn call(
        &self,
        source: &str,
        operation: &str,
        args: Value,
        call: &Invocation,
    ) -> Result<Value, Error> {
        call.check()?;
        if source != self.owner {
            return Err(Error::Blocked);
        }
        if operation == "chatRequest" || (operation == "cookies" && args["op"] != "get") {
            if let Some(scope) = &call.read_scope { scope.side_effect()?; }
        }
        match operation {
            "request" => self.request(args, call),
            "chatRequest" => Ok(self.io.block_on(self.transport.chat_bootstrap(
                &self.session,
                &self.chat_domains,
                &args,
                &call.cancelled,
            ))?),
            "crypto" if args["op"] == "md5Hex" => {
                let digest = |text: &Value| {
                    let input = text.as_str().ok_or(Error::Config)?;
                    Ok(Value::String(format!("{:x}", Md5::digest(input.as_bytes()))))
                };
                match &args["args"]["text"] {
                    Value::Array(texts) => {
                        let mut hashes = Vec::with_capacity(texts.len());
                        for text in texts {
                            call.check()?;
                            hashes.push(digest(text)?);
                        }
                        Ok(Value::Array(hashes))
                    }
                    value => digest(value),
                }
            }
            "persist" => {
                self.state
                    .borrow_mut()
                    .persist(args.get("data").cloned().ok_or(Error::Config)?, call)?;
                Ok(Value::Null)
            }
            "cookies" => {
                let input = &args["args"];
                let url = Url::parse(input["url"].as_str().ok_or(Error::Config)?)
                    .map_err(|_| Error::Config)?;
                match args["op"].as_str() {
                    Some("get") => Ok(json!(self.session.cookies(&url, &self.domains)?)),
                    Some("set") => {
                        let values = serde_json::from_value(input["cookies"].clone())
                            .map_err(|_| Error::Config)?;
                        self.session.set_cookies(&url, &self.domains, &values)?;
                        Ok(json!(true))
                    }
                    Some("clear") => {
                        self.session.clear_host(&url, &self.domains)?;
                        Ok(json!(true))
                    }
                    _ => Err(Error::Unsupported),
                }
            }
            _ => Err(Error::Unsupported),
        }
    }
}
