CREATE TABLE claim_pair_event (
  pair_event INTEGER PRIMARY KEY REFERENCES pair_event(id),
  label TEXT NOT NULL CHECK (label IN ('intra', 'inter')),
  changed INTEGER CHECK (changed IN (0, 1))
);
CREATE TABLE claim_root_pair (
  version INTEGER NOT NULL,
  pair INTEGER NOT NULL,
  label TEXT NOT NULL CHECK (label IN ('intra', 'inter')),
  PRIMARY KEY (version, pair),
  FOREIGN KEY (version, pair) REFERENCES root_pair(version, pair)
) WITHOUT ROWID;
CREATE TABLE claim_context_issue (
  version INTEGER NOT NULL REFERENCES version(id),
  context TEXT NOT NULL,
  PRIMARY KEY (version, context)
) WITHOUT ROWID;
