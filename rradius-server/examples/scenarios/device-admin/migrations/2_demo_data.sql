-- Demo data. In production, delete this file. The passwords are bcrypt (htpasswd -nbBC 10 x <password>).
INSERT INTO admins (username, password, role, enabled) VALUES
  ('netadmin1', '{crypt}$2y$10$MZSNw6hZJBIkr494NBkaA./T3h3MOVHt8IVMmsiGq5i4ZvN8qMGEO', 'netadmin', 1),
  ('noc1', '{crypt}$2y$10$QKOTVOrb7N0yowDTgTj4yun7d7OE3yK/UvWc0sLHb0kc2IAXtAeua', 'noc', 1);
