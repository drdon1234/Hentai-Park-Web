use crate::{Error, ImageAsset};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Hash, PartialEq, Eq)]
pub struct ImageKey {
    pub source: String,
    pub identity: String,
    pub strips: u32,
    pub crop: bool,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Visible,
    Preload,
    Background,
}
#[derive(Clone)]
pub struct Route {
    pub host: String,
    pub browser_session: Option<String>,
}
type Output = Result<Arc<ImageAsset>, Error>;
type Action = Box<dyn FnOnce(&CancellationToken) -> Result<ImageAsset, Error> + Send>;
struct Job {
    count: [usize; 3],
    ordinal: u64,
    route: Route,
    action: Option<Action>,
    token: CancellationToken,
    finished: Arc<(Mutex<Option<Output>>, Condvar)>,
}
struct Ready {
    value: Arc<ImageAsset>,
    last: u64,
}
struct State {
    serial: u64,
    preferred: usize,
    active: usize,
    jobs: HashMap<ImageKey, Job>,
    cache: HashMap<ImageKey, Ready>,
    hosts: HashMap<String, usize>,
    sessions: HashMap<String, usize>,
    browsers: usize,
}
pub struct ImagePool {
    state: Mutex<State>,
    maximum: usize,
    per_host: usize,
}
pub struct ImageTicket {
    pool: Arc<ImagePool>,
    key: ImageKey,
    priority: Priority,
    finished: Arc<(Mutex<Option<Output>>, Condvar)>,
}
impl ImagePool {
    pub fn invalidate(&self, key: &ImageKey, asset: &Arc<ImageAsset>) {
        let mut state = self.state.lock().unwrap();
        if state
            .cache
            .get(key)
            .is_some_and(|entry| Arc::ptr_eq(&entry.value, asset))
        {
            state.cache.remove(key);
        }
    }
    pub fn new(maximum: usize, per_host: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                serial: 0,
                preferred: 0,
                active: 0,
                jobs: HashMap::new(),
                cache: HashMap::new(),
                hosts: HashMap::new(),
                sessions: HashMap::new(),
                browsers: 0,
            }),
            maximum: maximum.max(1),
            per_host: per_host.max(1),
        })
    }
    pub fn request(
        self: &Arc<Self>,
        key: ImageKey,
        route: Route,
        priority: Priority,
        action: impl FnOnce(&CancellationToken) -> Result<ImageAsset, Error> + Send + 'static,
    ) -> ImageTicket {
        let mut state = self.state.lock().unwrap();
        state.serial += 1;
        let ordinal = state.serial;
        if state
            .cache
            .get(&key)
            .is_some_and(|entry| !entry.value.file.path().is_file())
        {
            state.cache.remove(&key);
        }
        let finished = if let Some(entry) = state.cache.get_mut(&key) {
            entry.last = ordinal;
            Arc::new((Mutex::new(Some(Ok(entry.value.clone()))), Condvar::new()))
        } else {
            let job = state.jobs.entry(key.clone()).or_insert_with(|| Job {
                count: [0; 3],
                ordinal,
                route,
                action: Some(Box::new(action)),
                token: CancellationToken::new(),
                finished: Arc::new((Mutex::new(None), Condvar::new())),
            });
            job.count[priority as usize] += 1;
            job.finished.clone()
        };
        drop(state);
        self.schedule();
        ImageTicket {
            pool: self.clone(),
            key,
            priority,
            finished,
        }
    }
    fn schedule(self: &Arc<Self>) {
        let mut state = self.state.lock().unwrap();
        while state.active < self.maximum {
            let chosen = state
                .jobs
                .iter()
                .filter(|(_, j)| {
                    j.action.is_some()
                        && state.hosts.get(&j.route.host).copied().unwrap_or(0) < self.per_host
                        && j.route.browser_session.as_ref().is_none_or(|id| {
                            state.browsers < 4 && state.sessions.get(id).copied().unwrap_or(0) < 2
                        })
                })
                .min_by_key(|(_, j)| {
                    (
                        if state.preferred >= 8 {
                            0
                        } else {
                            j.count.iter().position(|n| *n > 0).unwrap_or(3)
                        },
                        j.ordinal,
                    )
                })
                .map(|(key, _)| key.clone());
            let Some(key) = chosen else { break };
            state.preferred = (state.preferred + 1) % 9;
            let job = state.jobs.get_mut(&key).unwrap();
            let action = job.action.take().unwrap();
            let token = job.token.clone();
            let finished = job.finished.clone();
            let route = job.route.clone();
            state.active += 1;
            *state.hosts.entry(route.host.clone()).or_default() += 1;
            if let Some(id) = &route.browser_session {
                state.browsers += 1;
                *state.sessions.entry(id.clone()).or_default() += 1;
            }
            let pool = self.clone();
            std::thread::spawn(move || {
                let result = if token.is_cancelled() {
                    Err(Error::Cancelled)
                } else {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(&token)))
                        .unwrap_or(Err(Error::InvalidImage))
                        .map(Arc::new)
                };
                let mut state = pool.state.lock().unwrap();
                state.active -= 1;
                fn release(map: &mut HashMap<String, usize>, key: &str) {
                    if let Some(n) = map.get_mut(key) {
                        *n -= 1;
                        if *n == 0 {
                            map.remove(key);
                        }
                    }
                }
                release(&mut state.hosts, &route.host);
                if let Some(id) = route.browser_session {
                    state.browsers -= 1;
                    release(&mut state.sessions, &id);
                }
                let current = state
                    .jobs
                    .get(&key)
                    .is_some_and(|job| Arc::ptr_eq(&job.finished, &finished));
                if current {
                    state.jobs.remove(&key);
                    if !token.is_cancelled() {
                        if let Ok(value) = &result {
                            let last = state.serial;
                            state.cache.insert(
                                key,
                                Ready {
                                    value: value.clone(),
                                    last,
                                },
                            );
                            while state.cache.len() > 64
                                || state
                                    .cache
                                    .values()
                                    .map(|r| r.value.temporary_bytes())
                                    .sum::<u64>()
                                    > 96 * 1024 * 1024
                            {
                                let oldest = state
                                    .cache
                                    .iter()
                                    .min_by_key(|(_, entry)| entry.last)
                                    .map(|(key, _)| key.clone())
                                    .unwrap();
                                state.cache.remove(&oldest);
                            }
                        }
                    }
                }
                *finished.0.lock().unwrap() = Some(if token.is_cancelled() {
                    Err(Error::Cancelled)
                } else {
                    result
                });
                finished.1.notify_all();
                drop(state);
                pool.schedule();
            });
        }
    }
}
impl ImageTicket {
    pub fn key(&self) -> &ImageKey {
        &self.key
    }
    pub fn wait(&self, cancelled: impl Fn() -> bool) -> Output {
        let mut value = self.finished.0.lock().unwrap();
        loop {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            if let Some(output) = value.as_ref() {
                return output.clone();
            }
            value = self
                .finished
                .1
                .wait_timeout(value, Duration::from_millis(12))
                .unwrap()
                .0;
        }
    }
}
impl Drop for ImageTicket {
    fn drop(&mut self) {
        let mut state = self.pool.state.lock().unwrap();
        if let Some(job) = state.jobs.get_mut(&self.key) {
            if Arc::ptr_eq(&job.finished, &self.finished) {
                job.count[self.priority as usize] -= 1;
                if job.count == [0; 3] {
                    job.token.cancel();
                    state.jobs.remove(&self.key);
                }
            }
        }
    }
}
