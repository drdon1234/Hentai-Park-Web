use super::{
    remux::{Bootstrap, Window},
    Error, HlsClient, MediaList, Result,
};
use axum::body::Bytes;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::{watch, OnceCell, Semaphore};
use tokio_util::sync::CancellationToken;
use url::Url;

const CACHE_BYTES: usize = 24 * 1024 * 1024;
struct Job {
    token: CancellationToken,
    done: watch::Sender<Option<Result<Arc<Window>>>>,
}
#[derive(Default)]
struct State {
    epoch: usize,
    bootstrap: Arc<OnceCell<Bootstrap>>,
    jobs: HashMap<usize, Arc<Job>>,
    cache: VecDeque<(usize, Bytes)>,
    bytes: usize,
    init: Option<Bytes>,
}
pub struct Windows {
    client: Arc<HlsClient>,
    url: Url,
    list: Arc<MediaList>,
    start: Vec<f64>,
    epochs: Vec<usize>,
    stop: CancellationToken,
    slots: Semaphore,
    state: Mutex<State>,
}
impl Windows {
    pub fn new(
        client: Arc<HlsClient>,
        url: Url,
        list: Arc<MediaList>,
        stop: CancellationToken,
    ) -> Arc<Self> {
        let mut t = 0.;
        let mut epoch = 0;
        let mut starts = Vec::new();
        let mut flags = Vec::new();
        for (index, segment) in list.segments.iter().enumerate() {
            if segment.discontinuity {
                epoch = index;
            }
            flags.push(epoch);
            starts.push(t);
            t += segment.seconds;
        }
        Arc::new(Self {
            client,
            url,
            list,
            start: starts,
            epochs: flags,
            stop,
            slots: Semaphore::new(2),
            state: Mutex::default(),
        })
    }
    async fn build(
        &self,
        index: usize,
        cell: &OnceCell<Bootstrap>,
        token: &CancellationToken,
    ) -> Result<Arc<Window>> {
        let _slot = tokio::select! {biased;_=token.cancelled()=>return Err(Error::Cancelled),p=self.slots.acquire()=>p.map_err(|_|Error::Cancelled)?};
        let epoch = self.epochs[index];
        let bootstrap = cell
            .get_or_try_init(|| async {
                let bytes = self
                    .client
                    .segment(&self.url, &self.list.segments[epoch], false, token)
                    .await?;
                tokio::task::spawn_blocking(move || Bootstrap::new(bytes))
                    .await
                    .map_err(|_| Error::Unavailable)?
            })
            .await?
            .clone();
        let bytes = if index == epoch {
            None
        } else {
            Some(
                self.client
                    .segment(&self.url, &self.list.segments[index], false, token)
                    .await?,
            )
        };
        let start = self.start[index];
        let epoch_start = self.start[epoch];
        let cancel = token.clone();
        tokio::task::spawn_blocking(move || {
            bootstrap
                .window(
                    index,
                    bytes.as_deref().unwrap_or_else(|| bootstrap.first()),
                    start,
                    epoch_start,
                    &cancel,
                )
                .map(Arc::new)
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
    fn ensure(self: &Arc<Self>, s: &mut State, index: usize) -> Arc<Job> {
        if let Some(job) = s.jobs.get(&index) {
            return job.clone();
        }
        let (done, _) = watch::channel(None);
        let job = Arc::new(Job {
            token: self.stop.child_token(),
            done,
        });
        s.jobs.insert(index, job.clone());
        let host = self.clone();
        let cell = s.bootstrap.clone();
        let own = job.clone();
        tokio::spawn(async move {
            let mut result = host.build(index, &cell, &own.token).await;
            if let Ok(mut s) = host.state.lock() {
                if s.jobs.get(&index).is_some_and(|j| Arc::ptr_eq(j, &own)) {
                    s.jobs.remove(&index);
                    if !own.token.is_cancelled() {
                        if let Ok(w) = &result {
                            if let Some(init) = &s.init {
                                if init != &w.initialization {
                                    result = Err(Error::Unavailable);
                                }
                            }
                        }
                        if let Ok(w) = &result {
                            s.init.get_or_insert_with(|| w.initialization.clone());
                            if w.media.len() <= CACHE_BYTES {
                                while s.bytes + w.media.len() > CACHE_BYTES {
                                    let Some((_, evicted)) = s.cache.pop_front() else {
                                        break;
                                    };
                                    s.bytes -= evicted.len();
                                }
                                s.bytes += w.media.len();
                                s.cache.push_back((index, w.media.clone()));
                            }
                        }
                    }
                }
            }
            if own.token.is_cancelled() {
                result = Err(Error::Cancelled);
            }
            own.done.send_replace(Some(result));
        });
        job
    }
    pub async fn resource(self: &Arc<Self>, current: usize, init: bool) -> Result<Bytes> {
        let job = {
            let mut s = self.state.lock().map_err(|_| Error::Storage)?;
            if self.stop.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let epoch = *self.epochs.get(current).ok_or(Error::Unavailable)?;
            if init && current != epoch {
                return Err(Error::Unavailable);
            }
            if s.epoch != epoch {
                for (_, job) in s.jobs.drain() {
                    job.token.cancel();
                }
                s.cache.clear();
                s.bytes = 0;
                s.init = None;
                s.bootstrap = Arc::default();
                s.epoch = epoch;
            }
            if init {
                if let Some(bytes) = &s.init {
                    return Ok(bytes.clone());
                }
            }
            let mut end = current;
            let mut ahead = 0.;
            while self.epochs.get(end + 1) == Some(&epoch) && end - current < 4 && ahead < 20. {
                end += 1;
                ahead += self.list.segments[end].seconds;
            }
            s.jobs.retain(|i, job| {
                if *i < current || *i > end {
                    job.token.cancel();
                    false
                } else {
                    true
                }
            });
            let hit = s
                .cache
                .iter()
                .position(|(i, _)| *i == current)
                .and_then(|pos| s.cache.remove(pos));
            let job = if hit.is_none() {
                Some(self.ensure(&mut s, current))
            } else {
                None
            };
            for next in current + 1..=end {
                if !s.cache.iter().any(|(i, _)| *i == next) {
                    self.ensure(&mut s, next);
                }
            }
            if let Some(entry) = hit {
                let bytes = entry.1.clone();
                s.cache.push_back(entry);
                return Ok(bytes);
            }
            job.ok_or(Error::Unavailable)?
        };
        let mut watch = job.done.subscribe();
        let window = tokio::select! {biased;_=self.stop.cancelled()=>return Err(Error::Cancelled),v=watch.wait_for(Option::is_some)=>v.map_err(|_|Error::Cancelled)?.clone().ok_or(Error::Unavailable)??};
        Ok(if init {
            window.initialization.clone()
        } else {
            window.media.clone()
        })
    }
}
