# Workspaces

Workspaces are a way to manage ephemeral data. They allow users to create
folders that are automatically deleted if they haven't been used for a certain
period of time. This is a convenient method for handling temporary files and
preventing clutter on file systems.

Workspaces uses ZFS to efficiently manage the creation, extension, and deletion
of workspaces and supports optional **email notifications** for important events.

## Installation

Before installing Workspaces, you must have Rust installed.  
You also need to install the following system packages:

```console
$ sudo apt install sqlite3 libsqlite3-dev libssl-dev pkg-config
````

Then install and build Workspaces:

```console
$ make && sudo make install
```

You must manually modify the `/etc/workspaces/workspaces.toml` file, and you
must have already set up a ZFS zpool.

To activate automatic deletion of old workspaces, enable the corresponding
systemd timer:

```console
$ sudo systemctl enable --now maintain-workspaces.timer
```

> **Note:**
> The `workspaces maintain` command (triggered by the timer) requires **admin (root)** privileges.

## Email Notifications

Workspaces can optionally send notification emails for the following events:

* A workspace is **created**, **extended**, or **manually expired**
* A workspace is **deleted** after its retention period
* Periodic **expiry reminders** (based on each filesystem's `expiry_notifications_on_days` schedule)

To enable this, configure the `[smtp]` section in `/etc/workspaces/workspaces.toml`
and make sure each user sets up their personal email address once:

```bash
mkdir -p ~/.config
echo 'email = "user@example.org"' > ~/.config/workspaces.toml
```

If a user has not configured their email, the CLI will print a clear reminder
with the exact command to fix it.

### Taking addresses from the directory (FreeIPA / LDAP / AD)

Instead of each user's own file, Workspaces can use the address the directory
has for them, which it asks SSSD for:

```toml
[smtp]
# ... relay, username, password ...
recipient_source = "sssd-then-file"
skip_inactive_users = true
placeholder_mail_domains = ["katherlab.internal"]
admin_report_to = "it@example.org"
```

* **`recipient_source`** — where to look up the recipient's address:
  * `"file"` — `~USER/.config/workspaces.toml` only. This is the default, so
    existing setups keep working unchanged.
  * `"sssd"` — the `mail` attribute the directory has for the user.
  * `"sssd-then-file"` — the directory, falling back to the user's own file.
* **`skip_inactive_users`** — don't notify users whose directory account is
  disabled (`nsAccountLock = TRUE`). Needs one of the SSSD sources.
* **`placeholder_mail_domains`** — addresses in these domains (e.g.
  `alice@katherlab.internal`) count as no address at all. Case is irrelevant.
* **`admin_report_to`** — where to send a summary of the notifications a
  `workspaces maintain` run could not deliver. One mail per run, grouped by
  user; nothing is sent if there is nothing to report.

The lookup goes through SSSD's InfoPipe, which only hands out attributes that
are allow-listed in `/etc/sssd/sssd.conf`:

```ini
[ifp]
user_attributes = +mail, +nsAccountLock
```

```console
$ sudo systemctl restart sssd
```

You can check what Workspaces will see for a user with:

```console
$ busctl call --system org.freedesktop.sssd.infopipe \
    /org/freedesktop/sssd/infopipe org.freedesktop.sssd.infopipe \
    GetUserAttr sas alice 2 mail nsAccountLock
a{sv} 2 "mail" as 1 "alice@tu-dresden.de" "nsAccountLock" as 1 "FALSE"
```

A user who cannot be reached — no address, only a placeholder address, a
disabled account, no such user any more, or a directory that cannot be asked —
is skipped, and the skipped expiry reminder is **not** recorded as sent: once
they have a working address again, they still get their reminder. With
`recipient_source` other than `"file"`, the CLI's reminder about a missing
address says what the directory is missing instead of naming a file the user
cannot usefully edit.

## User Tutorial

This tutorial will walk you through the process of using Workspaces, including
creating a workspace, extending its expiry date, and manually expiring it.

### Creating a Workspace

Use the `workspaces filesystems` command to display the available filesystems:

```console
$ workspaces filesystems
NAME  USED   FREE    TOTAL   DURATION  RETENTION
bulk  4805G  17391G  22196G       90d        30d
ssd      0G   5999G   5999G       30d         7d
```

To create a workspace named `testws` on the `bulk` filesystem with a ten-day
expiry date:

```console
$ workspaces create -f bulk -d 10 testws
Created workspace at /mnt/bulk/mvantreeck/testws
```

If SMTP is configured, you’ll also receive a short email confirmation.

Use `workspaces list` to view all available workspaces:

```console
$ workspaces list
NAME    USER        FS    EXPIRY          SIZE  MOUNTPOINT                       SHARES
testws  mvantreeck  bulk  expires in  9d    0G  /mnt/bulk/mvantreeck/testws
```

### Machine-Readable Output

The `list` and `filesystems` commands support a global `--format` flag for
scripting:

```console
$ workspaces --format csv list
$ workspaces --format json filesystems
```

`csv` emits an RFC 4180 header row followed by one record per line. `json`
emits an array of objects. In both, values are raw rather than the
human-friendly strings shown in the table — for example, sizes are given in
**bytes** (`size_bytes`, `used_bytes`, ...) and the expiry is an
[RFC 3339](https://datatracker.ietf.org/doc/html/rfc3339) timestamp (`expiry`),
so a consumer can compute days-remaining or deletion status itself. A
disabled filesystem has an empty `duration_days`. The `shares` field is an
empty string / `null` when the workspace is private, otherwise a
comma-separated list like `alice:ro, bob:rw`.

Columns can be restricted with `-o` (repeatable); the selected columns
determine the keys/fields in the machine-readable output too:

```console
$ workspaces --format json list -o name -o expiry
[{"name":"testws","expiry":"2026-07-18T10:00:00+00:00"}]
```

The `SHARES` column shows which other users a workspace has been shared with
(for example `alice:ro, bob:rw`), or is blank if the workspace is private.

### Extending a Workspace

To extend your workspace before it expires:

```console
$ workspaces extend -f bulk -d 7 testws
```

You’ll receive an email confirming the new expiry date.

### Manually Expiring a Workspace

To manually expire a workspace that is no longer needed:

```console
$ workspaces expire -f bulk testws
```

The workspace becomes read-only and will be deleted automatically later.
An email notification is sent when it’s marked expired or scheduled for deletion.

### Sharing a Workspace

You can grant another user access to one of your workspaces. By default they
get **read-only** access; use `--read-write` to let them modify files too.

```console
$ workspaces share -f bulk testws alice          # read-only
$ workspaces share -f bulk testws alice --read-write
```

To remove a user's access again:

```console
$ workspaces share -f bulk testws alice --revoke
```

Only the workspace's owner (or an admin) may share it.

Access is granted via POSIX ACLs, so the underlying filesystem must have
ACLs enabled. On ZFS this means each pool root must be set up once by an
administrator:

```console
# must be run as root, once per pool
$ sudo zfs set acltype=posixacl zpool/bulk
```

New files created in a shared workspace inherit the access automatically,
so you don't need to re-share after adding data.

### Manually Running the Garbage Collector

Usually, your administrator will have configured automatic cleanup through the
systemd timer. However, an admin can also run it manually:

```console
# must be run as root
$ sudo workspaces maintain
```

This will delete expired workspaces beyond their retention date and send
final deletion notifications.


