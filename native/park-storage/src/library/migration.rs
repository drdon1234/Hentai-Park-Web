use regex::Regex;
use rusqlite::{params, Connection, Result, TransactionBehavior};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

static NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)第\s*(\d+(?:\.\d+)?)\s*[话話章回集篇节節部]|(?:chapter|chap|ch|episode|ep|#)\.?\s*(\d+(?:\.\d+)?)").unwrap()
});
static HAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"第\s*([零〇一二两三四五六七八九十百千]+)\s*[话話章回集篇节節部]").unwrap());
static VOLUME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)vol(?:ume)?\.?\s*\d+|第\s*\d+\s*[卷册冊]").unwrap());
static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+(?:\.\d+)?").unwrap());
static SEPARATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\s\p{P}\p{S}]+").unwrap());

fn chapter_number(title: &str) -> Option<String> {
    if let Some(captures) = NUMBER.captures(title) {
        return captures.get(1).or(captures.get(2))?.as_str().parse::<f64>().ok().map(|n| n.to_string());
    }
    if let Some(captures) = HAN.captures(title) {
        let mut total = 0u64;
        let mut digits = 0u64;
        for ch in captures[1].chars() {
            let unit = match ch { '十' => 10, '百' => 100, '千' => 1000, _ => 0 };
            if unit != 0 {
                total = total.checked_add(digits.max(1).checked_mul(unit)?)?;
                digits = 0;
            } else {
                let digit = "零一二三四五六七八九".chars().position(|d| d == ch)
                    .or(match ch { '〇' => Some(0), '两' => Some(2), _ => None })?;
                digits = digits.checked_mul(10)?.checked_add(digit as u64)?;
            }
        }
        return Some(total.checked_add(digits)?.to_string());
    }
    DIGITS.find_iter(&VOLUME.replace_all(title, " ")).last()?.as_str().parse::<f64>().ok().map(|n| n.to_string())
}

type Chapter = (String, String);

fn unique_indices(chapters: &[Chapter], key: fn(&str) -> Option<String>) -> HashMap<String, Option<usize>> {
    let mut result = HashMap::new();
    for (index, (_, title)) in chapters.iter().enumerate() {
        if let Some(k) = key(title).filter(|v| !v.is_empty()) {
            result.entry(k).and_modify(|v| *v = None).or_insert(Some(index));
        }
    }
    result
}

fn chapter_matches(old: &[Chapter], fresh: &[Chapter]) -> HashMap<usize, usize> {
    let mut matches = HashMap::new();
    let mut used = HashSet::new();
    let exact = |s: &str| Some(SEPARATORS.replace_all(&s.to_lowercase(), "").into_owned());
    for key in [exact as fn(&str) -> Option<String>, chapter_number] {
        let new = unique_indices(fresh, key);
        for (name,index) in unique_indices(old, key) {
            if let (Some(from), Some(Some(&to))) = (index, new.get(&name).map(Option::as_ref)) {
                if !matches.contains_key(&from) && used.insert(to) { matches.insert(from,to); }
            }
        }
    }
    matches
}

pub struct Migration<'a> {
    pub from_source: &'a str,
    pub from_id: &'a str,
    pub to_source: &'a str,
    pub detail: &'a Value,
    pub categories: bool,
    pub progress: bool,
    pub remove: bool,
}

