.POSIX:

BIN = target/release/workspaces

$(BIN): src/main.rs src/cli.rs src/config.rs src/zfs.rs
	cargo build --release

install: $(BIN)
	# install binary
	install -D -m 4755 $(BIN) /usr/local/bin/workspaces
	test -e /usr/bin/workspaces || ln -s /usr/local/bin/workspaces /usr/bin/workspaces
	# copy config
	mkdir -p /etc/workspaces
	cp workspaces.toml /etc/workspaces/workspaces.example.toml
	test -e /etc/workspaces/workspaces.toml || install -m 0600 workspaces.toml /etc/workspaces/
	# make database dir
	mkdir -p /usr/local/lib/workspaces
	# install systemd service / timer
	cp maintain-workspaces.service /etc/systemd/system/
	cp maintain-workspaces.timer /etc/systemd/system/
