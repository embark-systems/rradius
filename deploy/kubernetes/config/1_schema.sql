-- Add users in a new numbered file, such as 2_users.sql: an applied file must not change.
--   INSERT INTO users VALUES ('alice', '{argon2}$argon2id$...');
CREATE TABLE users (username TEXT PRIMARY KEY, password TEXT);
CREATE TABLE accounting (username TEXT, session_id TEXT, status TEXT, received INTEGER);
