# GUI coverage: every CLI command has a home

Every command, flag and argument of `mokuro-bunko` has a row here saying where the same
thing is done without a terminal:

- **the library server's own pages**, the same locally, remotely and in Docker: the
  first-run [setup](/setup) and the admin panel (`/_admin`, admins only), whose
  [This server](/_admin#server) tab covers the machine the server runs on (OCR backend,
  engines and models, diagnostics, the log);
- **the local processor pages** (`/app/...`, desktop only, on the loopback control
  port): the first-launch chooser, the processor's pairing, settings and status;
- **the tray** (`Tray → "…"`, desktop only);
- or **"CLI-only because …"**.

The table is checked by a test, `gui::coverage` in `crates/mokuro-bunko`. It walks the
clap tree and fails when:

- a command, flag or argument has no row;
- a row names no home (an `/app/`, `/_admin` or `/setup` link, a tray item, or "CLI-only
  because");
- a row links an `/app/` page or a settings section that does not exist, an admin panel
  tab (`/_admin#tab`) the panel does not have, or a tray item the menu does not have;
- a row names something the CLI no longer has (checked in the full build).

Hidden flags and subcommands (internal ones such as `update prefetch` and
`install-ocr --probe`, and the 0.5 spellings) are walked like any other, so each has a
row, saying "CLI-only because it is internal (hidden)".

When you add a flag, add its row. Rows use the CLI's spelling, such as
`install-ocr --from` or `admin add-user <USERNAME>`. Global flags have no command in
front (`--config`).

## Global flags

| Command | Where |
|---|---|
| `--config` | CLI-only because the server's pages work on the config it runs with (its path is in [This server](/_admin#server) → Log); `-c` picks another file for a command |
| `--verbose` | CLI-only because it only adds detail to terminal output. [This server](/_admin#server) → Log shows the server log |
| `--version` | [Status](/_admin#status) → Updates shows this version and the latest one |

## Library server

| Command | Where |
|---|---|
| `serve` | The chooser's [Library server](/app/) starts it (the tray runs it from then on); `Tray → "Quit"` stops it. Docker and services run it themselves |
| `serve --host` | CLI-only because where the server listens is fixed before it serves (`server.host`, `MOKURO_HOST`); the chooser keeps the default (the home network) |
| `serve --port` | CLI-only because where the server listens is fixed before it serves (`server.port`, `MOKURO_PORT`); the chooser picks a free port from 8080 |
| `serve --ocr` | [This server](/_admin#server) → OCR → Run it on (`ocr.backend`: auto, CPU, or the GPU found) |
| `serve --generations` | [Settings](/_admin#settings) → OCR → Generations |
| `setup` | The server's own [setup](/setup): admin account, who may join, remote access (tunnel, Dynamic DNS, HTTPS), OCR on this machine. From another computer it asks for the one-time code in the server log |
| `setup --skip-if-exists` | CLI-only because it exists for container entrypoints and scripts; [setup](/setup) only runs while no admin exists |
| `doctor` | [This server](/_admin#server) → Diagnostics → Run checks |
| `doctor --processor` | [Diagnostics](/app/settings/doctor) on the processor's own page |
| `healthcheck` | CLI-only because it is the container health probe; the admin panel answering is the same answer |
| `healthcheck --url` | CLI-only because it probes an arbitrary address for container health checks |

## Users and invites

| Command | Where |
|---|---|
| `admin` | [Users](/_admin#users) and [Invites](/_admin#invites) |
| `admin add-user` | [Users](/_admin#users) → Add user; the first admin is made by [setup](/setup) |
| `admin add-user <USERNAME>` | [Users](/_admin#users) → Add user → Username |
| `admin add-user --role` | [Users](/_admin#users) → Add user → Role |
| `admin add-user --password` | [Users](/_admin#users) → Add user → Password |
| `admin delete-user` | [Users](/_admin#users) → Delete |
| `admin delete-user <USERNAME>` | [Users](/_admin#users) → the user's row |
| `admin delete-user --yes` | [Users](/_admin#users) → Delete asks in a dialog instead |
| `admin list-users` | [Users](/_admin#users) |
| `admin list-users --status` | [Users](/_admin#users) shows each account's status (active, pending, disabled, deleted) |
| `admin change-role` | [Users](/_admin#users) → Change role |
| `admin change-role <USERNAME>` | [Users](/_admin#users) → the user's row |
| `admin change-role <ROLE>` | [Users](/_admin#users) → Change role → Role |
| `admin generate-invite` | [Invites](/_admin#invites) → Generate |
| `admin generate-invite --role` | [Invites](/_admin#invites) → Generate → Role |
| `admin generate-invite --expires` | [Invites](/_admin#invites) → Generate → Expires in |
| `admin list-invites` | [Invites](/_admin#invites) |
| `admin list-invites --all` | [Invites](/_admin#invites) lists used and expired codes with their status |
| `admin delete-invite` | [Invites](/_admin#invites) → Delete |
| `admin delete-invite <CODE>` | [Invites](/_admin#invites) → the code's row |
| `admin approve-user` | [Users](/_admin#users) → Approve (pending accounts) |
| `admin approve-user <USERNAME>` | [Users](/_admin#users) → the user's row |
| `admin disable-user` | [Users](/_admin#users) → Disable |
| `admin disable-user <USERNAME>` | [Users](/_admin#users) → the user's row |
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
| `config` | [Settings](/_admin#settings) and [This server](/_admin#server) change the keys a page needs |
| `config show` | CLI-only because it prints the whole file with `MOKURO_*` applied, for the terminal; the pages show each setting where it is changed |
| `config set` | CLI-only because it reaches every key, including the ones fixed before the server starts; [Settings](/_admin#settings) changes the common ones live |
| `config set <KEY>` | CLI-only because `config set` is |
| `config set <VALUE>` | CLI-only because `config set` is |
| `config path` | [This server](/_admin#server) → Log names the storage folder; CLI-only for the config file's path, which a page cannot open |
| `config init` | The chooser's [Library server](/app/) writes a new file on a desktop; [setup](/setup) fills it in |
| `config init --force` | CLI-only because resetting the file under a running server would undo its own settings; stop it first |
| `config cors-add` | [Settings](/_admin#settings) → CORS; also [setup](/setup) → Remote access → Other web apps |
| `config cors-add <ORIGIN>` | [Settings](/_admin#settings) → CORS → the origin |
| `config cors-remove` | [Settings](/_admin#settings) → CORS → Remove |
| `config cors-remove <ORIGIN>` | [Settings](/_admin#settings) → CORS → that origin's Remove |

## HTTPS, tunnel and Dynamic DNS

| Command | Where |
|---|---|
| `ssl` | [setup](/setup) → Remote access → HTTPS; later CLI-only because the certificate files live on the server and a change needs a restart |
| `ssl enable` | [setup](/setup) → Remote access → HTTPS (the server restarts with it) |
| `ssl enable --auto-cert` | [setup](/setup) → HTTPS → Self-signed certificate |
| `ssl enable --cert` | [setup](/setup) → HTTPS → My certificate files → Certificate file |
| `ssl enable --key` | [setup](/setup) → HTTPS → My certificate files → Private key file |
| `ssl disable` | CLI-only because HTTPS is fixed while the server runs; run it, then restart |
| `ssl status` | CLI-only because it reads the certificate files on the server for the terminal |
| `ssl generate` | CLI-only because it writes files on the server; [setup](/setup)'s self-signed choice makes one by itself |
| `ssl generate --hostname` | CLI-only because `ssl generate` is |
| `ssl generate --days` | CLI-only because `ssl generate` is |
| `tunnel` | [Connectivity](/_admin#connectivity) → Cloudflare Tunnel |
| `tunnel status` | [Connectivity](/_admin#connectivity) → Cloudflare Tunnel (says when cloudflared is missing) |
| `tunnel cloudflare` | [Connectivity](/_admin#connectivity) → Start Tunnel (the running server owns the tunnel) |
| `tunnel cloudflare --port` | [Connectivity](/_admin#connectivity): the server tunnels its own port |
| `dyndns` | [Connectivity](/_admin#connectivity) → Dynamic DNS |
| `dyndns setup` | [setup](/setup) → Remote access → Dynamic DNS (provider, domain, token); later [Connectivity](/_admin#connectivity) → Dynamic DNS → Save Settings |
| `dyndns status` | [Connectivity](/_admin#connectivity) → Dynamic DNS → Last Update |
| `dyndns update` | [Connectivity](/_admin#connectivity) → Dynamic DNS → Test Update |
| `dyndns enable` | [Connectivity](/_admin#connectivity) → Dynamic DNS → Start |
| `dyndns disable` | [Connectivity](/_admin#connectivity) → Dynamic DNS → Stop |

## Updates

| Command | Where |
|---|---|
| `update` | [Status](/_admin#status) → Updates for the server; [Updates](/app/settings/update) on a processor's page |
| `update check` | [Status](/_admin#status) → Updates → Check now; [Updates](/app/settings/update) → Check again |
| `update apply` | [Status](/_admin#status) → Updates → Update and restart; [Updates](/app/settings/update) → Install the update |
| `update apply --yes` | [Status](/_admin#status) → Updates: the button is the confirmation |
| `update apply --restart` | [Status](/_admin#status) → Updates → Update and restart |
| `update prefetch` | CLI-only because it is internal (hidden): an automatic update runs it from the downloaded release before installing it. [Status](/_admin#status) → Updates → "Install updates automatically" turns them on |
| `update prefetch --processor-config` | CLI-only because `update prefetch` is |
| `update prefetch --manifest-url` | CLI-only because `update prefetch` is |

## OCR backend and models

| Command | Where |
|---|---|
| `install-ocr` | [This server](/_admin#server) → OCR → Install (progress there; it runs in the background); a processor's [OCR install](/app/setup/ocr) |
| `install-ocr --variant` | [This server](/_admin#server) → OCR → Run it on (auto, CPU, the GPU found); [OCR install](/app/setup/ocr) → Pack |
| `install-ocr --if-needed` | CLI-only because `serve` and `processor serve` run it themselves in the background ([This server](/_admin#server) shows its progress and a Retry) |
| `install-ocr --from` | [OCR install](/app/setup/ocr) → More options → Install from a folder (a processor's local page only). A server installs from its signed release, or from the `ocr-offline` folder shipped next to the program |
| `install-ocr --dir` | [OCR install](/app/setup/ocr) → More options → Install packs into |
| `install-ocr --no-models` | [OCR install](/app/setup/ocr) → More options → Only the pack |
| `install-ocr --force` | [This server](/_admin#server) → OCR → Reinstall; [OCR install](/app/setup/ocr) → More options → Reinstall |
| `install-ocr --list` | [This server](/_admin#server) → OCR shows the hardware and the installed packs; [OCR & models](/app/settings/ocr) → Packs and hardware |
| `install-ocr --processor` | A processor's [OCR install](/app/setup/ocr) (its pages always mean the processor) |
| `install-ocr --probe` | CLI-only because it is internal (hidden): an automatic update loads a new backend pack in a child process to check it works before switching to it |
| `install-ocr --backend` | CLI-only because it is the hidden 0.5 spelling of `--variant`, kept for old scripts |
| `install-ocr --engines` | CLI-only because it is a hidden 0.5 flag that 0.7 accepts and ignores (the models follow the configured engines) |
| `install-ocr --detector` | CLI-only because it is a hidden 0.5 flag that 0.7 accepts and ignores |
| `models` | [This server](/_admin#server) → Engines and models; [OCR & models](/app/settings/ocr) for a processor |
| `models list` | [This server](/_admin#server) → Engines and models (and Details); [OCR & models](/app/settings/ocr) → List the models |
| `models list --processor` | [OCR & models](/app/settings/ocr) → List the models |
| `models download` | [This server](/_admin#server) → Engines and models → Download missing; [OCR & models](/app/settings/ocr) → Download |
| `models download --engine` | [OCR & models](/app/settings/ocr) → Engine; a server fetches what its enabled generations use |
| `models download --processor` | [OCR & models](/app/settings/ocr) → Download |
| `models verify` | [This server](/_admin#server) → Engines and models → Verify; [OCR & models](/app/settings/ocr) → Verify them |
| `models verify --processor` | [OCR & models](/app/settings/ocr) → Verify them |

## Processor

| Command | Where |
|---|---|
| `processor` | [Pair a processor](/app/setup/processor) and its [settings](/app/settings/processor) |
| `processor serve` | [Pair a processor](/app/setup/processor) starts it at the end (the tray runs it from then on); [Connection](/app/settings/processor) → Start it; `Tray → "Quit"` stops it |
| `processor serve --config` | The pages use this machine's processor.yaml (shown in [Connection](/app/settings/processor)); `MOKURO_PROCESSOR_CONFIG` points it elsewhere |
| `processor serve --verbose` | CLI-only because it only adds detail to terminal output. The processor's log is in [Logs](/app/settings/logs) |
| `processor setup` | [Pair a processor](/app/setup/processor): library address, account, connection test, this machine, the OCR choices |
| `processor setup --config` | [Pair a processor](/app/setup/processor) writes this machine's processor.yaml and shows its path |
| `processor setup --url` | [Pair a processor](/app/setup/processor) → Library address |
| `processor setup --username` | [Pair a processor](/app/setup/processor) → Processor username |
| `processor setup --password-stdin` | [Pair a processor](/app/setup/processor) → Password |
| `processor setup --name` | [Pair a processor](/app/setup/processor) → Name; [Connection](/app/settings/processor) → Name |
| `processor setup --backend` | [Pair a processor](/app/setup/processor) → OCR install → Backend pack |
| `processor setup --tls-verify` | [Pair a processor](/app/setup/processor) → Check the library's certificate; [Connection](/app/settings/processor) |
| `processor setup --auto-update` | [Pair a processor](/app/setup/processor) → "Update this processor automatically when its library updates" (off by default); [Updates](/app/settings/update) changes it later |
| `processor setup --yes` | CLI-only because it answers terminal prompts, and the pages have no prompts |
| `processor setup --no-install` | CLI-only because the [pairing](/app/setup/processor) always installs OCR (a processor without it does nothing) |
| `processor setup --no-service` | CLI-only because the pages never write a service: the tray runs the processor, and `Tray → "Start at login"` starts it at login |
| `processor setup --force` | [Pair a processor](/app/setup/processor) says when a processor.yaml exists, and replaces it only when that box is ticked |
| `processor status` | [Connection](/app/settings/processor) → Last status; its [status page](/app/dashboard) shows it live |
| `processor status --config` | [Connection](/app/settings/processor) reads this machine's processor.yaml |
| `processor service` | CLI-only because a background service is for headless machines, set up from their terminal; on a desktop the tray runs the processor and `Tray → "Start at login"` starts it |
| `processor service --config` | CLI-only because `processor service` is |
| `processor service --install` | CLI-only because `processor service` is |

## The desktop app itself

| Command | Where |
|---|---|
| `gui` | [Home](/app/): the first-launch chooser (library server or processor). The tray opens it when nothing is set up (`Tray → "Set up…"`) |
| `gui --no-browser` | CLI-only because it starts the pages without opening a browser, for remote shells; it prints the sign-in link |
| `gui --open` | [Home](/app/) is the default; any local page can be the start page (`--open /app/settings/update`) |
| `tray` | `Tray → "Start at login"` starts it at login; `mokuro-bunko` with no arguments on a desktop runs it (the Windows app, the macOS app) |
| `tray --storage` | CLI-only because the tray finds the instances of this machine by itself (the default storages, the configs, the services); this adds another folder for a hand-made setup |
| `tray --no-supervise` | CLI-only because it is for watching instances something else runs (a service); the chooser and the pairing hand what they start to the tray |
| `tray --log-stderr` | CLI-only because it only moves the tray's log to the terminal |
