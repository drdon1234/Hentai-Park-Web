use super::library_updates::{now_ms, settings};
use super::*;
use crate::ProgressStore;
use park_storage::subscriptions as sub;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

#[derive(uniffi::Record)]
pub struct SubscriptionDimension {
    pub key: String,
    pub label: String,
}

#[derive(uniffi::Record)]
pub struct SubscriptionSite {
    pub key: String,
    pub name: String,
    pub dimensions: Vec<SubscriptionDimension>,
}

fn storage<T>(_: T) -> SourceFailure {
    SourceFailure::Storage
}
fn account_identity(v: &Value) -> Value {
    match v {
        Value::Object(obj) => {
            let fields: BTreeMap<_, _> = obj
                .iter()
                .filter(|(k, _)| {
                    !matches!(
                        k.to_lowercase().replace('_', "").as_str(),
                        "token"
                            | "accesstoken"
                            | "refreshtoken"
                            | "expires"
                            | "expiresat"
                            | "expiry"
                            | "retryat"
                            | "failures"
                            | "ratelimited"
                            | "name"
                            | "updatedat"
                    )
                })
                .map(|(k, v)| (k, account_identity(v)))
                .collect();
            json!(fields)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(account_identity).collect()),
        _ => v.clone(),
    }
}
impl SourceService {
    fn context_hash(&self, store: &ProgressStore, key: &str, manifest: &Value) -> Result<String> {
        let state = park_runtime::SourceState::open(&store.root, key)?;
        let data = state.snapshot();
        let account = data["data"].as_object().map(|obj| {
            obj.iter()
                .filter(|(k, _)| {
                    let k = k.to_lowercase();
                    ["credential", "account", "user", "session"]
                        .iter()
                        .any(|pattern| k.contains(pattern))
                })
                .map(|(k, v)| json!([k, account_identity(v)]))
                .collect::<Vec<_>>()
        });
        let names = manifest["account"]["requiredCookies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let defaults = json!({});
        self.routes
            .sessions
            .partition(key, Purpose::Source)
            .map_err(storage)?
            .subscription_context(
                &names,
                &manifest["version"],
                data.get("settings")
                    .filter(|v| v.is_object())
                    .unwrap_or(&defaults),
                &json!(account),
                settings(store)?["proxy"].as_str().unwrap_or(""),
            )
            .map_err(storage)
    }
    fn check_target(
        &self,
        store: &ProgressStore,
        t: &sub::Target,
        lease: &str,
        hours: i64,
        r: &Request,
        deadline: Instant,
    ) -> std::result::Result<bool, &'static str> {
        let budget = || {
            if r.cancelled.is_cancelled() || Instant::now() >= deadline {
                return Err("CANCELLED");
            }
            Ok(Invocation::child(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(45)),
                &r.cancelled,
            ))
        };
        let manifest = self
            .catalog
            .manifest(&t.source, budget()?)
            .map_err(|_| "UNSUPPORTED")?;
        if manifest["capabilities"]["subscriptionUpdates"] != true {
            return Err("UNSUPPORTED");
        }
        let hash = self
            .context_hash(store, &t.source, &manifest)
            .map_err(|_| "CONFIG")?;
        let mut state = {
            let mut db = store.lock().map_err(|_| "CONFIG")?;
            sub::begin(&mut db, &t.id, lease, &hash).map_err(|_| "CONFIG")?
        };
        let mut cursors: HashSet<String> = state.progress.cursor.iter().cloned().collect();
        let mut complete = false;
        for page in 0..3 {
            let args = [
                json!({"dimension":t.dimension,"target":t.target}),
                json!({"cursor":state.progress.cursor}),
            ];
            let (_, response) = self
                .catalog
                .invoke(&t.source, "updateSubscriptions.check", &args, budget()?)
                .map_err(|e| match e {
                    park_runtime::Error::Cancelled => "CANCELLED",
                    _ => "NETWORK",
                })?;
            if response["ok"] != true {
                return Err(match response["error"]["code"].as_str() {
                    Some("LOGIN_REQUIRED") => "LOGIN_REQUIRED",
                    Some("HTTP") => "HTTP",
                    Some("BLOCKED") => "BLOCKED",
                    Some("CLOUDFLARE") => "CLOUDFLARE",
                    Some("CONFIG") => "CONFIG",
                    Some("PARSE") => "PARSE",
                    Some("UNSUPPORTED") => "UNSUPPORTED",
                    Some("NOT_FOUND") => "NOT_FOUND",
                    _ => "NETWORK",
                });
            }
            let value = &response["value"];
            let list = value["items"]
                .as_array()
                .filter(|a| a.len() <= 20000)
                .ok_or("PARSE")?;
            let has_next = value["hasNext"].as_bool().ok_or("PARSE")?;
            let mut all_seen = !list.is_empty();
            for val in list {
                let item: sub::Item = serde_json::from_value(val.clone()).map_err(|_| "PARSE")?;
                if !item.valid() {
                    return Err("PARSE");
                }
                all_seen &= state.seen.contains(&item.id);
                state.progress.items.push(item);
            }
            complete = !has_next
                || value["windowed"] == true
                    && ((!state.baseline && page == 2) || state.baseline && all_seen);
            if complete {
                break;
            }
            let next = value["cursor"]
                .as_str()
                .filter(|s| !s.is_empty() && s.encode_utf16().count() <= 4096)
                .ok_or("PARSE")?;
            if !cursors.insert(next.to_owned()) {
                return Err("PARSE");
            }
            state.progress.cursor = Some(next.into());
            if page < 2 {
                std::thread::sleep(Duration::from_millis(300))
            }
        }
        let manifest = self
            .catalog
            .manifest(&t.source, budget()?)
            .map_err(|_| "CANCELLED")?;
        if self
            .context_hash(store, &t.source, &manifest)
            .map_err(|_| "CONFIG")?
            != hash
        {
            return Err("CANCELLED");
        }
        let mut db = store.lock().map_err(|_| "CONFIG")?;
        budget()?;
        sub::commit(
            &mut db,
            t,
            lease,
            state,
            complete,
            now_ms().map_err(|_| "CONFIG")?,
            hours,
        )
        .map_err(|e| match e {
            park_storage::backup::Error::Invalid => "PARSE",
            _ => "CONFIG",
        })
    }
}
#[uniffi::export]
impl SourceService {
    pub fn subscription_sites(&self, locale: String, request: Arc<Request>) -> Result<Vec<SubscriptionSite>> {
        let mut sites = Vec::new();
        for site in self.catalog.installed()? {
            let manifest = match self.catalog.manifest(&site.key, plugins::budget(&request)) {
                Ok(value) => value,
                Err(park_runtime::Error::Cancelled) => return Err(SourceFailure::Cancelled),
                Err(_) => continue,
            };
            if manifest["capabilities"]["subscriptionUpdates"] != true { continue; }
            let Some(rows) = manifest["updateSubscriptions"]["dimensions"].as_array() else { continue };
            let copy = park_domain::LocalizedCopy::new(&locale, manifest["i18n"].clone());
            let dimensions: Vec<_> = rows.iter().filter_map(|row| {
                let key = row["key"].as_str()?;
                Some(SubscriptionDimension {
                    key: key.to_owned(),
                    label: copy.text(row["label"].as_str().unwrap_or(key), &Default::default()),
                })
            }).collect();
            if !dimensions.is_empty() {
                sites.push(SubscriptionSite {
                    name: manifest["name"].as_str().unwrap_or(&site.key).to_owned(),
                    key: site.key, dimensions,
                });
            }
        }
        Ok(sites)
    }

