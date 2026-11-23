# Changelog

All notable changes to Workspaces, newest first.

## 0.8.0

### Added

* Notification recipients can be taken from the directory (FreeIPA, LDAP, AD)
  instead of each user's `~/.config/workspaces.toml`, via SSSD's InfoPipe:
  `[smtp] recipient_source = "file" | "sssd" | "sssd-then-file"`. The default
  stays `"file"`, so existing setups are unaffected. Needs
  `[ifp] user_attributes = +mail, +nsAccountLock` in `sssd.conf`.
* `[smtp] skip_inactive_users` — don't notify users whose directory account is
  disabled (`nsAccountLock = TRUE`).
* `[smtp] placeholder_mail_domains` — addresses in these domains (e.g.
  `alice@katherlab.internal`) count as no address at all.
* `[smtp] admin_report_to` — after each `workspaces maintain` run, one mail
  summarising every notification that could not be delivered, grouped by user.
  Nothing is sent when there is nothing to report.
* Machine-readable output: `workspaces --format csv|json list` and
  `workspaces --format csv|json filesystems`.
* Settings the configuration file holds but Workspaces doesn't know are now
  listed on startup, with a hint where a known name belongs, instead of being
  ignored in silence.

### Changed

* Failures are explained in plain words instead of a Rust panic or a debug-
  printed error, with exit codes to match: 7 for a full disk or exhausted
  quota, 8 for a database that cannot be read or written, 9 for a bug.
* A notification that could not be delivered is no longer recorded as sent, so
  a user whose address starts working again still gets their expiry reminder.
* A recipient's address is only looked up once a reminder is actually due.
* `workspaces create` creates the dataset before committing to the database,
  so a failure no longer leaves behind a workspace that exists in the database
  and nowhere else — which then blocked creating it again.
* `workspaces maintain` keeps going when a single workspace cannot be
  maintained, and reports how many steps failed at the end.
* `workspaces share`: read-write access is `-w`/`--read-write`, freeing `-r`
  for `--revoke`.

### Fixed

* Creating a fresh database failed outright (`cannot change into wal mode from
  within a transaction`), as the first migration set the journal mode inside
  its own transaction.
* `workspaces maintain` no longer dies on a user who has been removed from the
  directory, on a workspace whose filesystem is no longer configured, or on a
  database write that fails.
* `expiry_notifications`, the name the example configuration suggested, was
  silently ignored; it is now accepted as an alias for
  `expiry_notifications_on_days`.
* `workspaces rename` of a workspace that doesn't exist says so instead of
  failing in `zfs`.
* A missing, unreadable or malformed configuration file, and a database that
  cannot be opened or migrated, are reported as such.

## 0.7.0

* `workspaces share` grants another user access to a workspace through POSIX
  ACLs, and `workspaces list` shows who a workspace is shared with. Requires
  `acltype=posixacl` on the dataset and the `acl` package.
* Only one missing-email warning per run, and clearer `setfacl` errors.

## 0.6.0

* Email notifications when a workspace is created, extended, expired or
  deleted, on top of the periodic expiry reminders.
* `workspaces notify-test` to check the SMTP setup.
* SMTP relay port, auth mechanism, TLS mode and a separate sender address are
  configurable; `workspaces expire` silences a workspace's notifications.
* `workspaces maintain` is restricted to root.

## 0.3.0 and earlier

See the git history.
