use rusqlite::Connection;

//TODO make result
pub const UPDATE_DB: &[fn(&mut Connection)] = &[
    |conn| {
        // Creates initial database
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        let transaction = conn.transaction().unwrap();
        transaction
            .execute(
                "CREATE TABLE workspaces (
                    filesystem      TEXT     NOT NULL,
                    user            TEXT     NOT NULL,
                    name            TEXT     NOT NULL,
                    expiration_time DATETIME NOT NULL,
                    UNIQUE(filesystem, user, name)
                )",
                (),
            )
            .unwrap();
        transaction.pragma_update(None, "user_version", 1).unwrap();
        transaction.commit().unwrap();
    },
    |conn| {
        let transaction = conn.transaction().unwrap();
        // Make id column explicit
        transaction
            .execute("ALTER TABLE workspaces RENAME TO workspaces_old", ())
            .unwrap();
        transaction
            .execute(
                "CREATE TABLE workspaces(
                    id              INTEGER  NOT NULL PRIMARY KEY,
                    filesystem      TEXT     NOT NULL,
                    user            TEXT     NOT NULL,
                    name            TEXT     NOT NULL,
                    expiration_time DATETIME NOT NULL,
                    UNIQUE(filesystem, user, name)
                )",
                (),
            )
            .unwrap();

        transaction
            .execute(
                "INSERT INTO workspaces(id, filesystem, user, name, expiration_time)
                    SELECT rowid, filesystem, user, name, expiration_time FROM workspaces_old",
                (),
            )
            .unwrap();

        transaction
            .execute("DROP TABLE workspaces_old", ())
            .unwrap();

        // Table for notifications
        transaction.pragma_update(None, "foreign_keys", 1).unwrap();
        transaction
            .execute(
                "CREATE TABLE notifications(
                    workspace_id INTEGER  NOT NULL,
                    timestamp    DATETIME NOT NULL,
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
                )",
                (),
            )
            .unwrap();

        transaction.pragma_update(None, "user_version", 2).unwrap();
        transaction.commit().unwrap();
    },
];
pub const NEWEST_DB_VERSION: usize = UPDATE_DB.len();
