ALTER TABLE joplin_mcp.index_state
  ADD COLUMN IF NOT EXISTS last_checked_at timestamptz;
