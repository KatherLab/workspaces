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
* Periodic **expiry reminders** (based on your configured `expiry_notifications` schedule)

To enable this, configure the `[smtp]` section in `/etc/workspaces/workspaces.toml`
and make sure each user sets up their personal email address once:

```bash
mkdir -p ~/.config
echo 'email = "user@example.org"' > ~/.config/workspaces.toml
```

If a user has not configured their email, the CLI will print a clear reminder
with the exact command to fix it.

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
NAME    USER        FS    EXPIRY          SIZE  MOUNTPOINT
testws  mvantreeck  bulk  expires in  9d    0G  /mnt/bulk/mvantreeck/testws
```

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

### Manually Running the Garbage Collector

Usually, your administrator will have configured automatic cleanup through the
systemd timer. However, an admin can also run it manually:

```console
# must be run as root
$ sudo workspaces maintain
```

This will delete expired workspaces beyond their retention date and send
final deletion notifications.


