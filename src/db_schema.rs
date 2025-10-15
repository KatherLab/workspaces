use std::error::Error;

use rusqlite::Connection;

pub const UPDATE_DB: &[fn(&mut Connection) -> Result<(), Box<dyn Error>>] = &[
    |conn| {
        // Create initial database
        let transaction = conn.transaction()?;
        transaction.pragma_update(None, "journal_mode", "WAL")?;
        transaction.execute(
            "CREATE TABLE workspaces ( \
                filesystem      TEXT     NOT NULL, \
                user            TEXT     NOT NULL, \
                name            TEXT     NOT NULL, \
                expiration_time DATETIME NOT NULL, \
                UNIQUE(filesystem, user, name) \
            )",
            (),
        )?;
        transaction.pragma_update(None, "user_version", 1)?;

        Ok(transaction.commit()?)
    },
    |conn| {
        let transaction = conn.transaction()?;

        // Make id column explicit
        transaction.execute("ALTER TABLE workspaces RENAME TO workspaces_old", ())?;
        transaction.execute(
            "CREATE TABLE workspaces( \
                id              INTEGER  NOT NULL PRIMARY KEY, \
                filesystem      TEXT     NOT NULL, \
                user            TEXT     NOT NULL, \
                name            TEXT     NOT NULL, \
                expiration_time DATETIME NOT NULL, \
                UNIQUE(filesystem, user, name) \
            )",
            (),
        )?;

        transaction.execute(
            "INSERT INTO workspaces(id, filesystem, user, name, expiration_time) \
                SELECT rowid, filesystem, user, name, expiration_time FROM workspaces_old",
            (),
        )?;

        transaction.execute("DROP TABLE workspaces_old", ())?;

        // Table for notifications
        transaction.pragma_update(None, "foreign_keys", 1)?;
        transaction.execute(
            "CREATE TABLE notifications( \
                workspace_id INTEGER  NOT NULL, \
                timestamp    DATETIME NOT NULL, \
                FOREIGN KEY(workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE \
            )",
            (),
        )?;

        transaction.pragma_update(None, "user_version", 2)?;
        Ok(transaction.commit()?)
    },
];

pub const NEWEST_DB_VERSION: usize = UPDATE_DB.len();
