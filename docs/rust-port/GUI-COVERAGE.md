# GUI coverage: every CLI command in the desktop app

Every command, flag and argument of `mokuro-bunko` has a row here. The row says where
the same thing is done in the desktop app or in the library's admin panel (`/_admin`),
or why it stays a command line feature ("CLI-only because …"). The desktop app is the
set of `/app/...` pages. `mokuro-bunko gui`, `serve` and `processor serve` serve them
on their loopback control port.

The table is checked by a test, `gui::coverage` in `crates/mokuro-bunko`. It walks the
clap tree and fails when:

- a command, flag or argument has no row;
- a row names neither a page nor a "CLI-only because" reason;
- a row links an `/app/` page or a settings section that does not exist;
- a row names something the CLI no longer has (checked in the full build).

Hidden flags and subcommands (internal ones such as `update prefetch` and
`install-ocr --probe`, and the 0.5 spellings) are walked like any other, so each has a
row, saying "CLI-only because it is internal (hidden)".

When you add a flag, add its row. Rows use the CLI's spelling, such as
`install-ocr --from` or `admin add-user <USERNAME>`. Global flags have no command in
front (`--config`).

The admin panel lives on the library server, so the server must be running to use it.
The app's [Users & library](/app/settings/library) section links each of its tabs.

## Global flags

| Command | Where |
|---|---|
| `--config` | The app works on the config file it was started with (`mokuro-bunko -c PATH gui`), and every job it runs gets the same `-c`. [Advanced](/app/settings/advanced) → "Where the files are" shows the path |
| `--verbose` | CLI-only because it only adds detail to terminal output. The app's jobs show their full output, and [Logs](/app/settings/logs) shows the log files |
| `--version` | [Updates](/app/settings/update) shows this version and the latest one |

## Library server

| Command | Where |
|---|---|
| `serve` | [Library server setup](/app/setup/server) starts it at the end; [Server](/app/settings/server) → "Start the server". The tray starts and stops it |
| `serve --host` | [Server](/app/settings/server) → Listen on (`server.host`) |
| `serve --port` | [Server](/app/settings/server) → Port (`server.port`); first set in [setup](/app/setup/server) |
| `serve --ocr` | [Server](/app/settings/server) → Devices (`ocr.backend`) |
| `serve --generations` | /_admin → Settings → OCR (the engine generations and their order) |
| `setup` | [Library server setup](/app/setup/server): folder, admin account, registration, remote access, HTTPS, then two toggles: OCR on this machine (its install runs in the flow) and start with the machine |
| `setup --skip-if-exists` | CLI-only because it exists for container entrypoints and scripts. [Setup](/app/setup/server) says when a configuration already exists, and replaces it only when that box is ticked |
| `doctor` | [Diagnostics](/app/settings/doctor) runs the same checks with live output; the OCR install runs it at the end (in the [server](/app/setup/server) and [processor](/app/setup/processor) setups, and on the [OCR install](/app/setup/ocr) page) |
| `doctor --processor` | [Diagnostics](/app/settings/doctor) → Check: the processor |
| `healthcheck` | [Diagnostics](/app/settings/doctor) → "Is the server answering?" |
| `healthcheck --url` | CLI-only because it probes an arbitrary address for container health checks. The app checks this machine's own server |

## Users and invites