    pub fn save_subscription(
        &self, store: Arc<ProgressStore>, original: Option<String>, source: String,
        dimension: String, input: String, label: String, request: Arc<Request>,
    ) -> Result<bool> {
        let input = input.trim();
        let label = label.trim();
        if input.is_empty() || input.encode_utf16().count() > 2048 || label.encode_utf16().count() > 512 {
            return Err(SourceFailure::Configuration);
        }
        let old: Option<String> = {
            let mut db = store.lock().map_err(storage)?;
            sub::migrate(&mut db, &store.root).map_err(storage)?;
            original.as_ref().map(|id| db.query_row(
                "SELECT target FROM subscriptions WHERE identity=? AND source=? AND dimension=?",
                rusqlite::params![id, source, dimension], |row| row.get(0),
            )).transpose().map_err(storage)?
        };
        let (target, default_label) = if old.as_deref() == Some(input) {
            (input.to_owned(), input.to_owned())
        } else {
            let budget = plugins::budget(&request);
            let manifest = self.catalog.manifest(&source, budget.clone())?;
            if manifest["capabilities"]["subscriptionUpdates"] != true
                || !manifest["updateSubscriptions"]["dimensions"].as_array()
                    .is_some_and(|rows| rows.iter().any(|row| row["key"] == dimension)) {
                return Err(SourceFailure::Unsupported);
            }
            let (_, result) = self.catalog.invoke(&source, "updateSubscriptions.normalize",
                &[json!(dimension), json!(input)], budget)?;
            let data = downloads::checked(result)?;
            let target = data["id"].as_str().map(str::trim).filter(|v| !v.is_empty())
                .ok_or(SourceFailure::Configuration)?;
            (target.to_owned(), data["label"].as_str().filter(|v| !v.is_empty()).unwrap_or(target).to_owned())
        };
        let mut db = store.lock().map_err(storage)?;
        request.check().map_err(|_| SourceFailure::Cancelled)?;
        sub::save_target(&mut db, original.as_deref(), &source, &dimension, &target,
            if label.is_empty() { &default_label } else { label }, now_ms()?).map_err(storage)
    }

