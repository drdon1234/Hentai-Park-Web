use crate::{Error, Invocation};
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::Duration;

// 每个租约对应一个独立的 QuickJS 堆。所有宿主线程合计最多六个活跃实例。
const CAPACITY: usize = 6;
static POOL: LazyLock<(Mutex<usize>, Condvar)> = LazyLock::new(|| (Mutex::new(0), Condvar::new()));

pub(crate) struct EngineLease;

impl EngineLease {
    pub(crate) fn acquire(call: &Invocation) -> Result<Self, Error> {
        let (mutex, changed) = &*POOL;
        let mut used = mutex.lock().map_err(|_| Error::Storage)?;
        loop {
            call.check()?;
            if *used < CAPACITY {
                *used += 1;
                return Ok(Self);
            }
            // 请求可从任一平台线程取消，即使其他引擎仍在运行也会退出等待。
            used = changed
                .wait_timeout(used, Duration::from_millis(10))
                .map_err(|_| Error::Storage)?
                .0;
        }
    }
}
impl Drop for EngineLease {
    fn drop(&mut self) {
        let (mutex, changed) = &*POOL;
        let mut used = mutex.lock().unwrap_or_else(|error| error.into_inner());
        *used -= 1;
        changed.notify_one();
    }
}
