-- The subscribers of aka.toml. The rows go in 2_subscribers.sql, made by the commands in
-- "效能量測" in docs/usage.md, since their Ki and OPc are sealed under aka.key.
CREATE TABLE aka_subscribers (imsi TEXT PRIMARY KEY, ki TEXT NOT NULL, opc TEXT NOT NULL, sqn INTEGER NOT NULL);