    pub fn check_subscriptions(
        &self,
        store: Arc<ProgressStore>,
        force: bool,
        request: Arc<Request>,
    ) -> Result<String> {
        let hours = settings(&store)?["subscriptionUpdateHours"]
            .as_i64()
            .unwrap_or(6);
        if hours <= 0 && !force {
            return Ok(json!({"checked":0,"failed":0}).to_string());
        }
        let rows = {
            let mut db = store.lock().map_err(storage)?;
            sub::migrate(&mut db, &store.root).map_err(storage)?;
            sub::targets(&db).map_err(storage)?
        };
        let mut groups = BTreeMap::<String, Vec<sub::Target>>::new();
        for t in rows {
            groups.entry(t.source.clone()).or_default().push(t)
        }
        let groups = Mutex::new(VecDeque::from_iter(groups.into_values()));
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut checked = 0;
        let mut failed = 0;
        std::thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| -> Result<(usize, usize)> {
                        let mut counts = (0, 0);
                        while let Some(group) = {
                            let mut q = groups.lock().map_err(storage)?;
                            q.pop_front()
                        } {
                            for t in group {
                                if request.cancelled.is_cancelled() || Instant::now() >= deadline {
                                    return Ok(counts);
                                }
                                let lease = {
                                    let db = store.lock().map_err(storage)?;
                                    sub::acquire(&db, &t.id, now_ms()?, force).map_err(storage)?
                                };
                                if let Some(lease) = lease {
                                    match self.check_target(
                                        &store,
                                        &t,
                                        &lease,
                                        if hours > 0 { hours } else { 6 },
                                        &request,
                                        deadline,
                                    ) {
                                        Ok(committed) => counts.0 += usize::from(committed),
                                        Err(code) => {
                                            let db = store.lock().map_err(storage)?;
                                            sub::fail(&db, &t.id, &lease, now_ms()?, code)
                                                .map_err(storage)?;
                                            counts.1 += 1
                                        }
                                    }
                                }
                            }
                        }
                        Ok(counts)
                    })
                })
                .collect();
            for worker in workers {
                let (a, b) = worker.join().map_err(storage)??;
                checked += a;
                failed += b
            }
            Ok(())
        })?;
        if request.cancelled.is_cancelled() {
            return Err(SourceFailure::Cancelled);
        }
        Ok(json!({"checked":checked,"failed":failed}).to_string())
    }
}
#[uniffi::export]
impl ProgressStore {
    pub fn subscription_event_page(&self, after_time: Option<i64>, after_key: String, limit: u32) -> Result<String> {
        let mut db = self.lock().map_err(storage)?;
        sub::migrate(&mut db, &self.root).map_err(storage)?;
        let events = sub::event_page(&db, after_time, &after_key, limit).map_err(storage)?;
        Ok(Value::Array(events).to_string())
    }

