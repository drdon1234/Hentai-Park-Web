CREATE TABLE IF NOT EXISTS "works" ("source" TEXT NOT NULL, "id" TEXT NOT NULL, "title" TEXT NOT NULL DEFAULT '', "cover" TEXT NOT NULL DEFAULT '', "json" TEXT NOT NULL DEFAULT '{}', "favorite" INTEGER NOT NULL DEFAULT 0 CHECK ("favorite" IN (0, 1)), "favorited_at" INTEGER NULL, "last_update_check" INTEGER NULL, "reader_mode" INTEGER NULL, "one_shot" INTEGER NOT NULL DEFAULT 0 CHECK ("one_shot" IN (0, 1)), PRIMARY KEY ("source", "id"));
CREATE TABLE IF NOT EXISTS "categories" ("id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, "name" TEXT NOT NULL UNIQUE, "sort" INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS "work_categories" ("source" TEXT NOT NULL, "work_id" TEXT NOT NULL, "category_id" INTEGER NOT NULL REFERENCES categories (id) ON DELETE CASCADE, PRIMARY KEY ("source", "work_id", "category_id"));
CREATE TABLE IF NOT EXISTS "chapters" ("source" TEXT NOT NULL, "work_id" TEXT NOT NULL, "chapter_id" TEXT NOT NULL, "title" TEXT NOT NULL DEFAULT '', "sort" INTEGER NOT NULL DEFAULT 0, "read" INTEGER NOT NULL DEFAULT 0 CHECK ("read" IN (0, 1)), "last_page" INTEGER NOT NULL DEFAULT 0, "page_count" INTEGER NOT NULL DEFAULT 0, "date_fetch" INTEGER NOT NULL DEFAULT 0, "date_read" INTEGER NULL, "progress_updated_at" INTEGER NULL, "position" INTEGER NOT NULL DEFAULT 0, "duration" INTEGER NOT NULL DEFAULT 0, "position_updated_at" INTEGER NULL, PRIMARY KEY ("source", "work_id", "chapter_id"));
CREATE TABLE IF NOT EXISTS "history" ("source" TEXT NOT NULL, "work_id" TEXT NOT NULL, "time" INTEGER NOT NULL, "chapter_id" TEXT NULL, "chapter_title" TEXT NULL, "page" INTEGER NOT NULL DEFAULT 0, "page_count" INTEGER NOT NULL DEFAULT 0, "position" INTEGER NULL, PRIMARY KEY ("source", "work_id"));
CREATE TABLE IF NOT EXISTS "download_tasks" ("uid" TEXT NOT NULL, "source" TEXT NOT NULL, "json" TEXT NOT NULL, "dir" TEXT NOT NULL, "created_at" INTEGER NOT NULL, PRIMARY KEY ("uid"));
CREATE TABLE IF NOT EXISTS "restore_checkpoints" ("id" INTEGER NOT NULL, "token" TEXT NOT NULL, PRIMARY KEY ("id"));
CREATE TABLE IF NOT EXISTS subscriptions (
      identity TEXT PRIMARY KEY, source TEXT NOT NULL, dimension TEXT NOT NULL,
      target TEXT NOT NULL, label TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1,
      notify INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL,
      UNIQUE(source,dimension,target));
CREATE TABLE IF NOT EXISTS subscription_checks (
      identity TEXT PRIMARY KEY REFERENCES subscriptions(identity) ON DELETE CASCADE,
      baseline INTEGER NOT NULL DEFAULT 0, last_success INTEGER NOT NULL DEFAULT 0,
      next_due INTEGER NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0,
      error_code TEXT NOT NULL DEFAULT '', progress TEXT NOT NULL DEFAULT '{}',
      context_hash TEXT NOT NULL DEFAULT '',
      seen_count INTEGER NOT NULL DEFAULT 0,
      lease_owner TEXT NOT NULL DEFAULT '', lease_until INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS subscription_seen (
      identity TEXT NOT NULL REFERENCES subscriptions(identity) ON DELETE CASCADE,
      content_id TEXT NOT NULL, PRIMARY KEY(identity,content_id));
CREATE TABLE IF NOT EXISTS subscription_events (
      event_key TEXT PRIMARY KEY, source TEXT NOT NULL, content_id TEXT NOT NULL,
      identity TEXT NOT NULL,
      title TEXT NOT NULL, route TEXT NOT NULL, created_at INTEGER NOT NULL,
      unread INTEGER NOT NULL DEFAULT 1, notify INTEGER NOT NULL DEFAULT 1,
      notification_id INTEGER NOT NULL, delivered INTEGER NOT NULL DEFAULT 0,
      attempts INTEGER NOT NULL DEFAULT 0, next_attempt INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_subscription_events_pending ON subscription_events(delivered,next_attempt);
CREATE TABLE IF NOT EXISTS subscription_event_targets (
      event_key TEXT NOT NULL REFERENCES subscription_events(event_key) ON DELETE CASCADE,
      identity TEXT NOT NULL REFERENCES subscriptions(identity) ON DELETE CASCADE,
      PRIMARY KEY(event_key,identity));
CREATE TABLE IF NOT EXISTS subscription_meta (name TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tavern_drafts (id TEXT PRIMARY KEY, content TEXT NOT NULL, portrait BLOB);
CREATE TABLE IF NOT EXISTS tavern_resources (
    uid TEXT PRIMARY KEY, item_json TEXT NOT NULL, kind TEXT NOT NULL,
    format TEXT NOT NULL, card_json TEXT NOT NULL, original BLOB NOT NULL,
    saved_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS tavern_edits (
    revision TEXT PRIMARY KEY, uid TEXT NOT NULL REFERENCES tavern_resources(uid) ON DELETE CASCADE,
    saved_at INTEGER NOT NULL, content TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_tavern_edits ON tavern_edits(uid,saved_at DESC);
CREATE TRIGGER IF NOT EXISTS remember_tavern_edit BEFORE UPDATE OF card_json ON tavern_resources
    WHEN OLD.card_json <> NEW.card_json BEGIN
    INSERT INTO tavern_edits VALUES(lower(hex(randomblob(16))),OLD.uid,OLD.saved_at,OLD.card_json);
    END;
CREATE TRIGGER IF NOT EXISTS prune_tavern_edits AFTER INSERT ON tavern_edits BEGIN
    DELETE FROM tavern_edits WHERE uid=NEW.uid AND revision NOT IN
      (SELECT revision FROM tavern_edits WHERE uid=NEW.uid ORDER BY saved_at DESC,rowid DESC LIMIT 8);
    END;
CREATE TABLE IF NOT EXISTS tavern_sessions (
    id TEXT PRIMARY KEY, resource_uid TEXT NOT NULL REFERENCES tavern_resources(uid) ON DELETE RESTRICT,
    data_json TEXT NOT NULL, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS idx_tavern_sessions_updated ON tavern_sessions(updated_at);
CREATE INDEX IF NOT EXISTS idx_works_favorite ON works (favorite);
CREATE INDEX IF NOT EXISTS idx_chapters_fetch ON chapters (date_fetch);
PRAGMA user_version = 10;