| Command | Where |
|---|---|
| `admin` | /_admin → Users and Invites (linked from [Users & library](/app/settings/library)) |
| `admin add-user` | /_admin → Users → Add user; the first admin is made by [setup](/app/setup/server) |
| `admin add-user <USERNAME>` | /_admin → Users → Add user → Username |
| `admin add-user --role` | /_admin → Users → Add user → Role |
| `admin add-user --password` | /_admin → Users → Add user → Password |
| `admin delete-user` | /_admin → Users → Delete |
| `admin delete-user <USERNAME>` | /_admin → Users → the user's row |
| `admin delete-user --yes` | /_admin → Users → Delete asks in a dialog instead |
| `admin list-users` | /_admin → Users |
| `admin list-users --status` | /_admin → Users shows each account's status (active, pending, disabled, deleted) |
| `admin change-role` | /_admin → Users → Change role |
| `admin change-role <USERNAME>` | /_admin → Users → the user's row |
| `admin change-role <ROLE>` | /_admin → Users → Change role → Role |
| `admin generate-invite` | /_admin → Invites → Generate |
| `admin generate-invite --role` | /_admin → Invites → Generate → Role |
| `admin generate-invite --expires` | /_admin → Invites → Generate → Expires in |
| `admin list-invites` | /_admin → Invites |
| `admin list-invites --all` | /_admin → Invites lists used and expired codes with their status |
| `admin delete-invite` | /_admin → Invites → Delete |
| `admin delete-invite <CODE>` | /_admin → Invites → the code's row |
| `admin approve-user` | /_admin → Users → Approve (pending accounts) |
| `admin approve-user <USERNAME>` | /_admin → Users → the user's row |
| `admin disable-user` | /_admin → Users → Disable |
| `admin disable-user <USERNAME>` | /_admin → Users → the user's row |
| `admin restore-user` | CLI-only because the admin panel cannot bring deleted accounts back yet. This command is the recovery path for an account deleted by mistake |
| `admin restore-user <USERNAME>` | CLI-only because `admin restore-user` is |
| `admin restore-user --role` | CLI-only because `admin restore-user` is |
| `admin restore-user --password` | CLI-only because `admin restore-user` is |
| `admin set-password` | CLI-only because it is the recovery path when the only admin is locked out. Users change their own password on the account page |
| `admin set-password <USERNAME>` | CLI-only because `admin set-password` is |
| `admin set-password --password` | CLI-only because `admin set-password` is |

## Configuration

| Command | Where |
|---|---|
| `config` | [Settings](/app/settings/server); every key is in [Advanced](/app/settings/advanced) |
| `config show` | [Advanced](/app/settings/advanced) → config.yaml (the file with `MOKURO_*` applied) |
| `config set` | [Advanced](/app/settings/advanced) → Any setting; the common keys have their own fields in [Server](/app/settings/server) |
| `config set <KEY>` | [Advanced](/app/settings/advanced) → Key (the same list `config set` accepts) |
| `config set <VALUE>` | [Advanced](/app/settings/advanced) → Value |
| `config path` | [Advanced](/app/settings/advanced) → "Where the files are" |
| `config init` | [Library server setup](/app/setup/server) writes a new file; [Advanced](/app/settings/advanced) → "Reset to defaults" |
| `config init --force` | [Advanced](/app/settings/advanced) → "Reset to defaults" (asks first) |
| `config cors-add` | [Remote access](/app/settings/remote) → Other web apps → Add; also /_admin → Settings |
| `config cors-add <ORIGIN>` | [Remote access](/app/settings/remote) → Other web apps → the origin |
| `config cors-remove` | [Remote access](/app/settings/remote) → Other web apps → Remove |
| `config cors-remove <ORIGIN>` | [Remote access](/app/settings/remote) → Other web apps → that origin's Remove |

## HTTPS, tunnel and Dynamic DNS

| Command | Where |
|---|---|
| `ssl` | [HTTPS](/app/settings/https) |
| `ssl enable` | [HTTPS](/app/settings/https) → Turn on; also a choice in [setup](/app/setup/server) |
| `ssl enable --auto-cert` | [HTTPS](/app/settings/https) → "Use a self-signed certificate" |
| `ssl enable --cert` | [HTTPS](/app/settings/https) → Certificate file |
| `ssl enable --key` | [HTTPS](/app/settings/https) → Private key file |
| `ssl disable` | [HTTPS](/app/settings/https) → "Turn HTTPS off" |
| `ssl status` | [HTTPS](/app/settings/https) shows it; "Show details" runs the command |
| `ssl generate` | [HTTPS](/app/settings/https) → Make a self-signed certificate |
| `ssl generate --hostname` | [HTTPS](/app/settings/https) → Host name |
| `ssl generate --days` | [HTTPS](/app/settings/https) → Valid for (days) |
| `tunnel` | /_admin → Connectivity (linked from [Remote access](/app/settings/remote)) |
| `tunnel status` | [Remote access](/app/settings/remote) → "Is cloudflared installed?"; /_admin → Connectivity |
| `tunnel cloudflare` | /_admin → Connectivity → Cloudflare tunnel (the running server owns the tunnel) |
| `tunnel cloudflare --port` | /_admin → Connectivity: the server tunnels its own port |
| `dyndns` | [Remote access](/app/settings/remote); /_admin → Connectivity → Dynamic DNS |
| `dyndns setup` | [Setup](/app/setup/server) → Remote access → Dynamic DNS (provider, domain, token); later in /_admin → Connectivity |
| `dyndns status` | [Remote access](/app/settings/remote) → "Dynamic DNS status" |
| `dyndns update` | [Remote access](/app/settings/remote) → "Update the DNS record now" |
| `dyndns enable` | [Remote access](/app/settings/remote) → "Turn DynDNS on" |
| `dyndns disable` | [Remote access](/app/settings/remote) → "Turn DynDNS off" |

