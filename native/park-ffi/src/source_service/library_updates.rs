use super::*;
use crate::ProgressStore;
use park_storage::library::updates as library;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::path::Path;

fn update_lock(store: &ProgressStore) -> Result<Option<std::fs::File>> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(store.root.join("library_update.lock")).map_err(|_| SourceFailure::Storage)?;
    let acquired = park_storage::recovery::try_lock_file(&file, true).map_err(|_| SourceFailure::Storage)?;
    Ok(acquired.then_some(file))
}

fn supports_updates(manifest: &Value, count: i64) -> bool {
    let media = manifest["type"].as_str().unwrap_or("");
    let domain = manifest["category"].as_str().unwrap_or("");
    !matches!(media, "article" | "game" | "live")
        && !matches!(domain, "live" | "shortvideo" | "onlineGame" | "game")
        && manifest["metadataOnly"] != true && manifest["capabilities"]["detail"] == true
        && (!matches!(media, "video" | "voice") || manifest["capabilities"]["videos"] == true)
        && (matches!(domain, "doujin" | "anime" | "aiDrama" | "novel") || count >= 2)
}

pub(super) fn now_ms() -> Result<i64> {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .ok().and_then(|v|v.as_millis().try_into().ok()).ok_or(SourceFailure::Storage)
}
pub(super) fn settings(store:&ProgressStore)->Result<Value> {
    match std::fs::read(store.root.join("settings.json")) {
        Ok(data)=>serde_json::from_slice(&data).map_err(|_|SourceFailure::Storage),
        Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(json!({})),
        Err(_)=>Err(SourceFailure::Storage)
    }
}

#[uniffi::export]
impl SourceService {
    pub fn check_favorites(&self,store:Arc<ProgressStore>,category:Option<i64>,download_directory:String,
                           force:bool,request:Arc<Request>)->Result<String> {
        let Some(_lock) = update_lock(&store)? else {
            return Ok(json!({"busy":true}).to_string())
        };
        let prefs=settings(&store)?;
        let now=now_ms()?;
        let hours=prefs["libraryUpdateHours"].as_i64().unwrap_or(0);
        if !force && (hours<=0 || now.saturating_sub(prefs["lastLibraryUpdate"].as_i64().unwrap_or(0))<hours.saturating_mul(3600000)) {
            return Ok(json!({"due":false}).to_string())
        }
        let directory = Path::new(&download_directory);
        let auto=prefs["autoDownloadNew"]==true;
        let installed:HashSet<_>=self.catalog.installed()?.into_iter().map(|s|s.key).collect();
        let rows={let db=store.lock().map_err(|_|SourceFailure::Storage)?;
            library::targets(&db,category,&prefs).map_err(|_|SourceFailure::Storage)?};
        let mut groups=BTreeMap::<String,Vec<Value>>::new();
        for row in rows {
            let Some(key)=row["source"].as_str() else{continue};
            if installed.contains(key){groups.entry(key.to_owned()).or_default().push(row)}
        }
        let groups=Mutex::new(VecDeque::from_iter(groups));
        let mut totals=[0usize;4];
        std::thread::scope(|scope| -> Result<()> {
            let workers:Vec<_>=(0..4).map(|_|scope.spawn(||->Result<[usize;4]>{
                let mut result=[0usize;4];
                loop {
                    if request.cancelled.is_cancelled(){return Err(SourceFailure::Cancelled)}
                    let group=groups.lock().map_err(|_|SourceFailure::Storage)?.pop_front();
                    let Some((key,rows))=group else{break};
                    let manifest=match self.catalog.manifest(&key,plugins::budget(&request)) {
                        Ok(v)=>v,Err(park_runtime::Error::Cancelled)=>return Err(SourceFailure::Cancelled),
                        Err(_)=>{result[2]+=rows.len();continue}
                    };
                    for row in rows {
                        if !supports_updates(&manifest, row["count"].as_i64().unwrap_or(0)) {continue}
                        match self.refresh_favorite(&store,&key,&manifest,&row,auto.then_some(directory),&request){
                            Ok(fresh)=>{result[0]+=1;result[1]+=fresh;result[3]+=usize::from(fresh>0)}
                            Err(SourceFailure::Cancelled)=>return Err(SourceFailure::Cancelled),
                            Err(_)=>result[2]+=1
                        }
                    }
                }
                Ok(result)
            })).collect();
            for worker in workers {
                let result=worker.join().map_err(|_|SourceFailure::Unavailable)??;
                for (total,n) in totals.iter_mut().zip(result){*total+=n}
            }
            Ok(())
        })?;
        if request.cancelled.is_cancelled(){return Err(SourceFailure::Cancelled)}
        if category.is_none(){
            let _gate=park_storage::recovery::gate(&store.root,true).map_err(|_|SourceFailure::Storage)?;
            park_storage::backup::set_preferences(&store.root,[("lastLibraryUpdate".into(),now.into())]).map_err(|_|SourceFailure::Storage)?;
        }
        Ok(json!({"checked":totals[0],"chapters":totals[1],"failed":totals[2],"updated":totals[3]}).to_string())
    }
}
impl SourceService {
    fn refresh_favorite(&self,store:&ProgressStore,key:&str,manifest:&Value,row:&Value,download:Option<&Path>,request:&Request)->Result<usize>{
        let id=row["id"].as_str().ok_or(SourceFailure::Storage)?;
        let (_,response)=self.catalog.invoke(key,"detail",&[id.into(),row["item"].clone()],plugins::budget(request))?;
        let item=downloads::checked(response)?;
        let media=match manifest["type"].as_str(){Some("video"|"voice")=>"video",Some("novel")=>"novel",_=>"comic"};
        reconcile_detail(store,key,id,manifest,item,download.map(|p|library::ChapterQueue::Library(p,media)),request).map(|fresh|fresh.len())
    }
}

