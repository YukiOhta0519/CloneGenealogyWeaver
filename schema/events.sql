CREATE TABLE run (
  run_id TEXT PRIMARY KEY,
  schema_version INTEGER NOT NULL,
  tool_version TEXT NOT NULL,
  git_version TEXT NOT NULL,
  repository TEXT NOT NULL,
  config TEXT NOT NULL,
  graph_sha256 TEXT NOT NULL,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  status TEXT NOT NULL CHECK (status IN ('running', 'finished', 'failed')),
  stop_message TEXT,
  CHECK ((stop_message IS NOT NULL) = (status = 'failed'))
);
CREATE TABLE plugin (
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('detector', 'classifier')),
  name TEXT NOT NULL,
  config TEXT NOT NULL,
  UNIQUE (kind, name),
  CHECK ((kind = 'detector') = (id = 1))
);
CREATE TABLE version (
  id INTEGER PRIMARY KEY,
  commit_id TEXT NOT NULL UNIQUE,
  topo_order INTEGER NOT NULL UNIQUE,
  committed_at TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending', 'ok', 'broken')),
  failed_plugin INTEGER REFERENCES plugin(id),
  failure_message TEXT,
  root TEXT CHECK (root IN ('empty_tree', 'censored')),
  pair_count INTEGER,
  fragment_count INTEGER,
  scope_sha256 TEXT,
  CHECK ((status = 'ok') = (scope_sha256 IS NOT NULL))
);
CREATE TABLE input_parent (
  version INTEGER NOT NULL REFERENCES version(id),
  position INTEGER NOT NULL,
  parent INTEGER NOT NULL REFERENCES version(id),
  PRIMARY KEY (version, position)
) WITHOUT ROWID;
CREATE TABLE edge (
  id INTEGER PRIMARY KEY,
  parent INTEGER REFERENCES version(id),
  child INTEGER NOT NULL REFERENCES version(id),
  is_ancestor INTEGER CHECK (is_ancestor IN (0, 1)),
  skipped_versions INTEGER NOT NULL,
  implicit_pairs INTEGER NOT NULL,
  implicit_fragments INTEGER NOT NULL,
  UNIQUE (parent, child),
  CHECK ((parent IS NULL) = (is_ancestor IS NULL))
);
CREATE UNIQUE INDEX edge_empty_tree ON edge(child) WHERE parent IS NULL;
CREATE TABLE path (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE
);
CREATE TABLE fragment (
  id INTEGER PRIMARY KEY,
  path INTEGER NOT NULL REFERENCES path(id),
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  UNIQUE (path, start_line, end_line)
);
CREATE TABLE pair (
  id INTEGER PRIMARY KEY,
  fragment_a INTEGER NOT NULL REFERENCES fragment(id),
  fragment_b INTEGER NOT NULL REFERENCES fragment(id),
  UNIQUE (fragment_a, fragment_b),
  CHECK (fragment_a <> fragment_b)
);
CREATE TABLE edge_file_issue (
  edge INTEGER NOT NULL REFERENCES edge(id),
  old_path INTEGER REFERENCES path(id),
  new_path INTEGER REFERENCES path(id),
  issue TEXT NOT NULL CHECK (issue IN ('diff_failed', 'hunk_too_large')),
  hunks INTEGER
);
CREATE TABLE fragment_mapping (
  edge INTEGER NOT NULL REFERENCES edge(id),
  parent_fragment INTEGER NOT NULL REFERENCES fragment(id),
  child_fragment INTEGER REFERENCES fragment(id),
  child_kind TEXT CHECK (child_kind IN ('detected', 'projected')),
  renamed INTEGER NOT NULL CHECK (renamed IN (0, 1)),
  event TEXT NOT NULL CHECK (event IN ('Stable', 'Modified', 'Deleted')),
  similarity_cut INTEGER NOT NULL CHECK (similarity_cut IN (0, 1)),
  file_deleted INTEGER CHECK (file_deleted IN (0, 1)),
  parent_lines INTEGER NOT NULL,
  parent_symbol_lines INTEGER NOT NULL,
  parent_words INTEGER NOT NULL,
  retained_lines INTEGER NOT NULL,
  retained_symbol_lines INTEGER NOT NULL,
  retained_words INTEGER NOT NULL,
  common_words INTEGER NOT NULL,
  modified_lines INTEGER NOT NULL,
  whitespace_lines INTEGER NOT NULL,
  taken_in_lines INTEGER NOT NULL,
  taken_in_modified_lines INTEGER NOT NULL,
  added_lines INTEGER NOT NULL,
  child_words INTEGER,
  text_similarity REAL CHECK (text_similarity BETWEEN 0 AND 1),
  PRIMARY KEY (edge, parent_fragment),
  CHECK ((child_fragment IS NULL) = (event = 'Deleted')),
  CHECK ((child_fragment IS NULL) = (child_kind IS NULL)),
  CHECK (similarity_cut = 0 OR event = 'Deleted'),
  CHECK ((file_deleted IS NULL) = (event <> 'Deleted')),
  CHECK (file_deleted IS NOT 1 OR retained_lines = 0),
  CHECK ((event = 'Deleted' AND similarity_cut = 0) = (retained_lines = 0)),
  CHECK ((child_words IS NULL) = (retained_lines = 0)),
  CHECK ((text_similarity IS NULL) = (child_words IS NULL)),
  CHECK (common_words <= retained_words),
  CHECK (child_words IS NULL OR text_similarity = CASE WHEN parent_words + child_words = 0 THEN 1.0 ELSE 2.0 * common_words / (parent_words + child_words) END),
  CHECK (child_words IS NULL OR (similarity_cut = 1) = (parent_words + child_words > 0 AND 20 * common_words < 3 * (parent_words + child_words)))
) WITHOUT ROWID;
CREATE TABLE fragment_entry (
  edge INTEGER NOT NULL REFERENCES edge(id),
  fragment INTEGER NOT NULL REFERENCES fragment(id),
  added INTEGER NOT NULL CHECK (added IN (0, 1)),
  file_added INTEGER NOT NULL CHECK (file_added IN (0, 1)),
  PRIMARY KEY (edge, fragment),
  CHECK (file_added = 0 OR added = 1)
) WITHOUT ROWID;
CREATE TABLE pair_event (
  id INTEGER PRIMARY KEY,
  edge INTEGER NOT NULL REFERENCES edge(id),
  event TEXT NOT NULL CHECK (event IN ('Appear', 'Persist', 'Merge', 'Disappear')),
  parent_pair INTEGER REFERENCES pair(id),
  child_pair INTEGER REFERENCES pair(id),
  reason TEXT CHECK (reason IN ('fragment_deleted', 'fragments_merged', 'out_of_scope', 'undetected_fragment', 'not_paired')),
  entered_scope INTEGER CHECK (entered_scope IN (0, 1)),
  similarity REAL CHECK (similarity BETWEEN 0 AND 1),
  CHECK ((parent_pair IS NULL) = (event = 'Appear')),
  CHECK ((child_pair IS NULL) = (event = 'Disappear')),
  CHECK ((reason IS NULL) = (event <> 'Disappear')),
  CHECK ((entered_scope IS NULL) = (event <> 'Appear'))
);
CREATE INDEX pair_event_edge ON pair_event(edge);
CREATE UNIQUE INDEX pair_event_parent ON pair_event(edge, parent_pair) WHERE parent_pair IS NOT NULL;
CREATE TABLE root_pair (
  version INTEGER NOT NULL REFERENCES version(id),
  pair INTEGER NOT NULL REFERENCES pair(id),
  similarity REAL CHECK (similarity BETWEEN 0 AND 1),
  PRIMARY KEY (version, pair)
) WITHOUT ROWID;
CREATE TABLE run_resume (
  resumed_at TEXT NOT NULL,
  from_version INTEGER NOT NULL REFERENCES version(id)
);
CREATE TABLE plugin_failure (
  version INTEGER NOT NULL REFERENCES version(id),
  plugin INTEGER NOT NULL REFERENCES plugin(id),
  attempt INTEGER NOT NULL,
  message TEXT NOT NULL,
  PRIMARY KEY (version, plugin, attempt)
) WITHOUT ROWID;
CREATE VIEW fragment_view AS
  SELECT f.id, p.path, f.start_line, f.end_line FROM fragment f JOIN path p ON p.id = f.path;
