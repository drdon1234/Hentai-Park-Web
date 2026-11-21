use base64::prelude::{Engine, BASE64_STANDARD};
use rquickjs::{Context, Ctx, Function, Persistent, Promise, Runtime};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("CANCELLED")]
    Cancelled,
    #[error("TIMEOUT")]
    Timeout,
    #[error("PARSE")]
    Script,
    #[error("CONFIG")]
    Config,
    #[error("UNSUPPORTED")]
    Unsupported,
    #[error("NETWORK")]
    Network,
    #[error("BLOCKED")]
    Blocked,
    #[error("STORAGE")]
    Storage,
}

#[derive(Clone)]
pub struct Invocation {
    pub(crate) cancelled: CancellationToken,
    pub(crate) read_scope: Option<std::sync::Arc<park_net::ReadScope>>,
    deadline: Instant,
}
impl Invocation {
    pub fn child(timeout: Duration, parent: &CancellationToken) -> Self {
        Self {
            cancelled: parent.child_token(),
            read_scope: None,
            deadline: Instant::now() + timeout,
        }
    }
    pub fn new(timeout: Duration) -> Self {
        Self {
            cancelled: CancellationToken::new(),
            read_scope: None,
            deadline: Instant::now() + timeout,
        }
    }
    pub fn cancel(&self) {
        self.cancelled.cancel()
    }
    pub fn with_read_scope(mut self, scope: std::sync::Arc<park_net::ReadScope>) -> Self {
        self.read_scope = Some(scope);
        self
    }
    pub fn check(&self) -> Result<(), Error> {
        if self.cancelled.is_cancelled() || self.read_scope.as_ref().is_some_and(|scope| !scope.active()) {
            Err(Error::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(Error::Timeout)
        } else {
            Ok(())
        }
    }
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// 由宿主实施能力授权和域限制。归属来自实例，调用者不能覆盖。
pub trait Capabilities: 'static {
    fn call(
        &self,
        source: &str,
        operation: &str,
        args: Value,
        invocation: &Invocation,
    ) -> Result<Value, Error>;
}

fn settle<'js>(
    ctx: &Ctx<'js>,
    promise: Promise<'js>,
    timers: &Function<'js>,
    budget: &Invocation,
    finish_invocation: bool,
) -> rquickjs::Result<String> {
    loop {
        if budget.check().is_err() {
            return Err(rquickjs::Error::WouldBlock);
        }
        if ctx.execute_pending_job() {
            continue;
        }
        if let Some(result) = promise.result() {
            if finish_invocation {
                timers.call::<_, i32>((true,))?;
            }
            return result;
        }
        let delay: i32 = timers.call((false,))?;
        if delay < 0 {
            return Err(rquickjs::Error::WouldBlock);
        }
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay as u64));
        }
    }
}

fn decode_text(label: String, bytes: String, fatal: bool, ignore_bom: bool) -> Option<String> {
    let encoding = encoding_rs::Encoding::for_label_no_replacement(label.as_bytes())?;
    let bytes = BASE64_STANDARD.decode(bytes).ok()?;
    let (text, invalid) = if ignore_bom {
        encoding.decode_without_bom_handling(&bytes)
    } else {
        encoding.decode_with_bom_removal(&bytes)
    };
    if fatal && invalid {
        None
    } else {
        Some(text.into_owned())
    }
}

fn base64_encode(text: String) -> Option<String> {
    let bytes = text
        .chars()
        .map(|v| u8::try_from(v as u32).ok())
        .collect::<Option<Vec<_>>>()?;
    Some(BASE64_STANDARD.encode(bytes))
}

pub struct SourceRuntime {
    timers: Persistent<Function<'static>>,
    reusable: bool,
    context: Context,
    active: Rc<RefCell<Invocation>>,
    key: String,
    manifest: Value,
    _lease: crate::pool::EngineLease,
}