    pub fn subscription_targets(&self) -> Result<String> {
        let mut db = self.lock().map_err(storage)?;
        sub::migrate(&mut db, &self.root).map_err(storage)?;
        Ok(Value::Array(sub::target_view(&db).map_err(storage)?).to_string())
    }

    pub fn subscription_post_target(&self, key: String, source: String) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        self.lock().map_err(storage)?.query_row(
            "SELECT s.target FROM subscription_event_targets t JOIN subscriptions s USING(identity)
             JOIN subscription_events e USING(event_key)
             WHERE t.event_key=?1 AND s.source=?2 AND e.source=s.source
             ORDER BY s.created_at,s.identity LIMIT 1",
            rusqlite::params![key, source], |row| row.get(0)
        ).optional().map_err(storage)
    }

    pub fn subscription_updates(&self) -> Result<String> {
        let mut db = self.lock().map_err(storage)?;
        sub::migrate(&mut db, &self.root).map_err(storage)?;
        Ok(sub::view(&db).map_err(storage)?.to_string())
    }
    pub fn set_subscription_option(&self, id: String, check: Option<bool>, notify: Option<bool>) -> Result<()> {
        let mut db = self.lock().map_err(storage)?;
        sub::set_target_option(&mut db, &id, check, notify).map_err(storage)
    }

    pub fn remove_subscription(&self, id: String) -> Result<()> {
        self.lock().map_err(storage)?.execute("DELETE FROM subscriptions WHERE identity=?", [id]).map_err(storage)?;
        Ok(())
    }

    pub fn read_subscription_event(&self, key: String) -> Result<()> {
        self.lock()
            .map_err(storage)?
            .execute(
                "UPDATE subscription_events SET unread=0 WHERE event_key=?",
                [key],
            )
            .map_err(storage)?;
        Ok(())
    }
    pub fn read_all_subscription_events(&self) -> Result<()> {
        self.lock().map_err(storage)?.execute(
            "UPDATE subscription_events SET unread=0 WHERE unread=1", []
        ).map_err(storage)?;
        Ok(())
    }

    pub fn remove_subscription_events(&self, key: Option<String>) -> Result<()> {
        let db = self.lock().map_err(storage)?;
        if let Some(key) = key {
            db.execute("DELETE FROM subscription_events WHERE event_key=?", [key]).map_err(storage)?;
        } else {
            db.execute("DELETE FROM subscription_events", []).map_err(storage)?;
        }
        Ok(())
    }
    pub fn claim_subscription_notification(&self) -> Result<Option<String>> {
        let db = self.lock().map_err(storage)?;
        Ok(sub::claim_notification(&db, now_ms()?)
            .map_err(storage)?
            .map(|v| v.to_string()))
    }
    pub fn finish_subscription_notification(
        &self,
        key: String,
        claim: i64,
        delivered: bool,
    ) -> Result<()> {
        self.lock().map_err(storage)?.execute("UPDATE subscription_events SET delivered=?3,attempts=attempts+1 WHERE event_key=?1 AND next_attempt=?2 AND delivered=0",rusqlite::params![key,claim,delivered]).map_err(storage)?;
        Ok(())
    }
}