## Updates

| Command | Where |
|---|---|
| `update` | [Updates](/app/settings/update); the tray's "Check for updates" opens it |
| `update check` | [Updates](/app/settings/update) checks when it opens; "Check again" |
| `update apply` | [Updates](/app/settings/update) → "Install the update" (live output) |
| `update apply --yes` | [Updates](/app/settings/update): the button is the confirmation |
| `update prefetch` | CLI-only because it is internal (hidden): an automatic update runs it from the downloaded release before installing it. The app shows the result as the [dashboard](/app/dashboard)'s update line, and turns automatic updates on in [Updates](/app/settings/update) → "Install updates automatically" |
| `update prefetch --processor-config` | CLI-only because `update prefetch` is |
| `update prefetch --manifest-url` | CLI-only because `update prefetch` is |
| `update apply --restart` | CLI-only because it replaces the calling process with the new `serve`. From the app, restart the server from the tray after updating |

## OCR backend and models

| Command | Where |
|---|---|
| `install-ocr` | Runs inside the [processor setup](/app/setup/processor) (always) and the [server setup](/app/setup/server) (when "OCR on this machine" is on), with progress; later from Settings → [OCR & models](/app/settings/ocr) → [OCR install](/app/setup/ocr) |
| `install-ocr --variant` | The setups' OCR step and [OCR install](/app/setup/ocr) → Pack (automatic, cpu, cu130, rocm7.1) |
| `install-ocr --from` | The setups' OCR step and [OCR install](/app/setup/ocr) → More options → Install from a folder |
| `install-ocr --dir` | [OCR install](/app/setup/ocr) → More options → Install into |
| `install-ocr --no-models` | The setups' OCR step and [OCR install](/app/setup/ocr) → More options → Only the pack |
| `install-ocr --force` | The setups' OCR step and [OCR install](/app/setup/ocr) → More options → Reinstall |
| `install-ocr --if-needed` | CLI-only because `serve` and `processor serve` run it themselves, in the background, when local OCR needs a backend that is missing (its progress and a Retry are on the dashboard); in the desktop app the [OCR install](/app/setup/ocr) page is the deliberate way |
| `install-ocr --list` | [OCR & models](/app/settings/ocr) shows the hardware and the installed packs; "Packs and hardware" runs it |
| `install-ocr --processor` | The [processor setup](/app/setup/processor) installs for the processor; [OCR install](/app/setup/ocr) → For: the processor |
| `install-ocr --probe` | CLI-only because it is internal (hidden): an automatic update loads a new backend pack in a child process to check it works before switching to it |
| `install-ocr --backend` | CLI-only because it is the hidden 0.5 spelling of `--variant`, kept for old scripts |
| `install-ocr --engines` | CLI-only because it is a hidden 0.5 flag that 0.7 accepts and ignores (the models follow the configured engines) |
| `install-ocr --detector` | CLI-only because it is a hidden 0.5 flag that 0.7 accepts and ignores |
| `models` | [OCR & models](/app/settings/ocr) |
| `models list` | [OCR & models](/app/settings/ocr) → "List the models" |
| `models list --processor` | [OCR & models](/app/settings/ocr) → For: the processor |
| `models download` | [OCR & models](/app/settings/ocr) → Download; the OCR install ([OCR install](/app/setup/ocr), the setups) downloads them too |
| `models download --engine` | [OCR & models](/app/settings/ocr) → Engine |
| `models download --processor` | [OCR & models](/app/settings/ocr) → For: the processor |
| `models verify` | [OCR & models](/app/settings/ocr) → "Verify them" |
| `models verify --processor` | [OCR & models](/app/settings/ocr) → For: the processor |

## Processor