fn reconcile_detail(store:&ProgressStore,key:&str,id:&str,manifest:&Value,mut item:Value,download:Option<library::ChapterQueue<'_>>,request:&Request)->Result<Vec<Value>>{
        if item["id"].as_str()!=Some(id) || item["title"].as_str().is_none_or(|t|t.trim().is_empty()) {return Err(SourceFailure::Unsupported)}
        if item["mediaKind"].is_null(){item["mediaKind"]=manifest["type"].clone()}
        if item["contentDomain"].is_null(){item["contentDomain"]=manifest["category"].clone()}
        let directory=manifest["capabilities"]["voiceDirectory"]==true && item["voiceRoot"]["type"]=="directory";
        let single=item["chapters"].is_null();
        let chapters=if directory{None}else if single{
            Some(vec![(item["singleChapterId"].as_str().filter(|s|!s.is_empty()).unwrap_or("_").to_owned(),
                       item["title"].as_str().unwrap_or_default().to_owned())])
        }else{
            let list=item["chapters"].as_array().ok_or(SourceFailure::Unsupported)?;
            Some(list.iter().map(|c|Ok((c["id"].as_str().filter(|s|!s.is_empty()).ok_or(SourceFailure::Unsupported)?.to_owned(),
                c["title"].as_str().ok_or(SourceFailure::Unsupported)?.to_owned()))).collect::<Result<Vec<_>>>()?)
        };
        if request.cancelled.is_cancelled(){return Err(SourceFailure::Cancelled)}
        let mut db=store.lock().map_err(|_|SourceFailure::Storage)?;
        library::reconcile(&mut db,key,&item,chapters.as_deref(),single,item["chaptersComplete"]!=false,
            download,now_ms()?).map_err(|_|SourceFailure::Storage)
}

/// 桌面宿主通过自己的站点会话取详情，范围、锁与章节入库规则沿用共享书库。
#[derive(uniffi::Object)]
pub struct HostFavoriteCheck {
    _lock: std::fs::File,
    store: Arc<ProgressStore>,
    manifests: Value,
    targets: Vec<Value>,
    download: Option<PathBuf>,
}

#[uniffi::export]
impl ProgressStore {
    pub fn begin_host_favorite_check(self: Arc<Self>, category: Option<i64>, manifests: String,
        download_directory: String) -> Result<Option<Arc<HostFavoriteCheck>>> {
        let Some(lock) = update_lock(&self)? else { return Ok(None) };
        let manifests: Value = serde_json::from_str(&manifests).map_err(|_| SourceFailure::Unsupported)?;
        let prefs = settings(&self)?;
        let mut targets = {
            let db = self.lock().map_err(|_| SourceFailure::Storage)?;
            library::targets(&db, category, &prefs).map_err(|_| SourceFailure::Storage)?
        };
        targets.retain(|row| row["source"].as_str().is_some_and(|source|
            supports_updates(&manifests[source], row["count"].as_i64().unwrap_or(0))));
        Ok(Some(Arc::new(HostFavoriteCheck { _lock: lock, store: self, manifests, targets,
            download: (prefs["autoDownloadNew"] == true).then(|| PathBuf::from(download_directory)) })))
    }
}

#[uniffi::export]
impl HostFavoriteCheck {
    pub fn targets(&self) -> String { Value::Array(self.targets.clone()).to_string() }

    pub fn apply_detail(&self, ordinal: u64, detail: String, request: Arc<Request>) -> Result<String> {
        let row = usize::try_from(ordinal).ok().and_then(|i| self.targets.get(i)).ok_or(SourceFailure::Unsupported)?;
        let source = row["source"].as_str().ok_or(SourceFailure::Storage)?;
        let id = row["id"].as_str().ok_or(SourceFailure::Storage)?;
        let item = serde_json::from_str(&detail).map_err(|_| SourceFailure::Unsupported)?;
        let fresh = reconcile_detail(&self.store, source, id, &self.manifests[source], item,
            self.download.as_deref().map(library::ChapterQueue::Windows), &request)?;
        Ok(json!({"chapters": fresh, "autoDownload": self.download.is_some()}).to_string())
    }
}
