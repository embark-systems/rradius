-- Known devices. A MAC address is always 12 lowercase hexadecimal characters, however the switch writes it.
CREATE TABLE mac_devices (
  mac TEXT PRIMARY KEY,
  vlan TEXT NOT NULL,
  note TEXT                       -- note for people, the script does not read it
);
