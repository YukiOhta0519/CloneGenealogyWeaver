CREATE TABLE genealogy_run (
  genealogy_run_id TEXT PRIMARY KEY,
  events_run_id TEXT NOT NULL,
  events_path TEXT NOT NULL,
  schema_version INTEGER NOT NULL,
  tool_version TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE TABLE version_time (
  version INTEGER PRIMARY KEY,
  effective_at TEXT NOT NULL
);
CREATE TABLE genealogy (
  id INTEGER PRIMARY KEY,
  root_count INTEGER NOT NULL CHECK (root_count >= 1),
  birth_version INTEGER,
  birth_kind TEXT CHECK (birth_kind IN ('appear', 'left_censored')),
  censored INTEGER NOT NULL CHECK (censored IN (0, 1)),
  effective_days REAL CHECK (effective_days >= 0),
  dag_transitions INTEGER CHECK (dag_transitions >= 0),
  dag_transitions_with_skipped INTEGER,
  CHECK ((birth_version IS NULL) = (root_count > 1)),
  CHECK ((birth_kind IS NULL) = (root_count > 1)),
  CHECK ((effective_days IS NULL) = (birth_kind IS NOT 'appear')),
  CHECK ((dag_transitions IS NULL) = (effective_days IS NULL)),
  CHECK ((dag_transitions_with_skipped IS NULL) = (effective_days IS NULL)),
  CHECK (dag_transitions_with_skipped >= dag_transitions)
);
CREATE TABLE genealogy_root (
  genealogy INTEGER NOT NULL REFERENCES genealogy(id),
  version INTEGER NOT NULL,
  pair INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('appear', 'left_censored')),
  PRIMARY KEY (version, pair)
) WITHOUT ROWID;
CREATE INDEX genealogy_root_genealogy ON genealogy_root(genealogy);
CREATE TABLE genealogy_event (
  pair_event INTEGER PRIMARY KEY,
  genealogy INTEGER NOT NULL REFERENCES genealogy(id),
  role TEXT CHECK (role IN ('birth', 'end'))
);
CREATE INDEX genealogy_event_genealogy ON genealogy_event(genealogy, role);
CREATE TABLE genealogy_censored_tip (
  genealogy INTEGER NOT NULL REFERENCES genealogy(id),
  version INTEGER NOT NULL,
  pair INTEGER NOT NULL,
  PRIMARY KEY (version, pair)
) WITHOUT ROWID;
CREATE INDEX genealogy_censored_tip_genealogy ON genealogy_censored_tip(genealogy);
