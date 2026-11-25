-- Demo: accounts imported from an old system; every password is <username>-password. In production delete this file
-- and replace it with your own exported data. Each password value is the original hash from the old system with {format} in front.
INSERT INTO users (username, password, origin) VALUES
  -- userPassword from the old LDAP directory, copied as it is
  ('ldap1', '{SSHA}ZvqZAfdyjd1PQiq8rurmdodN8ihkZW1vc2FsdA==', 'ldap'),
  ('ldap2', '{SSHA512}dgYg0PfH9neglxpRGmR3p4Crhif+xIPQ/AurUaFcjdsf8dogk1i2s9DiU+DeaMqnHZuZBZSExkKQ0pOuZpT9EmRlbW9zYWx0', 'ldap'),
  ('ldap3', '{PBKDF2-SHA256}10000$MDEyMzQ1Njc4OWFiY2RlZg$fesJONwdF0Xh3lhrasrq8MKhcVo5p23hOe4RkdXVGOI', 'ldap'),
  ('ldap4', '{X-PBKDF2}HMACSHA2+256:AAAnEA==:ZGVtb3NhbHQ=:9qxNguGzOOSXPzISiwG6JVifVK1DUTy95ow8P+VBTQI=', 'ldap'),
  -- the second field of /etc/shadow, with {crypt} in front
  ('shell1', '{crypt}$1$demosalt$EhA8qeu4qSun8gh9DajmS1', 'shadow'),
  -- the old mail server's directory
  ('mail1', '{NS-MTA-MD5}0be637c7ffbaf0908fd571e4539b982dabcdefghijklmnopqrstuvwxyz012345', 'mail'),
  -- the old database stores hexadecimal MD5: convert it to Base64 first, then add {MD5} (see the manual)
  ('old1', '{MD5}kfiBqWi4k+QJLsoSpSKT7w==', 'legacy-db'),
  -- the old system stores the NT hash (for MS-CHAPv2 and PEAP only this and plaintext work)
  ('win1', '{nthash}9d232eea31049319fc8ab84300855a34', 'nt');

INSERT INTO user_reply (username, attribute, value) VALUES ('ldap1', 'Filter-Id', 'staff');
