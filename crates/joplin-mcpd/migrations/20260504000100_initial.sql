CREATE SCHEMA IF NOT EXISTS joplin_mcp;

CREATE TABLE IF NOT EXISTS joplin_mcp.mcp_users (
  id uuid PRIMARY KEY,
  joplin_user_id text NOT NULL UNIQUE,
  joplin_email text NOT NULL,
  display_name text,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_login_at timestamptz,
  disabled_at timestamptz
);

CREATE TABLE IF NOT EXISTS joplin_mcp.mcp_tokens (
  id uuid PRIMARY KEY,
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  token_hash bytea NOT NULL UNIQUE,
  hmac_key_id text NOT NULL,
  label text NOT NULL,
  scope text NOT NULL DEFAULT 'read',
  created_at timestamptz NOT NULL DEFAULT now(),
  created_from_ip inet,
  last_seen_at timestamptz,
  revoked_at timestamptz,
  revoked_by uuid,
  revoke_reason text,
  revoked_from_ip inet,
  expires_at timestamptz
);

CREATE INDEX IF NOT EXISTS mcp_tokens_user_id_idx
  ON joplin_mcp.mcp_tokens(user_id);

CREATE TABLE IF NOT EXISTS joplin_mcp.audit_log (
  id uuid PRIMARY KEY,
  user_id uuid REFERENCES joplin_mcp.mcp_users(id) ON DELETE SET NULL,
  event_type text NOT NULL,
  outcome text NOT NULL,
  client_label text,
  remote_ip inet,
  metadata jsonb NOT NULL DEFAULT '{}',
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS audit_log_user_time_idx
  ON joplin_mcp.audit_log(user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS joplin_mcp.index_state (
  user_id uuid PRIMARY KEY REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  status text NOT NULL,
  last_full_rebuild_at timestamptz,
  last_incremental_at timestamptz,
  last_seen_joplin_updated_time bigint,
  last_error text,
  updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS joplin_mcp.notebooks_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  joplin_item_id text NOT NULL,
  joplin_id text NOT NULL,
  parent_joplin_id text,
  title text NOT NULL,
  created_time bigint,
  updated_time bigint,
  deleted_time bigint,
  indexed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, joplin_id)
);

CREATE INDEX IF NOT EXISTS notebooks_parent_idx
  ON joplin_mcp.notebooks_index(user_id, parent_joplin_id);

CREATE TABLE IF NOT EXISTS joplin_mcp.notes_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  joplin_item_id text NOT NULL,
  joplin_id text NOT NULL,
  parent_joplin_id text,
  title text NOT NULL,
  body_text text NOT NULL,
  is_todo boolean NOT NULL DEFAULT false,
  created_time bigint,
  updated_time bigint,
  deleted_time bigint,
  resource_refs text[] NOT NULL DEFAULT '{}',
  search_vector tsvector GENERATED ALWAYS AS (
    setweight(to_tsvector('simple', coalesce(title, '')), 'A') ||
    setweight(to_tsvector('simple', coalesce(body_text, '')), 'B')
  ) STORED,
  indexed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, joplin_id)
);

CREATE INDEX IF NOT EXISTS notes_parent_idx
  ON joplin_mcp.notes_index(user_id, parent_joplin_id);

CREATE INDEX IF NOT EXISTS notes_updated_idx
  ON joplin_mcp.notes_index(user_id, updated_time DESC);

CREATE INDEX IF NOT EXISTS notes_search_idx
  ON joplin_mcp.notes_index USING gin(search_vector);

CREATE TABLE IF NOT EXISTS joplin_mcp.tags_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  joplin_item_id text NOT NULL,
  joplin_id text NOT NULL,
  title text NOT NULL,
  updated_time bigint,
  indexed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, joplin_id)
);

CREATE TABLE IF NOT EXISTS joplin_mcp.note_tags_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  note_joplin_id text NOT NULL,
  tag_joplin_id text NOT NULL,
  updated_time bigint,
  indexed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, note_joplin_id, tag_joplin_id)
);

CREATE INDEX IF NOT EXISTS note_tags_by_tag_idx
  ON joplin_mcp.note_tags_index(user_id, tag_joplin_id);

CREATE TABLE IF NOT EXISTS joplin_mcp.resources_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  joplin_item_id text NOT NULL,
  joplin_id text NOT NULL,
  title text NOT NULL,
  mime text,
  size_bytes bigint,
  file_extension text,
  updated_time bigint,
  indexed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, joplin_id)
);

CREATE TABLE IF NOT EXISTS joplin_mcp.deleted_items_index (
  user_id uuid NOT NULL REFERENCES joplin_mcp.mcp_users(id) ON DELETE CASCADE,
  joplin_id text NOT NULL,
  item_type int,
  source text NOT NULL,
  deleted_time bigint,
  tombstoned_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, joplin_id)
);
