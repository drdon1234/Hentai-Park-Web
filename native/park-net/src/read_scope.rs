use crate::{public_https, Error, Request};
use http::Method;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::{Origin, Url};

enum Observed {
    Empty,
    One(Origin, CancellationToken),
    Ineligible,
}

/// 首次读取只记录网络操作的来源。手动确认后的读取在发出每一跳前执行同一约束。
pub struct ReadScope {
    observed: Mutex<Observed>,
    required: Option<(Origin, CancellationToken)>,
}

impl ReadScope {
    pub fn observe() -> Self {
        Self {
            observed: Mutex::new(Observed::Empty),
            required: None,
        }
    }

    pub fn replay(&self, page: &Url) -> Option<Self> {
        let state = self.observed.lock().ok()?;
        let Observed::One(origin, generation) = &*state else {
            return None;
        };
        if origin != &page.origin() || generation.is_cancelled() {
            return None;
        }
        Some(Self {
            observed: Mutex::new(Observed::Empty),
            required: Some((origin.clone(), generation.clone())),
        })
    }

    pub fn active(&self) -> bool {
        self.required.as_ref().is_none_or(|(_, token)| {
            !token.is_cancelled()
                && self
                    .observed
                    .lock()
                    .is_ok_and(|state| !matches!(*state, Observed::Ineligible))
        })
    }

    pub fn side_effect(&self) -> Result<(), Error> {
        *self.observed.lock().map_err(|_| Error::Blocked)? = Observed::Ineligible;
        if self.required.is_some() {
            Err(Error::Blocked)
        } else {
            Ok(())
        }
    }

    pub(crate) fn hop(
        &self,
        request: &Request,
        generation: &CancellationToken,
    ) -> Result<(), Error> {
        if !self.active() {
            return Err(Error::Cancelled);
        }
        if !matches!(request.method, Method::GET | Method::HEAD)
            || request.body.is_some()
            || !public_https(&request.url)
        {
            return self.side_effect();
        }
        let origin = request.url.origin();
        if self
            .required
            .as_ref()
            .is_some_and(|(expected, _)| *expected != origin)
        {
            return self.side_effect();
        }
        let mut state = self.observed.lock().map_err(|_| Error::Blocked)?;
        match &*state {
            Observed::Empty => *state = Observed::One(origin, generation.clone()),
            Observed::One(previous, token) if *previous != origin || token.is_cancelled() => {
                *state = Observed::Ineligible
            }
            _ => {}
        }
        Ok(())
    }
}