/// 在一次提交内合并目标收藏与可明确匹配的进度。保留原作品的历史、下载及章节。
pub fn apply(db: &mut Connection, plan: Migration<'_>, now: i64) -> Result<bool> {
    let Migration { from_source,from_id,to_source,detail,categories,progress,remove } = plan;
    if from_source == to_source { return Err(rusqlite::Error::InvalidQuery); }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.query_row("SELECT 1 FROM works WHERE source=? AND id=? AND favorite=1",params![from_source,from_id], |_| Ok(()))?;
    let to_id = super::upsert_summary(&tx,to_source,detail)?;
    tx.execute("UPDATE works SET favorite=1,favorited_at=? WHERE source=? AND id=? AND favorite=0",params![now,to_source,to_id])?;
    if categories {
        tx.execute("INSERT OR IGNORE INTO work_categories(source,work_id,category_id)
            SELECT ?1,?2,category_id FROM work_categories WHERE source=?3 AND work_id=?4",params![to_source,to_id,from_source,from_id])?;
    }
    let mut fresh = Vec::new();
    let game = detail["mediaKind"]=="game";
    if !game {
        if let Some(chapters) = detail["chapters"].as_array() {
            let mut seen = HashSet::new();
            for c in chapters {
                let id = c["id"].as_str().filter(|id| !id.is_empty()).ok_or(rusqlite::Error::InvalidQuery)?;
                if !seen.insert(id) { return Err(rusqlite::Error::InvalidQuery); }
                fresh.push((id.to_owned(),c["title"].as_str().ok_or(rusqlite::Error::InvalidQuery)?.to_owned()));
            }
        } else if detail["chapters"].is_null() {
            fresh.push((detail["singleChapterId"].as_str().filter(|id| !id.is_empty()).unwrap_or("_").to_owned(),
                detail["title"].as_str().unwrap_or_default().to_owned()));
        } else { return Err(rusqlite::Error::InvalidQuery); }
        for (sort,(id,title)) in fresh.iter().enumerate() {
            tx.execute("INSERT INTO chapters(source,work_id,chapter_id,title,sort) VALUES(?,?,?,?,?)
                ON CONFLICT(source,work_id,chapter_id) DO UPDATE SET title=excluded.title,sort=excluded.sort",
                params![to_source,to_id,id,title,sort])?;
        }
        if detail["chaptersComplete"]!=false {
            tx.execute("UPDATE works SET one_shot=? WHERE source=? AND id=?",params![detail["chapters"].is_null(),to_source,to_id])?;
        }
    }
    let mut unmatched = false;
    if progress && !game {
        let old = {
            let mut stmt = tx.prepare("SELECT chapter_id,title FROM chapters WHERE source=? AND work_id=? ORDER BY sort,chapter_id")?;
            let rows = stmt.query_map(params![from_source,from_id], |r| Ok((r.get(0)?,r.get(1)?)))?;
            rows.collect::<Result<Vec<Chapter>>>()?
        };
        let matches = chapter_matches(&old,&fresh);
        for (i,(source_chapter,_)) in old.iter().enumerate() {
            let Some(&j) = matches.get(&i) else {
                unmatched |= tx.query_row("SELECT read OR last_page>0 OR position>0 OR progress_updated_at IS NOT NULL
                    OR position_updated_at IS NOT NULL FROM chapters WHERE source=? AND work_id=? AND chapter_id=?",
                    params![from_source,from_id,source_chapter], |r| r.get::<_,bool>(0))?;
                continue;
            };
            let (target_chapter,title) = &fresh[j];
            let keys = params![from_source,from_id,source_chapter,to_source,to_id,target_chapter];
            tx.execute("WITH merged AS (
                SELECT o.*, (coalesce(o.progress_updated_at,-1)>coalesce(n.progress_updated_at,-1)
                    OR (o.progress_updated_at IS NULL AND n.progress_updated_at IS NULL AND n.last_page=0 AND o.last_page>0)) AS take_page,
                    (coalesce(o.position_updated_at,-1)>coalesce(n.position_updated_at,-1)
                    OR (o.position_updated_at IS NULL AND n.position_updated_at IS NULL AND n.position=0 AND o.position>0)) AS take_position,
                    (max(coalesce(o.progress_updated_at,-1),coalesce(o.position_updated_at,-1))>
                     max(coalesce(n.progress_updated_at,-1),coalesce(n.position_updated_at,-1))
                     OR (n.progress_updated_at IS NULL AND n.position_updated_at IS NULL AND n.read=0 AND o.read=1)) AS take_read
                FROM chapters o,chapters n WHERE o.source=?1 AND o.work_id=?2 AND o.chapter_id=?3
                    AND n.source=?4 AND n.work_id=?5 AND n.chapter_id=?6)
                UPDATE chapters AS d SET last_page=iif(m.take_page,m.last_page,d.last_page),page_count=iif(m.take_page,m.page_count,d.page_count),
                    progress_updated_at=iif(m.take_page,m.progress_updated_at,d.progress_updated_at),
                    position=iif(m.take_position,m.position,d.position),duration=iif(m.take_position,m.duration,d.duration),
                    position_updated_at=iif(m.take_position,m.position_updated_at,d.position_updated_at),
                    read=iif(m.take_read,m.read,d.read),date_read=iif(m.take_read,m.date_read,d.date_read)
                FROM merged m WHERE d.source=?4 AND d.work_id=?5 AND d.chapter_id=?6",keys)?;
            tx.execute("INSERT INTO history(source,work_id,time,chapter_id,chapter_title,page,page_count,position)
                SELECT ?4,?5,time,?6,?7,page,page_count,position FROM history WHERE source=?1 AND work_id=?2 AND coalesce(chapter_id,'_')=?3
                ON CONFLICT(source,work_id) DO UPDATE SET time=excluded.time,chapter_id=excluded.chapter_id,chapter_title=excluded.chapter_title,
                page=excluded.page,page_count=excluded.page_count,position=excluded.position WHERE excluded.time>history.time",
                params![from_source,from_id,source_chapter,to_source,to_id,target_chapter,title])?;
        }
    }
    let removed = remove && !unmatched;
    if removed { tx.execute("UPDATE works SET favorite=0,favorited_at=NULL WHERE source=? AND id=?",params![from_source,from_id])?; }
    tx.commit()?;
    Ok(removed)
}
