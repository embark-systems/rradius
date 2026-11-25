-- Plans, users and BNG styles. An applied file must not be edited; to change the structure, add a file with the next number.

-- Each plan: the download (toward the user) and upload (from the user toward the network) rates, in kbit/s;
-- profile is the name of the same-named profile on the BNG, used by the styles that accept only a name.
CREATE TABLE plans (
  plan TEXT PRIMARY KEY,
  down_kbps INTEGER NOT NULL CHECK (down_kbps > 0),
  up_kbps INTEGER NOT NULL CHECK (up_kbps > 0),
  profile TEXT NOT NULL
);

CREATE TABLE subscribers (
  username TEXT PRIMARY KEY,
  password TEXT,                          -- Format: see docs/usage.md section 5
  plan TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);

-- Which kind of attribute each BNG (RADIUS client) uses. The client column is the name of [[radius.clients]] in the configuration file.
-- style: profile, wispr, mikrotik, alc_sla, erx (see rate.lua). A client not listed uses profile.
CREATE TABLE nas_styles (
  client TEXT PRIMARY KEY,
  style TEXT NOT NULL
);
