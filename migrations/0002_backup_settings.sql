-- When scheduled backups run and how many are kept.
--
-- `settings::all` lists only the rows present in this table, so without one
-- the section never reaches the Settings page and nobody can turn the schedule
-- off. The value is left empty: every field carries its own serde default, and
-- writing them here too would be a second copy to keep in step.
INSERT INTO core_setting (key, name, value) VALUES ('backup_settings', 'Backups', '{}');
