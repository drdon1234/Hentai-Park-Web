use super::*;

/// 与基线旧库和备份共用身份算法及字段限制。
pub(crate) fn target_identity(source: &str, dim: &str, target: &str, label: &str) -> Result<String> {
    if !crate::recovery::valid_key(source)
        || target.is_empty()
        || target.contains('\0')
        || target.encode_utf16().count() > 2048
        || label.encode_utf16().count() > 512
        || dim.is_empty()
        || dim.len() > 32
        || !dim.as_bytes()[0].is_ascii_lowercase()
        || !dim
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(Error::Invalid);
    }
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&[source, dim, target])?)
    ))
}

pub fn save_target(
    db: &mut Connection, original: Option<&str>, source: &str, dimension: &str,
    target: &str, label: &str, now: i64,
) -> Result<bool> {
    let id = target_identity(source, dimension, target, label)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (created, enabled, notify) = if let Some(old) = original {
        let values = tx.query_row(
            "SELECT created_at,enabled,notify FROM subscriptions WHERE identity=? AND source=? AND dimension=?",
            params![old, source, dimension], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if old == id {
            tx.execute("UPDATE subscriptions SET label=? WHERE identity=?", params![label, old])?;
            tx.commit()?;
            return Ok(true);
        }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM subscriptions WHERE identity=?)", [&id], |r| r.get(0))? {
            return Ok(false);
        }
        tx.execute("DELETE FROM subscriptions WHERE identity=?", [old])?;
        values
    } else {
        (now, true, true)
    };
    tx.execute(
        "INSERT INTO subscriptions(identity,source,dimension,target,label,enabled,notify,created_at)
         VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(identity) DO UPDATE SET
         label=excluded.label,enabled=excluded.enabled,notify=excluded.notify",
        params![id, source, dimension, target, label, enabled, notify, created],
    )?;
    tx.execute(
        "INSERT INTO subscription_checks(identity) VALUES(?) ON CONFLICT(identity) DO UPDATE SET
         lease_owner='',lease_until=0", [&id],
    )?;
    tx.commit()?;
    Ok(true)
}

pub fn set_target_option(
    db: &mut Connection, id: &str, check: Option<bool>, notify: Option<bool>,
) -> Result<()> {
    if check.is_some() == notify.is_some() { return Err(Error::Invalid); }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE subscriptions SET enabled=COALESCE(?1,enabled),notify=COALESCE(?2,notify) WHERE identity=?3",
        params![check, notify, id],
    )?;
    if changed == 0 { return Err(Error::Invalid); }
    if check == Some(false) {
        tx.execute("UPDATE subscription_checks SET lease_owner='',lease_until=0 WHERE identity=?", [id])?;
    }
    tx.commit()?;
    Ok(())
}