impl SourceRuntime {
    pub fn inspect(
        key: &str,
        script: &str,
        browser_apis: &str,
        call: Invocation,
    ) -> Result<Value, Error> {
        Self::inspect_script(Some(key), script, browser_apis, call)
    }
    pub fn inspect_script(
        expected: Option<&str>,
        script: &str,
        browser_apis: &str,
        call: Invocation,
    ) -> Result<Value, Error> {
        struct NoCapabilities;
        impl Capabilities for NoCapabilities {
            fn call(&self, _: &str, _: &str, _: Value, _: &Invocation) -> Result<Value, Error> {
                Err(Error::Blocked)
            }
        }
        Ok(Self::create(
            expected,
            script,
            browser_apis,
            &Value::Null,
            Rc::new(NoCapabilities),
            call,
            None,
        )?
        .manifest
        .clone())
    }
    pub fn open(
        key: &str,
        script: &str,
        browser_apis: &str,
        state: &Value,
        host: Rc<dyn Capabilities>,
        call: Invocation,
        install: bool,
    ) -> Result<Self, Error> {
        Self::create(
            Some(key),
            script,
            browser_apis,
            state,
            host,
            call,
            Some(install),
        )
    }
    fn create(
        expected: Option<&str>,
        script: &str,
        browser_apis: &str,
        state: &Value,
        host: Rc<dyn Capabilities>,
        call: Invocation,
        install: Option<bool>,
    ) -> Result<Self, Error> {
        if expected.is_some_and(|key| !crate::source_key(key))
            || (install.is_some() && expected.is_none())
        {
            return Err(Error::Config);
        }
        let key = expected.unwrap_or("");
        let lease = crate::pool::EngineLease::acquire(&call)?;
        let runtime = Runtime::new().map_err(|_| Error::Script)?;
        runtime.set_memory_limit(64 * 1024 * 1024);
        runtime.set_max_stack_size(1024 * 1024);
        let active = Rc::new(RefCell::new(call));
        let interrupt = active.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || interrupt.borrow().check().is_err())));
        let context = Context::full(&runtime).map_err(|_| Error::Script)?;
        let owner = key.to_owned();
        let budget = active.clone();
        let result = context.with(|ctx| -> rquickjs::Result<_> {
            let global = ctx.globals();
            let dispatch = move |op: String, args: String| {
                let call = budget.borrow().clone();
                let response = call.check().and_then(|_| {
                    let input = serde_json::from_str(&args).map_err(|_| Error::Config)?;
                    let value = host.call(&owner, &op, input, &call)?;
                    call.check()?;
                    Ok(value)
                });
                match response {
                    Ok(value) => json!({"ok": true,"value":value}),
                    Err(error) => json!({"ok":false,"code":error.to_string()})
                }.to_string()
            };
            global.set("parkHostCall",Function::new(ctx.clone(),dispatch)?)?;
            global.set("parkBase64Decode",Function::new(ctx.clone(),|s: String| -> Option<String> {
                Some(BASE64_STANDARD.decode(s).ok()?.into_iter().map(char::from).collect())
            })?)?;
            global.set("parkBase64Encode",Function::new(ctx.clone(),base64_encode)?)?;
            global.set("parkDecodeText",Function::new(ctx.clone(),decode_text)?)?;
            global.set("parkEncoding",Function::new(ctx.clone(),|label:String| {
                encoding_rs::Encoding::for_label_no_replacement(label.as_bytes()).map(|enc| enc.name().to_ascii_lowercase())
            })?)?;
            global.set("parkRandom",Function::new(ctx.clone(),|length:u32| -> Option<String> {
                if length > 65536 { return None; }
                let mut bytes=vec![0;length as usize];
                getrandom::fill(&mut bytes).ok()?;
                Some(BASE64_STANDARD.encode(bytes))
            })?)?;
            let start=Instant::now();
            global.set("parkMonotonic", Function::new(ctx.clone(),move || start.elapsed().as_secs_f64()*1000.0)?)?;
            let timers:Function = ctx.eval(include_str!("host.js"))?;
            ctx.eval::<(),_>(browser_apis)?;
            ctx.eval::<(),_>(include_str!("../../../assets/runtime/park_runtime.js"))?;
            ctx.eval::<(),_>(script)?;
            let value=if let Some(wait)=install {
                let setup:Function=ctx.eval("(key,state,install) => { if (Park.__pending?.key !== key) throw Error(); return Park.setup(key, JSON.parse(state), {waitForInit:install}).then(JSON.stringify) }")?;
                let promise:Promise=setup.call((key,state.to_string(),wait))?;
                settle(&ctx,promise,&timers,&active.borrow(),wait)?
            }else{
                let manifest:Function=ctx.eval("expected => {const key=Park.__pending?.key;if(!key||(expected && key!==expected))throw Error();return JSON.stringify(Park.manifest(key))}")?;
                manifest.call((key,))?
            };
            Ok((value,Persistent::save(&ctx,timers)))
        });
        active.borrow().check()?;
        let (value, timers) = result.map_err(|_| Error::Script)?;
        let manifest: Value = serde_json::from_str(&value).map_err(|_| Error::Script)?;
        let key = manifest["key"]
            .as_str()
            .filter(|k| crate::source_key(k) && expected.is_none_or(|e| e == *k))
            .ok_or(Error::Config)?
            .to_owned();
        Ok(Self {
            timers,
            reusable: true,
            context,
            active,
            key,
            manifest,
            _lease: lease,
        })
    }

    pub fn manifest(&self) -> &Value {
        &self.manifest
    }

    pub fn invoke(
        &mut self,
        method: &str,
        args: &[Value],
        call: Invocation,
    ) -> Result<Value, Error> {
        call.check()?;
        if !self.reusable {
            return Err(Error::Cancelled);
        }
        *self.active.borrow_mut() = call.clone();
        let result = self.context.with(|ctx| -> rquickjs::Result<String> {
            let invoke: Function =
                ctx.eval("(k,m,args) => Park.invoke(k,m,JSON.parse(args)).then(JSON.stringify)")?;
            let promise: Promise = invoke.call((
                self.key.as_str(),
                method,
                Value::Array(args.to_vec()).to_string(),
            ))?;
            settle(
                &ctx,
                promise,
                &self.timers.clone().restore(&ctx)?,
                &call,
                true,
            )
        });
        // 出错或中断后的 JS 队列不可移交给下一次调用。
        self.reusable = result.is_ok() && call.check().is_ok();
        call.check()?;
        serde_json::from_str(&result.map_err(|_| Error::Script)?).map_err(|_| Error::Script)
    }

    /// 正常卸载时确认插件的最后一次持久写入。中断实例仅释放，防止迟到状态覆盖磁盘。
    pub fn flush(&mut self, call: Invocation) -> Result<(), Error> {
        call.check()?;
        if !self.reusable {
            return Err(Error::Cancelled);
        }
        *self.active.borrow_mut() = call.clone();
        let outcome = self.context.with(|ctx| -> rquickjs::Result<String> {
            let flush: Function =
                ctx.eval("() => Park.__pending.store.flush().then(() => 'ok')")?;
            settle(
                &ctx,
                flush.call(())?,
                &self.timers.clone().restore(&ctx)?,
                &call,
                true,
            )
        });
        self.reusable = outcome.is_ok() && call.check().is_ok();
        call.check()?;
        outcome.map(|_| ()).map_err(|_| Error::Storage)
    }

    pub fn close(mut self, call: Invocation) -> Result<(), Error> {
        self.flush(call)
    }
}
