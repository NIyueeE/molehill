## Systemd Unit Examples

The directory lists some systemd unit files for example, which can be used to run `molehill` as a service on Linux.

[The `@` symbol in the name of unit files](https://superuser.com/questions/393423/the-symbol-and-systemctl-and-vsftpd) such as
`molehill@.service` facilitates the management of multiple instances of `molehill`.

For the naming of the example, `molehills` stands for `molehill --server`, and `molehillc` stands for `molehill --client`, `molehill` is just `molehill`.

For security, it is suggested to store configuration files with permission `600`, that is, only the owner can read the file, preventing arbitrary users on the system from accessing the secret tokens.

### With root privilege

Assuming that `molehill` is installed in `/usr/bin/molehill`, and the configuration file is in `/etc/molehill/app1.toml`, the following steps show how to run an instance of `molehill --server` with root.

1. Create a service file.

```bash
sudo cp molehills@.service /etc/systemd/system/
```

2. Create the configuration file `app1.toml`.

```bash
sudo mkdir -p /etc/molehill
# And create the configuration file named `app1.toml` inside /etc/molehill
```

3. Enable and start the service.

```bash
sudo systemctl daemon-reload # Make sure systemd find the new unit
sudo systemctl enable molehills@app1 --now
```

### Without root privilege

Assuming that `molehill` is installed in `~/.local/bin/molehill`, and the configuration file is in `~/.local/etc/molehill/app1.toml`, the following steps show how to run an instance of `molehill --server` without root.

1. Edit the example service file as...

```txt
# with root
# ExecStart=/usr/bin/molehill -s /etc/molehill/%i.toml
# without root
ExecStart=%h/.local/bin/molehill -s %h/.local/etc/molehill/%i.toml
```

2. Create a service file.

```bash
mkdir -p ~/.config/systemd/user
cp molehills@.service ~/.config/systemd/user/
```

3. Create the configuration file `app1.toml`.

```bash
mkdir -p ~/.local/etc/molehill
# And create the configuration file named `app1.toml` inside ~/.local/etc/molehill
```

4. Enable and start the service.

```bash
systemctl --user daemon-reload # Make sure systemd find the new unit
systemctl --user enable molehills@app1 --now
```

### Run multiple services

To run multiple services at once, simply add another configuration, say `app2.toml` under `/etc/molehill` (`~/.local/etc/molehill` for non-root), then run `sudo systemctl enable molehills@app2 --now` (`systemctl --user enable molehills@app2 --now` for non-root) to start an instance for that configuration.

The same applies to `molehillc@.service` for `molehill --client` and `molehill@.service` for `molehill`.