| Command | Where |
|---|---|
| `processor` | [Processor setup](/app/setup/processor) and [Processor settings](/app/settings/processor) |
| `processor serve` | [Processor setup](/app/setup/processor) starts it at the end; [Processor](/app/settings/processor) → "Start it". The tray starts and stops it |
| `processor serve --config` | The app uses this machine's processor.yaml (shown in [Processor](/app/settings/processor)); `MOKURO_PROCESSOR_CONFIG` points it elsewhere |
| `processor serve --verbose` | CLI-only because it only adds detail to terminal output. The processor's log is in [Logs](/app/settings/logs) |
| `processor setup` | [Processor setup](/app/setup/processor): library address, account, connection test, this machine, the OCR install, optionally start with the machine |
| `processor setup --config` | [Processor setup](/app/setup/processor) writes this machine's processor.yaml and shows its path |
| `processor setup --url` | [Processor setup](/app/setup/processor) → Library address |
| `processor setup --username` | [Processor setup](/app/setup/processor) → Username |
| `processor setup --password-stdin` | [Processor setup](/app/setup/processor) → Password |
| `processor setup --name` | [Processor setup](/app/setup/processor) → Name; [Processor](/app/settings/processor) → Name |
| `processor setup --backend` | [Processor setup](/app/setup/processor) → OCR install → Pack: the processor uses the devices of the pack installed for it |
| `processor setup --tls-verify` | [Processor setup](/app/setup/processor) → Certificate check; [Processor](/app/settings/processor) |
| `processor setup --auto-update` | [Processor setup](/app/setup/processor) → "Update this processor automatically when its library updates" (off by default); [Updates](/app/settings/update) and [Processor](/app/settings/processor) change it later (`processor.auto_update`) |
| `processor setup --yes` | CLI-only because it answers terminal prompts, and the app has no prompts |
| `processor setup --no-install` | CLI-only because the app's [Processor setup](/app/setup/processor) always installs OCR (a processor without it does nothing); [OCR install](/app/setup/ocr) replaces the pack later |
| `processor setup --no-service` | [Processor setup](/app/setup/processor) → "Start with the machine" is off by default; later in [Start with the machine](/app/setup/startup) |
| `processor setup --force` | [Processor setup](/app/setup/processor) says when a processor.yaml exists, and replaces it only when that box is ticked |
| `processor status` | [Processor](/app/settings/processor) shows the last status; the [dashboard](/app/dashboard) of a running processor shows it live |
| `processor status --config` | [Processor](/app/settings/processor) reads this machine's processor.yaml |
| `processor service` | [Processor setup](/app/setup/processor) → Start with the machine; [Start with the machine](/app/setup/startup) → For: the processor (shows the service file) |
| `processor service --config` | [Start with the machine](/app/setup/startup) uses this machine's processor.yaml |
| `processor service --install` | [Processor setup](/app/setup/processor) → Start with the machine → As a background service; later [Start with the machine](/app/setup/startup) → How: as a background service → Set it up (the other choice, the tray at login, writes `tray.json` and the tray's login item instead; it has no CLI twin) |

## The desktop app itself

| Command | Where |
|---|---|
| `gui` | [Home](/app/): this command serves the app and opens it. Double-clicking the Windows command line (`bin\mokuro-bunko.exe`) with no `Mokuro Bunko.exe` above it does too |
| `gui --no-browser` | CLI-only because it starts the pages without opening a browser, for the tray and remote shells; it prints the sign-in link |
| `gui --open` | [Home](/app/) is the default; any app page can be the start page (`--open /app/settings/update`) |
| `tray` | The setups' "Start with the machine" → From the tray at login, or [Start with the machine](/app/setup/startup) → "Run from the tray when I log in": it starts the tray and adds its login item. Opening the macOS app or `Mokuro Bunko.exe` on Windows runs it too |
| `tray --storage` | CLI-only because the tray finds the instances of this machine by itself (the default storages, the configs, the services); this adds another folder for a hand-made setup |
| `tray --no-supervise` | CLI-only because it is for watching instances something else runs; the app's choice between the tray and a service ([Start with the machine](/app/setup/startup)) decides what the tray starts |
| `tray --log-stderr` | CLI-only because it only moves the tray's log to the terminal; [Logs](/app/settings/logs) shows the tray's log file |
