CREATE TABLE test_path_pair_event (
  pair_event INTEGER PRIMARY KEY REFERENCES pair_event(id),
  label TEXT NOT NULL CHECK (label IN ('product', 'test', 'mixed')),
  changed INTEGER CHECK (changed IN (0, 1))
);
CREATE TABLE test_path_root_pair (
  version INTEGER NOT NULL,
  pair INTEGER NOT NULL,
  label TEXT NOT NULL CHECK (label IN ('product', 'test', 'mixed')),
  PRIMARY KEY (version, pair),
  FOREIGN KEY (version, pair) REFERENCES root_pair(version, pair)
) WITHOUT ROWID;
