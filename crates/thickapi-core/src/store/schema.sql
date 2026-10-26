CREATE TABLE IF NOT EXISTS session_bindings (
  session_id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL,
  backend TEXT NOT NULL,
  cli_session TEXT,
  cwd TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS processes (
  id INTEGER PRIMARY KEY,
  turn_id TEXT,
  pid INTEGER,
  backend TEXT,
  started_at TEXT,
  ended_at TEXT,
  owner_pid INTEGER,
  owner_started INTEGER,
  pid_started INTEGER
);

CREATE TABLE IF NOT EXISTS account_health (
  account_id TEXT PRIMARY KEY,
  status TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS paid_daily (
  day TEXT PRIMARY KEY,
  usd REAL NOT NULL
);
